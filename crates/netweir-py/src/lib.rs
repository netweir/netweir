//! Python bindings. Kept thin: anything that could live in netweir-dom or
//! netweir-core does.

mod aio;
mod browser;
mod cookies;
mod crawl;
mod export;
mod fetch;
mod filter;
mod helpers;
mod node;
mod rules;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use netweir_dom::{Document, ParseXmlError, XmlNodes};
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
create_exception!(
    netweir,
    XMLError,
    PyValueError,
    "The document isn't well-formed XML. `line` and `column` (both from 1) say where, and `reason` what's wrong."
);

/// An XMLError, with where and why as attributes.
pub(crate) fn xml_error(py: Python<'_>, e: &netweir_dom::XmlError) -> PyErr {
    let err = XMLError::new_err(e.to_string());
    let value = err.value(py);
    let set = || -> PyResult<()> {
        value.setattr("line", e.line)?;
        value.setattr("column", e.column)?;
        value.setattr("reason", &e.reason)
    };
    match set() {
        Ok(()) => err,
        Err(other) => other,
    }
}

/// How to parse a body: as HTML or as XML.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Markup {
    Html,
    Xml,
}

impl Markup {
    /// `type=` as parsel takes it: "html", "xml", or None for HTML.
    pub(crate) fn named(name: Option<&str>) -> PyResult<Option<Markup>> {
        match name {
            None => Ok(None),
            Some("html") => Ok(Some(Markup::Html)),
            Some("xml") => Ok(Some(Markup::Xml)),
            Some(other) => Err(PyValueError::new_err(format!(
                "type must be \"html\" or \"xml\", not {other:?}"
            ))),
        }
    }

    /// What a response's Content-Type says it is. XML for the types
    /// Scrapy gives an XmlResponse (text/xml, application/xml, and RSS,
    /// Atom and RDF), and for any other `+xml` type (SOAP's, SVG) but
    /// XHTML's, which is a web page and is parsed as one.
    pub(crate) fn of(content_type: Option<&str>) -> Markup {
        let Some(ct) = content_type else {
            return Markup::Html;
        };
        let mime = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let xhtml = mime.ends_with("xhtml+xml");
        if !xhtml && (mime == "text/xml" || mime == "application/xml" || mime.ends_with("+xml")) {
            Markup::Xml
        } else {
            Markup::Html
        }
    }
}

/// Why parsing failed, before it's turned into a Python exception (which
/// needs the GIL back).
pub(crate) enum ParseFailure {
    Timeout(String),
    Xml(netweir_dom::XmlError),
}

impl ParseFailure {
    pub(crate) fn into_py(self, py: Python<'_>) -> PyErr {
        match self {
            ParseFailure::Timeout(why) => ParseTimeout::new_err(why),
            ParseFailure::Xml(e) => xml_error(py, &e),
        }
    }

    /// A message for the log, where there is no exception to raise.
    pub(crate) fn message(&self) -> String {
        match self {
            ParseFailure::Timeout(why) => why.clone(),
            ParseFailure::Xml(e) => e.to_string(),
        }
    }
}

/// Parses `text` as `markup`, giving up after `budget`. Call without the
/// GIL.
pub(crate) fn parse_markup(
    text: &str,
    markup: Markup,
    budget: Duration,
) -> Result<Document, ParseFailure> {
    match markup {
        Markup::Html => {
            Document::parse_within(text, budget).map_err(|e| ParseFailure::Timeout(e.to_string()))
        }
        Markup::Xml => Document::parse_xml_within(text, budget).map_err(|e| match e {
            ParseXmlError::Malformed(e) => ParseFailure::Xml(e),
            ParseXmlError::Timeout(t) => ParseFailure::Timeout(t.to_string()),
        }),
    }
}

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

/// Parses an HTML document, or with `type="xml"` an XML one, and returns
/// its root node.
///
/// With `timeout` (seconds), gives up and raises ParseTimeout on pages that
/// take longer, such as deeply nested hostile markup. XML that isn't
/// well-formed raises XMLError.
#[pyfunction]
#[pyo3(signature = (html, timeout=None, r#type=None))]
fn parse(py: Python<'_>, html: &str, timeout: Option<f64>, r#type: Option<&str>) -> PyResult<Node> {
    let budget = budget(timeout)?;
    let markup = Markup::named(r#type)?.unwrap_or(Markup::Html);
    let doc = py
        .detach(|| parse_markup(html, markup, budget))
        .map_err(|e| e.into_py(py))?;
    Ok(Node::root_of(Arc::new(doc)))
}

/// The elements of an XML document named `tag`, each read alone, as the
/// document is read (XMLFeedSpider's `iternodes`).
#[pyclass(frozen, module = "netweir")]
struct XmlNodeIter {
    nodes: Mutex<XmlNodes>,
}

#[pymethods]
impl XmlNodeIter {
    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Node>> {
        let next = py.detach(|| self.nodes.lock().unwrap_or_else(|e| e.into_inner()).next());
        match next {
            None => Ok(None),
            Some(Err(e)) => Err(xml_error(py, &e)),
            Some(Ok(doc)) => Ok(Some(Node::top_element_of(Arc::new(doc)))),
        }
    }
}

/// `tag` is `name` or `prefix:name` with the prefix in `namespaces`.
#[pyfunction]
fn _xml_nodes(text: String, tag: &str, namespaces: Vec<(String, String)>) -> PyResult<XmlNodeIter> {
    let nodes = XmlNodes::new(text, tag, &namespaces).map_err(PyValueError::new_err)?;
    Ok(XmlNodeIter {
        nodes: Mutex::new(nodes),
    })
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

/// What `netweir version -v` prints that only the build knows: the Rust
/// compiler, lexbor's version and the built-in profiles.
#[pyfunction]
fn _build_info(py: Python<'_>) -> PyResult<Bound<'_, pyo3::types::PyDict>> {
    let info = pyo3::types::PyDict::new(py);
    info.set_item("rustc", env!("NETWEIR_RUSTC"))?;
    info.set_item("lexbor", netweir_dom::LEXBOR_VERSION)?;
    info.set_item("profiles", netweir_core::built_in_profiles())?;
    Ok(info)
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    // Built without PyO3's reference pool, as threads need
    // (docs/design/threads.md); tests check every wheel was.
    m.add(
        "_reference_pool_disabled",
        cfg!(pyo3_disable_reference_pool),
    )?;
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
    m.add_function(wrap_pyfunction!(_build_info, m)?)?;
    m.add_function(wrap_pyfunction!(fingerprint, m)?)?;
    m.add_function(wrap_pyfunction!(_sitemap, m)?)?;
    m.add_function(wrap_pyfunction!(_xml_nodes, m)?)?;
    m.add_class::<XmlNodeIter>()?;
    m.add_function(wrap_pyfunction!(_robots_sitemaps, m)?)?;
    m.add_function(wrap_pyfunction!(_offsite, m)?)?;
    m.add_class::<Node>()?;
    m.add_class::<Selection>()?;
    m.add("SelectorError", m.py().get_type::<SelectorError>())?;
    m.add("XPathError", m.py().get_type::<XPathError>())?;
    m.add("ParseTimeout", m.py().get_type::<ParseTimeout>())?;
    m.add("XMLError", m.py().get_type::<XMLError>())?;
    browser::register(m)?;
    fetch::register(m)?;
    crawl::register(m)?;
    export::register(m)?;
    rules::register(m)?;
    helpers::register(m)?;
    Ok(())
}
