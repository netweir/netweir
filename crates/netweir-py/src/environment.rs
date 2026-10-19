//! A page's environment from Python's keyword arguments, named as
//! Playwright names `new_context`'s: `viewport`, `locale`, `timezone_id`
//! and so on, and `device` for a preset.

use netweir_browser::{Environment, Geolocation, Size};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

/// Every key an environment takes.
pub(crate) const KEYS: &[&str] = &[
    "device",
    "viewport",
    "screen",
    "device_scale_factor",
    "is_mobile",
    "has_touch",
    "locale",
    "timezone_id",
    "geolocation",
    "permissions",
    "color_scheme",
    "reduced_motion",
    "forced_colors",
    "contrast",
    "extra_http_headers",
    "offline",
    "user_agent",
    "init_scripts",
];

/// An environment from keyword arguments (or a dict of them, as
/// `Settings.browser_environment` holds). A `device` preset comes first,
/// and the rest change it. Raises TypeError for a key it doesn't know,
/// and ValueError for settings that contradict each other.
pub(crate) fn environment_from(given: Option<&Bound<'_, PyDict>>) -> PyResult<Environment> {
    let Some(given) = given else {
        return Ok(Environment::default());
    };
    for key in given.keys() {
        let key: String = key.extract()?;
        if !KEYS.contains(&key.as_str()) {
            return Err(PyTypeError::new_err(format!(
                "unknown environment setting {key:?}; there are {}",
                KEYS.join(", ")
            )));
        }
    }
    let get = |key: &str| -> PyResult<Option<Bound<'_, PyAny>>> {
        Ok(given.get_item(key)?.filter(|v| !v.is_none()))
    };
    let mut env = match get("device")? {
        Some(name) => {
            Environment::device(&name.extract::<String>()?).map_err(crate::browser::raise)?
        }
        None => Environment::default(),
    };
    match given.get_item("viewport")? {
        // Given as None, as Playwright's no_viewport: the window as
        // Chrome opens it.
        Some(v) if v.is_none() => {
            env.viewport = None;
            env.no_viewport = true;
        }
        Some(v) => env.viewport = Some(size_from("viewport", &v)?),
        None => {}
    }
    if let Some(v) = get("screen")? {
        env.screen = Some(size_from("screen", &v)?);
    }
    if let Some(v) = get("device_scale_factor")? {
        env.device_scale_factor = Some(v.extract()?);
    }
    if let Some(v) = get("is_mobile")? {
        env.mobile = Some(v.extract()?);
    }
    if let Some(v) = get("has_touch")? {
        env.has_touch = Some(v.extract()?);
    }
    if let Some(v) = get("locale")? {
        env.locale = Some(v.extract()?);
    }
    if let Some(v) = get("timezone_id")? {
        env.timezone = Some(v.extract()?);
    }
    if let Some(v) = get("geolocation")? {
        env.geolocation = Some(geolocation_from(&v)?);
    }
    if let Some(v) = get("permissions")? {
        if v.is_instance_of::<pyo3::types::PyString>() {
            return Err(PyTypeError::new_err(
                "permissions is a list of names, such as [\"geolocation\"]",
            ));
        }
        env.permissions = v.extract()?;
    }
    if let Some(v) = get("color_scheme")? {
        env.color_scheme = Some(
            v.extract::<String>()?
                .parse()
                .map_err(crate::browser::raise)?,
        );
    }
    if let Some(v) = get("reduced_motion")? {
        env.reduced_motion = Some(
            v.extract::<String>()?
                .parse()
                .map_err(crate::browser::raise)?,
        );
    }
    if let Some(v) = get("forced_colors")? {
        env.forced_colors = Some(
            v.extract::<String>()?
                .parse()
                .map_err(crate::browser::raise)?,
        );
    }
    if let Some(v) = get("contrast")? {
        env.contrast = Some(
            v.extract::<String>()?
                .parse()
                .map_err(crate::browser::raise)?,
        );
    }
    if let Some(v) = get("extra_http_headers")? {
        env.extra_headers = headers_from(&v)?;
    }
    if let Some(v) = get("offline")? {
        env.offline = Some(v.extract()?);
    }
    if let Some(v) = get("user_agent")? {
        env.user_agent = Some(v.extract()?);
    }
    if let Some(v) = get("init_scripts")? {
        if v.is_instance_of::<pyo3::types::PyString>() {
            return Err(PyTypeError::new_err(
                "init_scripts is a list of scripts, each a str",
            ));
        }
        env.init_scripts = v.extract()?;
    }
    env.validate().map_err(crate::browser::raise)?;
    Ok(env)
}

