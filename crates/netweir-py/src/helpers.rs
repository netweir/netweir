//! Prices, dates and the data in scripts. The reading is in netweir-dom;
//! the Python types (Price, datetime) are made in netweir/_helpers.py.

use netweir_dom::auto::fields;
use netweir_dom::date::{self, Civil, Zone};
use netweir_dom::js::{self, JsValue};
use netweir_dom::price;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyInt, PyList};

/// The first price in `text` as (amount digits, currency as written).
#[pyfunction]
fn _parse_price(text: &str) -> Option<(String, Option<String>)> {
    price::parse_price(text).map(|p| (p.amount, p.currency))
}

/// The ISO 4217 code for a currency as written, if it has only one.
#[pyfunction]
fn _currency_code(currency: &str) -> Option<&'static str> {
    price::currency_code(currency)
}

/// The product code in `text` as {type, value}, or None.
#[pyfunction]
fn parse_gtin(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    let found = netweir_dom::gtin::parse(text)
        .map(|g| serde_json::json!({"type": g.kind, "value": g.value}));
    crate::browser::to_python(py, &found.unwrap_or_default())
}

/// The rating in `text` as {rating_value, best_rating, review_count}, or
/// None.
#[pyfunction]
fn parse_rating(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    let found = fields::rating(&serde_json::Value::String(text.to_string()));
    crate::browser::to_python(py, &found.unwrap_or_default())
}

/// "InStock" or "OutOfStock", or None when `text` doesn't say.
#[pyfunction]
fn parse_availability(text: &str) -> Option<String> {
    fields::availability(&serde_json::Value::String(text.to_string()))
}

/// The salary in `text` as {raw, value_min, value_max, rate_type,
/// currency, currency_raw}, or None.
#[pyfunction]
fn parse_salary(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    let found = fields::salary(&serde_json::Value::String(text.to_string()));
    crate::browser::to_python(py, &found.unwrap_or_default())
}

type Fields = (i32, u32, u32, u32, u32, u32, u32);

/// The date in `text` as (fields, offset in seconds or None, whether it
/// was counted from `now` and so takes `now`'s zone, seconds of elapsed
/// time still to add in UTC).
#[pyfunction]
fn _parse_date(
    text: &str,
    day_first: Option<bool>,
    now: Fields,
) -> Option<(Fields, Option<i32>, bool, i64)> {
    let (year, month, day, hour, minute, second, micro) = now;
    let now = Civil {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micro,
    };
    let p = date::parse_date(text, day_first, now)?;
    let c = p.civil;
    let fields = (c.year, c.month, c.day, c.hour, c.minute, c.second, c.micro);
    let (offset, from_now) = match p.zone {
        Zone::Naive => (None, false),
        Zone::Offset(o) => (Some(o), false),
        Zone::AsNow => (None, true),
    };
    Some((fields, offset, from_now, p.elapsed))
}

/// The first JavaScript object or array literal in `text`, as Python
/// values, or None. With all=True, a list of every one, in order.
#[pyfunction]
#[pyo3(signature = (text, all=false))]
fn js_data(py: Python<'_>, text: &str, all: bool) -> PyResult<Py<PyAny>> {
    if all {
        let found = py.detach(|| js::all(text));
        list(py, &found)
    } else {
        match py.detach(|| js::first(text)) {
            Some(value) => to_python(py, &value),
            None => Ok(py.None()),
        }
    }
}

pub(crate) fn list(py: Python<'_>, values: &[JsValue]) -> PyResult<Py<PyAny>> {
    let out = PyList::empty(py);
    for v in values {
        out.append(to_python(py, v)?)?;
    }
    Ok(out.into_any().unbind())
}

/// A script value as Python. Nesting is capped where it's read, so this
/// can recurse.
pub(crate) fn to_python(py: Python<'_>, value: &JsValue) -> PyResult<Py<PyAny>> {
    Ok(match value {
        JsValue::Null => py.None(),
        JsValue::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        JsValue::Int(i) => i.into_pyobject(py)?.into_any().unbind(),
        JsValue::BigInt(digits) => py.get_type::<PyInt>().call1((digits,))?.unbind(),
        JsValue::Float(f) => f.into_pyobject(py)?.into_any().unbind(),
        JsValue::Str(s) => s.into_pyobject(py)?.into_any().unbind(),
        JsValue::Array(items) => list(py, items)?,
        JsValue::Object(members) => {
            let d = PyDict::new(py);
            for (k, v) in members {
                d.set_item(k, to_python(py, v)?)?;
            }
            d.into_any().unbind()
        }
    })
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_parse_price, m)?)?;
    m.add_function(wrap_pyfunction!(_currency_code, m)?)?;
    m.add_function(wrap_pyfunction!(_parse_date, m)?)?;
    m.add_function(wrap_pyfunction!(parse_gtin, m)?)?;
    m.add_function(wrap_pyfunction!(parse_rating, m)?)?;
    m.add_function(wrap_pyfunction!(parse_availability, m)?)?;
    m.add_function(wrap_pyfunction!(parse_salary, m)?)?;
    m.add_function(wrap_pyfunction!(js_data, m)?)
}
