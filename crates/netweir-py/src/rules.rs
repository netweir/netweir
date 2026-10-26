//! Declarative items and link rules: compiled from Python once, run in Rust
//! on every page.

use std::sync::Arc;

use netweir_dom::auto::{self, Schema};
use netweir_dom::date;
use netweir_dom::extract::{
    self, Convert, Field, Selector, TrackContext, Tracker, Tracking, Tracks, Value,
};
use netweir_dom::{Output, Query, XPath};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use url::Url;

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

/// Whether a CSS query ends in `::text` or `::attr()`, so finds strings
/// rather than elements. XPath can only tell when it runs.
fn ends_in_strings(kind: &str, query: &str) -> bool {
    kind == "css"
        && Query::new(query).is_ok_and(|q| q.outputs().iter().any(|o| *o != Output::Nodes))
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
    /// The compiled fields' names, in order.
    fields: Vec<String>,
    /// What an item holds, in order: the fields, or for an auto item its
    /// type's fields, then extra fields, then `metadata`.
    output: Vec<String>,
    /// Each output's value when nothing is found.
    defaults: Vec<Py<PyAny>>,
    auto: Option<Auto>,
}

/// An auto item: its type, and each compiled field's source label.
struct Auto {
    schema: Schema,
    sources: Vec<String>,
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
    /// The item's values in output order; `page` is the page's URL and
    /// `base` what its relative links are relative to.
    pub(crate) fn extract(
        &self,
        root: netweir_dom::Node<'_>,
        page: Option<&Url>,
        base: Option<&Url>,
    ) -> Vec<Value> {
        let values = self.spec.extract(root, base);
        self.assemble(root, page, base, values)
    }

    /// An auto item's values: its type's fields as auto-extraction found
    /// them, the compiled ones pinned in their place, extras, then
    /// metadata. Any other item's values are the compiled fields'.
    fn assemble(
        &self,
        root: netweir_dom::Node<'_>,
        page: Option<&Url>,
        base: Option<&Url>,
        values: Vec<Value>,
    ) -> Vec<Value> {
        let Some(auto) = &self.auto else {
            return values;
        };
        let now = date::now_utc();
        let found = auto::extract(auto.schema, root, page, base, now);
        let mut sources: Vec<(&str, String)> = found.sources.clone();
        let mut validation = found.validation.clone();
        let mut out = Vec::with_capacity(self.output.len());
        for (field, value) in found.fields {
            let Some(i) = self.fields.iter().position(|f| f == field) else {
                out.push(Value::Json(value));
                continue;
            };
            sources.retain(|(f, _)| *f != field);
            let src = &auto.sources[i];
            out.push(match &values[i] {
                Value::Missing => {
                    validation.push((field, format!("{src} matched nothing")));
                    Value::Missing
                }
                Value::Invalid(text) => {
                    validation.push((field, unreadable(src, text, field)));
                    Value::Json(serde_json::Value::Null)
                }
                Value::Error(reason) => {
                    validation.push((field, format!("{src} failed: {reason}")));
                    Value::Json(serde_json::Value::Null)
                }
                Value::List(items) => {
                    sources.push((field, src.clone()));
                    Value::List(
                        items
                            .iter()
                            .map(|v| match v {
                                Value::Invalid(text) => {
                                    validation.push((field, unreadable(src, text, field)));
                                    Value::Json(serde_json::Value::Null)
                                }
                                other => other.clone(),
                            })
                            .collect(),
                    )
                }
                other => {
                    sources.push((field, src.clone()));
                    other.clone()
                }
            });
        }
        for (i, name) in self.fields.iter().enumerate() {
            if !auto.schema.fields().contains(&name.as_str()) {
                out.push(values[i].clone());
            }
        }
        let order = |f: &str| {
            auto.schema
                .fields()
                .iter()
                .position(|x| *x == f)
                .unwrap_or(usize::MAX)
        };
        sources.sort_by_key(|(f, _)| order(f));
        let mut messages = serde_json::Map::new();
        for (field, message) in validation {
            if let serde_json::Value::Array(list) = messages
                .entry(field.to_string())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            {
                list.push(serde_json::Value::String(message));
            }
        }
        out.push(Value::Json(serde_json::json!({
            "probability": found.probability,
            "date_downloaded": date::iso_utc(now),
            "sources": sources.into_iter().map(|(f, s)| (f.to_string(), serde_json::Value::String(s))).collect::<serde_json::Map<_, _>>(),
            "validation_messages": messages,
        })));
        out
    }

