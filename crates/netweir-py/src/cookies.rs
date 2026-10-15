//! Cookies and storage states between Python and Rust: one dict shape
//! for the HTTP client, a crawl's requests and Chrome alike.

use std::path::PathBuf;

use netweir_browser::{Cookie, StorageState};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

/// Cookies as dicts: name, value, domain (a leading dot for the domain
/// and its subdomains), path, expires (-1 for a session cookie),
/// http_only, secure, same_site and partition_key.
pub(crate) fn cookie_list(py: Python<'_>, cookies: &[Cookie]) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for c in cookies {
        let d = PyDict::new(py);
        d.set_item("name", &c.name)?;
        d.set_item("value", &c.value)?;
        d.set_item("domain", &c.domain)?;
        d.set_item("path", &c.path)?;
        d.set_item("expires", c.expires)?;
        d.set_item("http_only", c.http_only)?;
        d.set_item("secure", c.secure)?;
        d.set_item("same_site", &c.same_site)?;
        d.set_item("partition_key", &c.partition_key)?;
        list.append(d)?;
    }
    Ok(list.into_any().unbind())
}

/// A cookie from a dict in Netweir's spelling or Playwright's
/// (`httpOnly`, `sameSite`, `partitionKey`). With `defaults`, domain and
/// path may be left out, for the URL it goes with to fill in.
pub(crate) fn cookie_from(d: &Bound<'_, PyAny>, defaults: bool) -> PyResult<Cookie> {
    let d = d
        .cast::<PyDict>()
        .map_err(|_| PyTypeError::new_err("each cookie must be a dict"))?;
    let text = |keys: &[&str]| -> PyResult<Option<String>> {
        for key in keys {
            if let Some(v) = d.get_item(key)?.filter(|v| !v.is_none()) {
                return Ok(Some(v.str()?.to_string()));
            }
        }
        Ok(None)
    };
    let flag = |keys: &[&str]| -> PyResult<bool> {
        for key in keys {
            if let Some(v) = d.get_item(key)? {
                return v.is_truthy();
            }
        }
        Ok(false)
    };
    let required = |key: &str| -> PyResult<String> {
        text(&[key])?.ok_or_else(|| PyValueError::new_err(format!("a cookie needs a {key:?}")))
    };
    let partition_key = match d.get_item("partitionKey")? {
        // Playwright's, or Chrome's own object.
        Some(v) if v.is_instance_of::<PyDict>() => v
            .get_item("topLevelSite")
            .ok()
            .filter(|s| !s.is_none())
            .map(|s| s.str().map(|s| s.to_string()))
            .transpose()?,
        _ => text(&["partition_key", "partitionKey"])?,
    };
    Ok(Cookie {
        name: required("name")?,
        value: required("value")?,
        domain: if defaults {
            text(&["domain"])?.unwrap_or_default()
        } else {
            required("domain")?
        },
        path: text(&["path"])?.unwrap_or_else(|| if defaults { "" } else { "/" }.into()),
        expires: d
            .get_item("expires")?
            .filter(|v| !v.is_none())
            .map(|v| v.extract())
            .transpose()?
            .unwrap_or(-1.0),
        http_only: flag(&["http_only", "httpOnly"])?,
        secure: flag(&["secure"])?,
        same_site: text(&["same_site", "sameSite"])?,
        partition_key,
    })
}

/// A storage state from a path to its JSON file or from a dict.
pub(crate) fn storage_state_from(source: &Bound<'_, PyAny>) -> PyResult<StorageState> {
    let invalid = |e: netweir_browser::Error| PyValueError::new_err(e.to_string());
    if source.is_instance_of::<PyDict>() {
        let text: String = source
            .py()
            .import("json")?
            .call_method1("dumps", (source,))?
            .extract()?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| PyValueError::new_err(e.to_string()))?;
        return StorageState::from_json(&value).map_err(invalid);
    }
    let path: PathBuf = source.extract().map_err(|_| {
        PyTypeError::new_err("a storage state is a path to its JSON file, or a dict")
    })?;
    StorageState::read(&path).map_err(invalid)
}

/// `state` as a dict, after writing it to `path` if there is one, readable
/// by its owner alone.
pub(crate) fn storage_state_out(
    py: Python<'_>,
    state: &StorageState,
    path: Option<PathBuf>,
) -> PyResult<Py<PyAny>> {
    if let Some(path) = path {
        state
            .write(&path)
            .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
    }
    crate::browser::to_python(py, &state.to_json())
}
