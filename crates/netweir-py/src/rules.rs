//! Declarative items and link rules: compiled from Python once, run in Rust
//! on every page.

use std::sync::Arc;

use netweir_dom::extract::{
    self, Convert, Field, Selector, TrackContext, Tracker, Tracking, Tracks, Value,
};
use netweir_dom::{Query, XPath};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::node::Node;
use crate::{SelectorError, XPathError};

/// A compiled CSS or XPath query, with the error type Python expects.
pub(crate) fn selector(kind: &str, query: &str) -> PyResult<Selector> {
    match kind {
        "css" => Query::new(query)
            .map(Selector::Css)
            .map_err(|e| SelectorError::new_err(e.to_string())),
        "xpath" => XPath::new(query)
            .map(Selector::XPath)
            .map_err(|e| XPathError::new_err(e.to_string())),
        other => Err(PyValueError::new_err(format!(
            "a query is css or xpath, not {other:?}"
        ))),
    }
}

/// What went wrong with one field on one page: ("module.Class.field",
/// what was found, "convert" or "query").
pub(crate) type Problem = (String, String, &'static str);

/// An extracted item, and its problems.
pub(crate) type Extracted<'py> = (Bound<'py, PyDict>, Vec<Problem>);

/// Where tracked selectors keep their fingerprints: an SQLite file.
#[pyclass(frozen, module = "netweir")]
pub struct TrackStore {
    inner: Arc<netweir_core::tracks::TrackStore>,
}

/// A TrackStore as netweir-dom's Tracks.
#[derive(Clone)]
pub(crate) struct StoreRef(Arc<netweir_core::tracks::TrackStore>);

impl Tracks for StoreRef {
    fn get(&self, site: &str, name: &str) -> Option<String> {
        self.0.get(site, name)
    }

    fn put(&self, site: &str, name: &str, fingerprint: &str) {
        self.0.put(site, name, fingerprint)
    }
}

impl TrackStore {
    pub(crate) fn tracks(&self) -> StoreRef {
        StoreRef(self.inner.clone())
    }
}

#[pymethods]
impl TrackStore {
    /// The saved fingerprint (JSON) for `name` on `site`, if any.
    fn get(&self, site: &str, name: &str) -> Option<String> {
        self.inner.get(site, name)
    }

    /// Closes the file.
    fn close(&self) {
        self.inner.close();
    }

    #[new]
    fn new(path: std::path::PathBuf) -> PyResult<TrackStore> {
        Ok(TrackStore {
            inner: Arc::new(
                netweir_core::tracks::TrackStore::open(&path)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?,
            ),
        })
    }
}

/// What tracking did to an item's fields: (field, "relocated" or "lost",
/// score, or for a lost one the best candidate's).
pub(crate) type Notes = Vec<(String, &'static str, Option<f64>)>;

pub(crate) struct ItemInner {
    spec: extract::ItemSpec,
    /// Python's class name, for messages.
    name: String,
    fields: Vec<String>,
    defaults: Vec<Py<PyAny>>,
}

// The last reference to an Item's spec can go on a runtime thread: a
// rule page's parse job keeps the engine, and its rules, until it ends,
// even when the next() that started it was cancelled.
impl Drop for ItemInner {
    fn drop(&mut self) {
        crate::aio::release(std::mem::take(&mut self.defaults));
    }
}

impl ItemInner {
    pub(crate) fn extract(&self, root: netweir_dom::Node<'_>) -> Vec<Value> {
        self.spec.extract(root)
    }

    /// `extract`, with tracked fields followed through `ctx`.
    pub(crate) fn extract_tracked(
        &self,
        root: netweir_dom::Node<'_>,
        ctx: &TrackContext<'_>,
    ) -> (Vec<Value>, Notes) {
        let (values, notes) = self.spec.extract_with(root, ctx);
        let notes = notes
            .into_iter()
            .map(|(i, t)| {
                let name = format!("{}.{}", self.name, self.fields[i]);
                match t {
                    Tracking::Relocated(score) => (name, "relocated", Some(score)),
                    Tracking::Lost(best) => (name, "lost", Some(best)),
                    _ => (name, "lost", None),
                }
            })
            .collect();
        (values, notes)
    }

    /// The item as a dict in field order, a missing value replaced by its
    /// default, and one that would not convert or whose query failed by
    /// None, with a Problem for each of those.
    pub(crate) fn to_python<'py>(
        &self,
        py: Python<'py>,
        values: Vec<Value>,
    ) -> PyResult<Extracted<'py>> {
        let dict = PyDict::new(py);
        let mut invalid = Vec::new();
        for ((name, default), value) in self.fields.iter().zip(&self.defaults).zip(values) {
            let v = match value {
                Value::Missing => default.clone_ref(py).into_bound(py),
                other => value_py(py, other, &mut |text, kind| {
                    invalid.push((format!("{}.{name}", self.name), text, kind))
                })?,
            };
            dict.set_item(name, v)?;
        }
        Ok((dict, invalid))
    }
}

fn value_py<'py>(
    py: Python<'py>,
    value: Value,
    invalid: &mut dyn FnMut(String, &'static str),
) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Missing => py.None().into_bound(py),
        Value::Text(s) => s.into_pyobject(py)?.into_any(),
        Value::Int(i) => i.into_pyobject(py)?.into_any(),
        Value::Float(f) => f.into_pyobject(py)?.into_any(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any(),
        Value::List(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(value_py(py, item, invalid)?)?;
            }
            list.into_any()
        }
        Value::Invalid(text) => {
            invalid(text, "convert");
            py.None().into_bound(py)
        }
        Value::Error(reason) => {
            invalid(reason, "query");
            py.None().into_bound(py)
        }
    })
}