/// One of emulate_media's settings: None (left out) leaves it, "null" (or
/// "no-override") takes its emulation away, and anything else is parsed.
pub(crate) fn media_setting<T: std::str::FromStr<Err = netweir_browser::Error>>(
    given: Option<String>,
) -> PyResult<Option<Option<T>>> {
    match given.as_deref() {
        None => Ok(None),
        Some("null" | "no-override") => Ok(Some(None)),
        Some(v) => Ok(Some(Some(v.parse().map_err(crate::browser::raise)?))),
    }
}

/// `{"width": 1280, "height": 720}`, as Playwright gives a size.
pub(crate) fn size_from(what: &str, v: &Bound<'_, PyAny>) -> PyResult<Size> {
    let bad = || PyTypeError::new_err(format!("{what} is a dict with \"width\" and \"height\""));
    let d = v.cast::<PyDict>().map_err(|_| bad())?;
    let side = |k: &str| -> PyResult<u32> {
        let n: i64 = d.get_item(k)?.ok_or_else(bad)?.extract()?;
        u32::try_from(n)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| PyValueError::new_err(format!("{what}'s {k} must be a positive number")))
    };
    Ok(Size {
        width: side("width")?,
        height: side("height")?,
    })
}

/// `{"latitude": .., "longitude": .., "accuracy": ..}`; accuracy is 0 if
/// left out, as Playwright has it.
pub(crate) fn geolocation_from(v: &Bound<'_, PyAny>) -> PyResult<Geolocation> {
    let bad = || {
        PyTypeError::new_err(
            "geolocation is a dict with \"latitude\" and \"longitude\", and \"accuracy\" if you like",
        )
    };
    let d = v.cast::<PyDict>().map_err(|_| bad())?;
    let number = |k: &str| -> PyResult<Option<f64>> {
        d.get_item(k)?
            .filter(|v| !v.is_none())
            .map(|v| v.extract())
            .transpose()
    };
    Ok(Geolocation {
        latitude: number("latitude")?.ok_or_else(bad)?,
        longitude: number("longitude")?.ok_or_else(bad)?,
        accuracy: number("accuracy")?.unwrap_or(0.0),
    })
}

/// A dict of header names and values, in its order.
pub(crate) fn headers_from(v: &Bound<'_, PyAny>) -> PyResult<Vec<(String, String)>> {
    let d = v
        .cast::<PyDict>()
        .map_err(|_| PyTypeError::new_err("headers are a dict of names and values"))?;
    d.iter()
        .map(|(k, v)| Ok((k.extract()?, v.extract()?)))
        .collect()
}

/// Checks an environment's settings, raising as a page given them would.
#[pyfunction]
pub(crate) fn _check_environment(given: &Bound<'_, PyDict>) -> PyResult<()> {
    environment_from(Some(given)).map(drop)
}

/// Every device preset, by name, as the keyword arguments it stands for.
#[pyfunction]
pub(crate) fn _devices(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let all = PyDict::new(py);
    for name in netweir_browser::device_names() {
        let env = Environment::device(name).map_err(crate::browser::raise)?;
        let d = PyDict::new(py);
        d.set_item("device", name)?;
        if let Some(s) = env.screen {
            d.set_item("screen", size_to(py, s)?)?;
        }
        if let Some(v) = env.viewport {
            d.set_item("viewport", size_to(py, v)?)?;
        }
        d.set_item("device_scale_factor", env.device_scale_factor)?;
        d.set_item("is_mobile", env.mobile)?;
        d.set_item("has_touch", env.has_touch)?;
        all.set_item(name, d)?;
    }
    Ok(all)
}

pub(crate) fn size_to(py: Python<'_>, s: Size) -> PyResult<Bound<'_, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("width", s.width)?;
    d.set_item("height", s.height)?;
    Ok(d)
}