    /// `extract`, with tracked fields followed through `ctx`.
    pub(crate) fn extract_tracked(
        &self,
        root: netweir_dom::Node<'_>,
        page: Option<&Url>,
        base: Option<&Url>,
        ctx: &TrackContext<'_>,
    ) -> (Vec<Value>, Notes) {
        let (values, notes) = self.spec.extract_with(root, base, ctx);
        let values = self.assemble(root, page, base, values);
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
        for ((name, default), value) in self.output.iter().zip(&self.defaults).zip(values) {
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

/// The note for a pinned field whose element held text that doesn't read
/// as the field: the text, its whitespace collapsed and cut at 80
/// characters, since the element can be a whole container.
fn unreadable(src: &str, text: &str, field: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = match text.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    };
    format!("{src} found '{text}', which doesn't read as {field}")
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
        Value::Json(v) => crate::browser::to_python(py, &v)?.into_bound(py),
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
    /// "text" | "int" | "float" | "bool" | "markdown" | "markdown_main" |
    /// "clean_text" | "clean_text_main", default, track name or None) for
    /// each field.
    #[new]
    #[allow(clippy::type_complexity)]
    #[pyo3(signature = (name, fields, schema=None))]
    fn new(
        py: Python<'_>,
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
        schema: Option<&str>,
    ) -> PyResult<ItemSpec> {
        let schema = schema
            .map(|s| {
                Schema::from_name(s).ok_or_else(|| {
                    PyValueError::new_err(format!("no auto-extracted item type {s:?}"))
                })
            })
            .transpose()?;
        let mut compiled = Vec::new();
        let mut names = Vec::new();
        let mut defaults = Vec::new();
        let mut sources = Vec::new();
        for (field, kind, query, pattern, all, strip, convert, default, track) in fields {
            let convert = match convert.as_str() {
                c if c.starts_with("auto:") => match schema {
                    Some(s) => Convert::Auto(s.kind_of(&c[5..]).ok_or_else(|| {
                        PyValueError::new_err(format!(
                            "{field} can't be pinned with a selector: no one element holds it. \
                             Give it into= to read it yourself."
                        ))
                    })?),
                    None => {
                        return Err(PyValueError::new_err(format!(
                            "{field}: {c:?} is for auto-extracted items"
                        )));
                    }
                },
                "text" => Convert::Text,
                "int" => Convert::Int,
                "float" => Convert::Float,
                "bool" => Convert::Bool,
                "markdown" => Convert::Markdown { main_only: false },
                "markdown_main" => Convert::Markdown { main_only: true },
                "clean_text" => Convert::CleanText { main_only: false },
                "clean_text_main" => Convert::CleanText { main_only: true },
                other => {
                    return Err(PyValueError::new_err(format!(
                        "a field converts to text, int, float, bool, markdown or clean_text, \
                         not {other:?}"
                    )));
                }
            };
            if convert.takes_element() && ends_in_strings(&kind, &query) {
                return Err(PyValueError::new_err(format!(
                    "{field}: Markdown and clean text are made from an element, and {query:?} \
                     gives strings"
                )));
            }
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
            sources.push(format!("{kind}:{query}"));
            names.push(field);
            defaults.push(default);
        }
        let (output, defaults, auto) = match schema {
            None => (names.clone(), defaults, None),
            Some(schema) => {
                let mut output = Vec::new();
                let mut out_defaults = Vec::new();
                for field in schema.fields() {
                    output.push(field.to_string());
                    out_defaults.push(match names.iter().position(|n| n == field) {
                        Some(i) => defaults[i].clone_ref(py),
                        None => py.None(),
                    });
                }
                for (i, n) in names.iter().enumerate() {
                    if !schema.fields().contains(&n.as_str()) {
                        output.push(n.clone());
                        out_defaults.push(defaults[i].clone_ref(py));
                    }
                }
                output.push("metadata".into());
                out_defaults.push(py.None());
                (output, out_defaults, Some(Auto { schema, sources }))
            }
        };
        Ok(ItemSpec {
            inner: Arc::new(ItemInner {
                spec: extract::ItemSpec::new(compiled),
                name,
                fields: names,
                output,
                defaults,
                auto,
            }),
        })
    }

    /// The item found below `node`, its problems, and what tracking did
    /// (with `store`, tracked fields are followed on `site`).
    #[pyo3(signature = (node, store=None, site="", threshold=0.75, base_url=None, page_url=None))]
    #[allow(clippy::too_many_arguments)]
    fn extract<'py>(
        &self,
        py: Python<'py>,
        node: &Node,
        store: Option<&TrackStore>,
        site: &str,
        threshold: f64,
        base_url: Option<&str>,
        page_url: Option<&str>,
    ) -> PyResult<(Bound<'py, PyDict>, Vec<Problem>, Notes)> {
        let (doc, id, inner) = (node.document().clone(), node.node_id(), self.inner.clone());
        let base = base_url.and_then(|b| Url::parse(b).ok());
        let page = page_url.and_then(|p| Url::parse(p).ok());
        let (tracks, site) = (store.map(TrackStore::tracks), site.to_string());
        // SAFETY: the id came from a Node of this document, which `doc` keeps
        // alive.
        let (values, notes) = py.detach(move || {
            let _reading = doc.read();
            let root = unsafe { doc.node(id) };
            match &tracks {
                Some(tracks) => inner.extract_tracked(
                    root,
                    page.as_ref(),
                    base.as_ref(),
                    &TrackContext {
                        tracks,
                        site: &site,
                        threshold,
                    },
                ),
                None => (
                    inner.extract(root, page.as_ref(), base.as_ref()),
                    Vec::new(),
                ),
            }
        });
        let (dict, problems) = self.inner.to_python(py, values)?;
        Ok((dict, problems, notes))
    }
}

/// The fields of an auto-extracted item type, in order.
#[pyfunction]
pub(crate) fn _auto_fields(schema: &str) -> PyResult<Vec<&'static str>> {
    Schema::from_name(schema)
        .map(Schema::fields)
        .map(<[&str]>::to_vec)
        .ok_or_else(|| PyValueError::new_err(format!("no auto-extracted item type {schema:?}")))
}