/// An Item class's fields, compiled. Built by `netweir.Item`; not meant to
/// be made by hand.
#[pyclass(frozen, module = "netweir")]
pub struct ItemSpec {
    pub(crate) inner: Arc<ItemInner>,
}

#[pymethods]
impl ItemSpec {
    /// `fields` holds (name, "css" | "xpath", query, pattern, all, strip,
    /// "text" | "int" | "float" | "bool", default, track name or None) for
    /// each field.
    #[new]
    #[allow(clippy::type_complexity)]
    fn new(
        name: String,
        fields: Vec<(
            String,
            String,
            String,
            Option<String>,
            bool,
            bool,
            String,
            Py<PyAny>,
            Option<String>,
        )>,
    ) -> PyResult<ItemSpec> {
        let mut compiled = Vec::new();
        let mut names = Vec::new();
        let mut defaults = Vec::new();
        for (field, kind, query, pattern, all, strip, convert, default, track) in fields {
            let convert = match convert.as_str() {
                "text" => Convert::Text,
                "int" => Convert::Int,
                "float" => Convert::Float,
                "bool" => Convert::Bool,
                other => {
                    return Err(PyValueError::new_err(format!(
                        "a field converts to text, int, float or bool, not {other:?}"
                    )));
                }
            };
            let mut f = Field::new(&field, selector(&kind, &query)?)
                .all(all)
                .strip(strip)
                .convert(convert);
            if let Some(p) = pattern {
                f = f.re(&p).map_err(PyValueError::new_err)?;
            }
            if let Some(t) = track {
                f = f.track(Tracker::new(&t, &kind, &query).map_err(PyValueError::new_err)?);
            }
            compiled.push(f);
            names.push(field);
            defaults.push(default);
        }
        Ok(ItemSpec {
            inner: Arc::new(ItemInner {
                spec: extract::ItemSpec::new(compiled),
                name,
                fields: names,
                defaults,
            }),
        })
    }

    /// The item found below `node`, its problems, and what tracking did
    /// (with `store`, tracked fields are followed on `site`).
    #[pyo3(signature = (node, store=None, site="", threshold=0.75))]
    fn extract<'py>(
        &self,
        py: Python<'py>,
        node: &Node,
        store: Option<&TrackStore>,
        site: &str,
        threshold: f64,
    ) -> PyResult<(Bound<'py, PyDict>, Vec<Problem>, Notes)> {
        let (doc, id, inner) = (node.document().clone(), node.node_id(), self.inner.clone());
        let (tracks, site) = (store.map(TrackStore::tracks), site.to_string());
        // SAFETY: the id came from a Node of this document, which `doc` keeps
        // alive.
        let (values, notes) = py.detach(move || {
            let _reading = doc.read();
            let root = unsafe { doc.node(id) };
            match &tracks {
                Some(tracks) => inner.extract_tracked(
                    root,
                    &TrackContext {
                        tracks,
                        site: &site,
                        threshold,
                    },
                ),
                None => (inner.extract(root), Vec::new()),
            }
        });
        let (dict, problems) = self.inner.to_python(py, values)?;
        Ok((dict, problems, notes))
    }
}

/// A link rule: which links to take from a page, and what to do with the
/// pages they lead to.
pub(crate) struct Rule {
    pub selector: Selector,
    pub item: Option<Arc<ItemInner>>,
    /// Apply the rules to the pages this rule leads to.
    pub follow: bool,
    /// Hand those pages to a Python callback.
    pub to_python: bool,
    pub priority: i32,
    /// Links are kept only if they match one of `allow` (when it isn't
    /// empty) and none of `deny`, and are within `allow_domains` (when it
    /// isn't empty) and outside `deny_domains`.
    pub allow: Vec<regex::Regex>,
    pub deny: Vec<regex::Regex>,
    pub allow_domains: Vec<String>,
    pub deny_domains: Vec<String>,
    /// Fetch the pages it leads to in Chrome.
    pub browser: bool,
    /// What to do on them first, in place of the crawl's steps.
    pub steps: Option<Arc<[netweir_browser::steps::Step]>>,
}

impl Rule {
    /// Whether this rule takes the link to `url`.
    pub fn takes(&self, url: &url::Url) -> bool {
        let s = url.as_str();
        (self.allow.is_empty() || self.allow.iter().any(|r| r.is_match(s)))
            && !self.deny.iter().any(|r| r.is_match(s))
            && !netweir_core::offsite(url, &self.allow_domains)
            && (self.deny_domains.is_empty() || netweir_core::offsite(url, &self.deny_domains))
    }
}

/// How much the element `query` finds below `node` looks like the one in
/// `fingerprint` (JSON), 0 to 1; None if it finds no element.
#[pyfunction]
pub fn similarity(
    node: &Node,
    kind: &str,
    query: &str,
    fingerprint: &str,
) -> PyResult<Option<f64>> {
    let fp = netweir_dom::track::Fingerprint::from_json(fingerprint)
        .ok_or_else(|| PyValueError::new_err("not a fingerprint"))?;
    let tracker = Tracker::new("similarity", kind, query).map_err(PyValueError::new_err)?;
    Ok(tracker
        .first_element(*node.get())
        .map_err(PyValueError::new_err)?
        .map(|el| fp.score(el)))
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(similarity, m)?)?;
    m.add_class::<ItemSpec>()?;
    m.add_class::<TrackStore>()
}
