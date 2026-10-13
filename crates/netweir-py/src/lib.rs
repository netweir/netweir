//! Python bindings. Kept thin: anything that could live in netweir-dom or
//! netweir-core does.

mod aio;
mod browser;
mod crawl;
mod export;
mod fetch;
mod filter;
mod helpers;
mod node;
mod rules;

use std::sync::Arc;
use std::time::Duration;

use netweir_dom::Document;
use pyo3::create_exception;
use pyo3::exceptions::{PyTimeoutError, PyValueError};
use pyo3::prelude::*;

pub(crate) use node::{Node, Selection};

create_exception!(
    netweir,
    SelectorError,
    PyValueError,
    "The CSS selector could not be parsed."
);
create_exception!(
    netweir,
    XPathError,
    PyValueError,
    "The XPath expression could not be parsed or evaluated."
);
create_exception!(
    netweir,
    ParseTimeout,
    PyTimeoutError,
    "Parsing took longer than the timeout."
);

/// Seconds from Python to a parse budget; `None` means no limit.
pub(crate) fn budget(timeout: Option<f64>) -> PyResult<Duration> {
    match timeout {
        None => Ok(Duration::MAX),
        Some(t) if t >= 0.0 && t.is_finite() => {
            Ok(Duration::try_from_secs_f64(t).unwrap_or(Duration::MAX))
        }
        Some(_) => Err(PyValueError::new_err(
            "timeout must be a non-negative number of seconds",
        )),
    }
}

/// Parses an HTML document and returns its root node.
///
/// With `timeout` (seconds), gives up and raises ParseTimeout on pages that
/// take longer, such as deeply nested hostile markup.
#[pyfunction]
#[pyo3(signature = (html, timeout=None))]
fn parse(py: Python<'_>, html: &str, timeout: Option<f64>) -> PyResult<Node> {
    let budget = budget(timeout)?;
    let doc = py
        .detach(|| Document::parse_within(html, budget))
        .map_err(|e| ParseTimeout::new_err(e.to_string()))?;
    Ok(Node::root_of(Arc::new(doc)))
}

/// The crawl's identity for a GET of `url`, as hex: equal for the same
/// page however its URL is written. None for anything but http(s).
#[pyfunction]
fn fingerprint(url: &str) -> Option<String> {
    netweir_core::canonical::fingerprint("GET", url)
        .map(|fp| fp.iter().map(|b| format!("{b:02x}")).collect())
}

/// A sitemap's kind ("urlset" or "index") and its entries, as dicts with
/// loc, lastmod, changefreq, priority and alternates. Gzip is unzipped, to
/// at most `max` bytes. ValueError if it isn't a sitemap.
#[pyfunction]
fn _sitemap<'py>(
    py: Python<'py>,
    body: &[u8],
    max: usize,
) -> PyResult<(&'static str, Vec<Bound<'py, pyo3::types::PyDict>>)> {
    let map = py
        .detach(|| netweir_core::sitemap::parse(body, max))
        .map_err(pyo3::exceptions::PyValueError::new_err)?;
    let kind = match map.kind {
        netweir_core::sitemap::Kind::Urlset => "urlset",
        netweir_core::sitemap::Kind::Index => "index",
    };
    let mut entries = Vec::with_capacity(map.entries.len());
    for e in map.entries {
        let d = pyo3::types::PyDict::new(py);
        d.set_item("loc", e.loc)?;
        d.set_item("lastmod", e.lastmod)?;
        d.set_item("changefreq", e.changefreq)?;
        d.set_item("priority", e.priority)?;
        d.set_item("alternates", e.alternates)?;
        entries.push(d);
    }
    Ok((kind, entries))
}

/// Whether `url` is outside `allowed`, as `allowed_domains` judges it.
#[pyfunction]
fn _offsite(url: &str, allowed: Vec<String>) -> PyResult<bool> {
    let allowed =
        netweir_core::allowed_domains(&allowed).map_err(pyo3::exceptions::PyValueError::new_err)?;
    let url = url::Url::parse(url)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("{url:?}: {e}")))?;
    Ok(netweir_core::offsite(&url, &allowed))
}

/// The sitemap URLs a robots.txt names, as written.
#[pyfunction]
fn _robots_sitemaps(body: &str) -> Vec<String> {
    netweir_core::robots::Robots::parse(body)
        .sitemaps()
        .to_vec()
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    // Before Python shuts down, Rust threads still in it get to finish;
    // a forked child has none of its parent's (aio.rs).
    let wait = wrap_pyfunction!(aio::_wait_for_rust_threads, m)?;
    m.add_function(wait.clone())?;
    m.py().import("atexit")?.call_method1("register", (wait,))?;
    let forked = wrap_pyfunction!(aio::_forked, m)?;
    m.add_function(forked.clone())?;
    let os = m.py().import("os")?;
    if os.hasattr("register_at_fork")? {
        let kwargs = pyo3::types::PyDict::new(m.py());
        kwargs.set_item("after_in_child", forked)?;
        os.call_method("register_at_fork", (), Some(&kwargs))?;
    }
    // Shipped on purpose, for tests/test_exit.py to drive the bridge.
    m.add_function(wrap_pyfunction!(aio::_test_sleep, m)?)?;
    m.add_function(wrap_pyfunction!(aio::_test_panic, m)?)?;
    m.add_function(wrap_pyfunction!(parse, m)?)?;
    m.add_function(wrap_pyfunction!(fingerprint, m)?)?;
    m.add_function(wrap_pyfunction!(_sitemap, m)?)?;
    m.add_function(wrap_pyfunction!(_robots_sitemaps, m)?)?;
    m.add_function(wrap_pyfunction!(_offsite, m)?)?;
    m.add_class::<Node>()?;
    m.add_class::<Selection>()?;
    m.add("SelectorError", m.py().get_type::<SelectorError>())?;
    m.add("XPathError", m.py().get_type::<XPathError>())?;
    m.add("ParseTimeout", m.py().get_type::<ParseTimeout>())?;
    browser::register(m)?;
    fetch::register(m)?;
    crawl::register(m)?;
    export::register(m)?;
    rules::register(m)?;
    helpers::register(m)?;
    Ok(())
}
