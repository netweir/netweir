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

use netweir_dom::xml::{Repairs, XmlOptions};
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

    /// What a response is, as Scrapy's response types decide it: by its
    /// Content-Type, or when there's none (or it's only
    /// `application/octet-stream`) by the start of its text. XML for the
    /// types Scrapy gives an XmlResponse (text/xml, application/xml, and
    /// RSS, Atom and RDF), and for any other `+xml` type (SOAP's, SVG) but
    /// XHTML's, which is a web page and is parsed as one.
    pub(crate) fn of(content_type: Option<&str>, text: &str) -> Markup {
        let mime = content_type
            .and_then(|ct| ct.split(';').next())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if mime.is_empty() || mime == "application/octet-stream" || mime == "binary/octet-stream" {
            return Markup::sniff(text);
        }
        let xhtml = mime.ends_with("xhtml+xml");
        if !xhtml && (mime == "text/xml" || mime == "application/xml" || mime.ends_with("+xml")) {
            Markup::Xml
        } else {
            Markup::Html
        }
    }

    /// A body with nothing to say what it is: HTML if `<html` comes early,
    /// XML if an XML declaration does or the first element isn't one HTML
    /// has (`<rss>`, `<urlset>`), HTML otherwise.
    fn sniff(text: &str) -> Markup {
        let mut head = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
        let mut cut = head.len().min(5000);
        while !head.is_char_boundary(cut) {
            cut -= 1;
        }
        let lower = head[..cut].to_ascii_lowercase();
        if lower.contains("<html") || lower.contains("<!doctype html") {
            return Markup::Html;
        }
        if lower.starts_with("<?xml") {
            return Markup::Xml;
        }
        // Past comments, processing instructions and a doctype, to the
        // first element.
        while let Some(rest) = head.strip_prefix('<') {
            if rest.starts_with('!') || rest.starts_with('?') {
                match rest.find('>') {
                    Some(end) => {
                        head = rest[end + 1..].trim_start();
                        continue;
                    }
                    None => break,
                }
            }
            let name: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && !matches!(c, '>' | '/'))
                .collect();
            if !name.is_empty() && (name.contains(':') || !netweir_dom::xml::is_html_tag(&name)) {
                return Markup::Xml;
            }
            break;
        }
        Markup::Html
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

/// Parses `text` as `markup`, giving up after `budget`, recovering from
/// broken XML if `recover`. Call without the GIL.
pub(crate) fn parse_markup(
    text: &str,
    markup: Markup,
    budget: Duration,
    recover: bool,
) -> Result<(Document, Repairs), ParseFailure> {
    match markup {
        Markup::Html => Document::parse_within(text, budget)
            .map(|doc| (doc, Repairs::default()))
            .map_err(|e| ParseFailure::Timeout(e.to_string())),
        Markup::Xml => {
            Document::parse_xml_with(text, XmlOptions { recover, budget }).map_err(|e| match e {
                ParseXmlError::Malformed(e) => ParseFailure::Xml(e),
                ParseXmlError::Timeout(t) => ParseFailure::Timeout(t.to_string()),
            })
        }
    }
}

/// Logs what recovering from broken XML repaired, once, on the "netweir"
/// logger, naming `place` (a URL) if there is one.
pub(crate) fn warn_repairs(py: Python<'_>, place: Option<&str>, repairs: &Repairs) -> PyResult<()> {
    let Some(message) = repair_message(place, repairs) else {
        return Ok(());
    };
    py.import("logging")?
        .call_method1("getLogger", ("netweir",))?
        .call_method1("warning", ("%s", message))?;
    Ok(())
}