/// Where a rule's links come from.
pub(crate) enum Links {
    /// What a CSS or XPath query finds.
    Query(Selector),
    /// A part of the page's auto-extracted navigation item: the items it
    /// lists, its next page, or its sub-categories.
    Navigation(Schema, Part),
    /// None: the rule's item is taken from the page the rules run on.
    Here,
}

#[derive(Clone, Copy)]
pub(crate) enum Part {
    Items,
    NextPage,
    SubCategories,
}

impl Links {
    /// From `add_rule`'s kind and query: "css" or "xpath" with a query,
    /// "navigation" with "<schema>:<part>", or "here".
    pub fn new(kind: &str, query: &str) -> PyResult<Links> {
        match kind {
            "here" => Ok(Links::Here),
            "navigation" => {
                let (schema, part) = query.split_once(':').unwrap_or((query, ""));
                let schema = Schema::from_name(schema)
                    .filter(|s| s.fields().contains(&"sub_categories"))
                    .ok_or_else(|| {
                        PyValueError::new_err(format!("{schema:?} isn't a navigation item type"))
                    })?;
                let part = match part {
                    "items" => Part::Items,
                    "next_page" => Part::NextPage,
                    "sub_categories" => Part::SubCategories,
                    other => {
                        return Err(PyValueError::new_err(format!(
                            "part is items, next_page or sub_categories, not {other:?}"
                        )));
                    }
                };
                Ok(Links::Navigation(schema, part))
            }
            _ => selector(kind, query).map(Links::Query),
        }
    }
}

/// A link rule: which links to take from a page, and what to do with the
/// pages they lead to.
pub(crate) struct Rule {
    pub links: Links,
    /// For a `Here` rule, the page type a page must be for its item to be
    /// taken.
    pub when: Option<Schema>,
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
    m.add_function(wrap_pyfunction!(_auto_fields, m)?)?;
    m.add_class::<ItemSpec>()?;
    m.add_class::<TrackStore>()
}