/// What warn_repairs logs; None when nothing was repaired.
pub(crate) fn repair_message(place: Option<&str>, repairs: &Repairs) -> Option<String> {
    if repairs.is_empty() {
        return None;
    }
    Some(match place {
        Some(place) => format!("{place}: {}", repairs.summary()),
        None => repairs.summary(),
    })
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
/// well-formed raises XMLError, unless `recover` is true: then what can be
/// repaired is, and a warning says what.
#[pyfunction]
#[pyo3(signature = (html, timeout=None, r#type=None, recover=false))]
fn parse(
    py: Python<'_>,
    html: &str,
    timeout: Option<f64>,
    r#type: Option<&str>,
    recover: bool,
) -> PyResult<Node> {
    let budget = budget(timeout)?;
    let markup = Markup::named(r#type)?.unwrap_or(Markup::Html);
    let (doc, repairs) = py
        .detach(|| parse_markup(html, markup, budget, recover))
        .map_err(|e| e.into_py(py))?;
    warn_repairs(py, None, &repairs)?;
    Ok(Node::root_of(Arc::new(doc)))
}

/// The elements of an XML document named `tag`, each read alone, as the
/// document is read (XMLFeedSpider's `iternodes`).
#[pyclass(frozen, module = "netweir")]
struct XmlNodeIter {
    /// Boxed: the reader holds a Document, which is aligned to 128 bytes,
    /// and Python allocates a class's objects aligned only to 16.
    nodes: Mutex<Box<XmlNodes>>,
    /// For the warning when recovering repaired anything.
    place: Option<String>,
}

// Every class here is allocated by Python, which aligns to 16 bytes and
// no more: anything held inline that wants more goes in a Box or an Arc.
const _: () = assert!(std::mem::align_of::<XmlNodeIter>() <= 16);
const _: () = assert!(std::mem::align_of::<node::Node>() <= 16);
const _: () = assert!(std::mem::align_of::<node::Selection>() <= 16);

#[pymethods]
impl XmlNodeIter {
    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Node>> {
        let next = py.detach(|| {
            self.nodes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .next_node()
        });
        match next {
            None => {
                let repairs = self
                    .nodes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .repairs();
                warn_repairs(py, self.place.as_deref(), &repairs)?;
                Ok(None)
            }
            Some(Err(e)) => Err(xml_error(py, &e)),
            Some(Ok((doc, binding))) => {
                let node = Node::top_element_of(Arc::new(doc));
                // A prefix the tag took from the document works in the
                // node's queries too.
                if let Some((prefix, uri)) = binding {
                    node.bind(prefix, uri);
                }
                Ok(Some(node))
            }
        }
    }
}

/// `tag` is `name` or `prefix:name`, the prefix bound in `namespaces` or
/// by the document. `timeout` is for reading each node; with `recover`,
/// broken XML is repaired where it can be, and a warning naming `place`
/// says what was, once the nodes run out.
#[pyfunction]
#[pyo3(signature = (text, tag, namespaces, recover=false, timeout=None, place=None))]
fn _xml_nodes(
    text: String,
    tag: &str,
    namespaces: Vec<(String, String)>,
    recover: bool,
    timeout: Option<f64>,
    place: Option<String>,
) -> PyResult<XmlNodeIter> {
    let options = XmlOptions {
        recover,
        budget: budget(timeout)?,
    };
    let nodes = XmlNodes::new(text, tag, &namespaces, options).map_err(PyValueError::new_err)?;
    Ok(XmlNodeIter {
        nodes: Mutex::new(Box::new(nodes)),
        place,
    })
}

/// `text` encoded as a browser would encode it in `encoding` (so that
/// decoding gives it back); None if that encoding can't hold it.
#[pyfunction]
fn _encode<'py>(
    py: Python<'py>,
    text: &str,
    encoding: &str,
) -> Option<Bound<'py, pyo3::types::PyBytes>> {
    netweir_core::encode(text, encoding).map(|b| pyo3::types::PyBytes::new(py, &b))
}

/// The crawl's identity for a GET of `url`, as hex: equal for the same
/// page however its URL is written. None for anything but http(s).
#[pyfunction]
fn fingerprint(url: &str) -> Option<String> {
    netweir_core::canonical::fingerprint("GET", url)
        .map(|fp| fp.iter().map(|b| format!("{b:02x}")).collect())
}

/// A sitemap's kind ("urlset" or "index"), its entries, as dicts with
/// loc, lastmod, changefreq, priority and alternates, and what reading a
/// broken one repaired (None if nothing). Gzip is unzipped, to at most
/// `max` bytes. ValueError if it isn't a sitemap.
/// With `stop`, reading gives up soon after it's set.
#[pyfunction]
#[pyo3(signature = (body, max, stop=None))]
#[allow(clippy::type_complexity)]
fn _sitemap<'py>(
    py: Python<'py>,
    body: &[u8],
    max: usize,
    stop: Option<&StopFlag>,
) -> PyResult<(
    &'static str,
    Vec<Bound<'py, pyo3::types::PyDict>>,
    Option<String>,
)> {
    let stop = stop.map_or_else(Default::default, |s| s.0.clone());
    let map = py
        .detach(|| netweir_core::sitemap::parse_until(body, max, &stop))
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
    Ok((kind, entries, map.repaired))
}

/// A flag a reader on another thread checks as it goes, to give up once
/// whoever wanted its answer has gone.
#[pyclass(frozen, module = "netweir")]
#[derive(Default)]
struct StopFlag(Arc<std::sync::atomic::AtomicBool>);

#[pymethods]
impl StopFlag {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    fn set(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
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
    m.add_function(wrap_pyfunction!(_encode, m)?)?;
    m.add_class::<XmlNodeIter>()?;
    m.add_class::<StopFlag>()?;
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
