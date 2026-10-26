//! `Node` and `Selection`: the parsel-style query API (css, xpath, get,
//! getall, re) and the Beautiful Soup style one (find_all and navigation).

use std::ops::Deref;
use std::sync::{Arc, Mutex};

use netweir_dom::extract::{TrackContext, Tracker, Tracking};
use netweir_dom::{
    Content, Document, EditError, Hit, NodeId, NodeKind, Query, Reading, XPath, XValue,
};
use pyo3::exceptions::{
    PyIndexError, PyKeyError, PyMemoryError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PySlice, PyString, PyTuple};

use crate::filter::{Criteria, Filter, single_string};
use crate::rules::TrackStore;
use crate::{SelectorError, XPathError};

/// A node in a parsed document: an element, a piece of text, a comment, or
/// the document itself.
#[pyclass(frozen, module = "netweir")]
pub(crate) struct Node {
    doc: Arc<Document>,
    id: NodeId,
    /// The namespace prefixes XPath queries from this node know, as
    /// parsel's `Selector.namespaces`: registered on it, or on the node or
    /// selection it came from (before it came from there).
    namespaces: Mutex<Namespaces>,
}

/// `(prefix, uri)` pairs; a later one for a prefix wins.
type Namespaces = Arc<Vec<(String, String)>>;

/// One result of a query: a node, or a string (an attribute's value or a
/// computed XPath value).
#[derive(Clone)]
enum Entry {
    Node(NodeId),
    Value(String),
}

/// The results of a query, in document order. Indexing gives a Node for an
/// element or comment and a str for text and attribute values.
#[pyclass(frozen, sequence, module = "netweir")]
pub(crate) struct Selection {
    doc: Arc<Document>,
    entries: Vec<Entry>,
    /// With track=: the selector found nothing and these results were
    /// found by similarity.
    relocated: bool,
    /// With track=: 1.0 when the selector matched, the similarity when
    /// relocated, the nearest candidate's score when nothing was similar
    /// enough, None when there was nothing to look for.
    score: Option<f64>,
    /// The namespaces of the node it came from, or registered on it,
    /// which its nodes and their queries inherit.
    namespaces: Mutex<Namespaces>,
}

impl Selection {
    fn of(doc: Arc<Document>, entries: Vec<Entry>, namespaces: Namespaces) -> Selection {
        Selection {
            doc,
            entries,
            relocated: false,
            score: None,
            namespaces: Mutex::new(namespaces),
        }
    }

    fn ns(&self) -> Namespaces {
        self.namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Node {
    pub(crate) fn root_of(doc: Arc<Document>) -> Node {
        let id = doc.root().id();
        Node::with(doc, id, Namespaces::default())
    }

    /// The root element of `doc` (the document node if it has none).
    pub(crate) fn top_element_of(doc: Arc<Document>) -> Node {
        let id = doc
            .root()
            .children()
            .find(|n| n.kind() == NodeKind::Element)
            .unwrap_or(doc.root())
            .id();
        Node::with(doc, id, Namespaces::default())
    }

    fn with(doc: Arc<Document>, id: NodeId, namespaces: Namespaces) -> Node {
        Node {
            doc,
            id,
            namespaces: Mutex::new(namespaces),
        }
    }

    /// Binds `prefix` to `uri` for queries from this node on.
    pub(crate) fn bind(&self, prefix: String, uri: String) {
        bind(&self.namespaces, prefix, uri);
    }

    fn namespaces(&self) -> Namespaces {
        self.namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn document(&self) -> &Arc<Document> {
        &self.doc
    }

    pub(crate) fn node_id(&self) -> NodeId {
        self.id
    }

    /// The node, with edits from other threads held off while it's used.
    pub(crate) fn get(&self) -> Held<'_> {
        Held {
            _reading: self.doc.read(),
            // SAFETY: `id` was taken from a node of `doc`, which this Node keeps alive.
            node: unsafe { self.doc.node(self.id) },
        }
    }

    /// Runs a tree edit without the GIL, so that while it waits for
    /// readers on other threads, they can finish.
    fn edit<R: Send>(
        &self,
        py: Python<'_>,
        edit: impl FnOnce(&Document) -> Result<R, EditError> + Send,
    ) -> PyResult<R> {
        let doc = self.doc.clone();
        py.detach(move || edit(&doc)).map_err(edit_error)
    }

    /// What `items` stand for, ready to insert: Nodes of this page as they
    /// are, strings as text the edit will create, so one edit does both.
    fn insertable(&self, items: &[Bound<'_, PyAny>]) -> PyResult<Vec<Insert>> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if let Ok(node) = item.cast::<Node>() {
                let node = node.get();
                if !Arc::ptr_eq(&node.doc, &self.doc) {
                    return Err(PyValueError::new_err(
                        "that node belongs to another page; parse its HTML into this one with new_tag()",
                    ));
                }
                out.push(Insert::Node(node.id));
            } else if let Ok(text) = item.cast::<PyString>() {
                out.push(Insert::Text(text.to_string()));
            } else {
                return Err(PyTypeError::new_err(format!(
                    "only a Node or a str can be inserted, not {}",
                    item.get_type().name()?
                )));
            }
        }
        Ok(out)
    }

    fn sibling(&self, id: NodeId) -> Node {
        Node::with(self.doc.clone(), id, self.namespaces())
    }

    fn wrap(&self, node: Option<netweir_dom::Node<'_>>) -> Option<Node> {
        node.map(|n| self.sibling(n.id()))
    }

    fn wrap_all<'a>(&self, nodes: impl Iterator<Item = netweir_dom::Node<'a>>) -> Vec<Node> {
        nodes.map(|n| self.sibling(n.id())).collect()
    }
}

/// An owned [`Content`], to carry into an edit that runs without the GIL.
enum Insert {
    Node(NodeId),
    Text(String),
}

fn contents(items: &[Insert]) -> Vec<Content<'_>> {
    items
        .iter()
        .map(|i| match i {
            Insert::Node(id) => Content::Node(*id),
            Insert::Text(t) => Content::Text(t),
        })
        .collect()
}

/// A node and a hold on its document's edits. Python-facing code reads
/// through one of these, so a thread editing the page waits until the
/// read is done (on free-threaded Python, readers don't hold the GIL).
pub(crate) struct Held<'a> {
    _reading: Reading<'a>,
    node: netweir_dom::Node<'a>,
}

impl<'a> Deref for Held<'a> {
    type Target = netweir_dom::Node<'a>;

    fn deref(&self) -> &Self::Target {
        &self.node
    }
}

fn edit_error(e: EditError) -> PyErr {
    match e {
        EditError::Invalid(why) => PyValueError::new_err(why),
        EditError::Reading => PyRuntimeError::new_err(format!(
            "{e} (a find_all() function can't change a page; collect what to change, then change it)"
        )),
    }
}

fn kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Element => "element",
        NodeKind::Text => "text",
        NodeKind::Comment => "comment",
        NodeKind::ProcessingInstruction => "processing_instruction",
        NodeKind::Document => "document",
        NodeKind::Doctype => "doctype",
        NodeKind::Other => "other",
    }
}

/// What `get()` returns for a node: text as itself, anything else as HTML.
fn node_string(node: netweir_dom::Node<'_>) -> String {
    match node.kind() {
        NodeKind::Text => node.data().unwrap_or_default().to_string(),
        _ => node.html(),
    }
}

fn entries_of(hits: Vec<Hit<'_>>) -> Vec<Entry> {
    hits.into_iter()
        .map(|h| match h {
            Hit::Node(n) => Entry::Node(n.id()),
            Hit::Value(v) => Entry::Value(v),
        })
        .collect()
}

fn css_entries(
    py: Python<'_>,
    doc: &Arc<Document>,
    scopes: Vec<NodeId>,
    css: &str,
) -> PyResult<Vec<Entry>> {
    let query = Query::cached(css).map_err(|e| SelectorError::new_err(e.to_string()))?;
    let doc = doc.clone();
    py.detach(move || {
        let _reading = doc.read();
        let mut out = Vec::new();
        for id in scopes {
            // SAFETY: every scope id came from `doc`.
            out.extend(entries_of(query.try_run(unsafe { doc.node(id) })?));
        }
        Ok::<_, netweir_dom::OutOfMemory>(out)
    })
    .map_err(|e| PyMemoryError::new_err(e.to_string()))
}

/// css(track=...) and xpath(track=...) on a bare Node: netweir._track knows
/// the store and threshold in force, and reports what happened.
fn tracked_through_python(
    py: Python<'_>,
    node: &Bound<'_, Node>,
    kind: &str,
    query: &str,
    name: &str,
) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("netweir._track")?
        .getattr("tracked")?
        .call1((node, kind, query, name, ""))?
        .unbind())
}

/// Keyword arguments to XPath variables: str, int, float or bool.
fn xpath_vars(vars: Option<&Bound<'_, PyDict>>) -> PyResult<Vec<(String, XValue)>> {
    let mut out = Vec::new();
    for (k, v) in vars.into_iter().flat_map(|d| d.iter()) {
        let name: String = k.extract()?;
        let value = if let Ok(b) = v.cast::<PyBool>() {
            XValue::Bool(b.is_true())
        } else if v.is_instance_of::<PyInt>() || v.is_instance_of::<PyFloat>() {
            XValue::Num(v.extract::<f64>()?)
        } else if let Ok(s) = v.cast::<PyString>() {
            XValue::Str(s.to_string())
        } else {
            return Err(PyTypeError::new_err(format!(
                "XPath variable ${name} must be a str, int, float or bool"
            )));
        };
        out.push((name, value));
    }
    Ok(out)
}

fn bind(namespaces: &Mutex<Namespaces>, prefix: String, uri: String) {
    let mut namespaces = namespaces.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = (**namespaces).clone();
    all.push((prefix, uri));
    *namespaces = Arc::new(all);
}

fn check_prefix(prefix: &str) -> PyResult<()> {
    if netweir_dom::xml::is_ncname(prefix) {
        Ok(())
    } else {
        Err(PyValueError::new_err(format!(
            "{prefix:?} isn't a valid namespace prefix"
        )))
    }
}

/// The namespaces a query runs with: those inherited, then those passed to
/// the query itself (a dict of prefix to URI), which win.
fn query_namespaces(
    inherited: &Namespaces,
    given: Option<&Bound<'_, PyAny>>,
) -> PyResult<Namespaces> {
    let Some(given) = given.filter(|g| !g.is_none()) else {
        return Ok(inherited.clone());
    };
    let given = given.cast::<PyDict>().map_err(|_| {
        PyTypeError::new_err("namespaces must be a dict of prefix to namespace URI")
    })?;
    let mut all = (**inherited).clone();
    for (prefix, uri) in given.iter() {
        all.push((prefix.extract()?, uri.extract()?));
    }
    Ok(Arc::new(all))
}

fn xpath_entries(
    py: Python<'_>,
    doc: &Arc<Document>,
    scopes: Vec<NodeId>,
    query: &str,
    vars: Option<&Bound<'_, PyDict>>,
    namespaces: Namespaces,
) -> PyResult<Vec<Entry>> {
    let xpath = XPath::cached(query).map_err(|e| XPathError::new_err(e.to_string()))?;
    let vars = xpath_vars(vars)?;
    let doc = doc.clone();
    py.detach(move || {
        let _reading = doc.read();
        let mut out = Vec::new();
        for id in scopes {
            // SAFETY: every scope id came from `doc`.
            let mut hits = xpath
                .run_in(unsafe { doc.node(id) }, &vars, &namespaces)
                .map_err(|e| e.to_string())?;
            // As lxml (so parsel) has it in XML: the document node is no
            // result, so `/*/..` finds nothing, as `..` from a node read
            // on its own doesn't.
            if doc.is_xml() {
                hits.retain(|h| !matches!(h, Hit::Node(n) if n.kind() == NodeKind::Document));
            }
            out.extend(entries_of(hits));
        }
        Ok::<_, String>(out)
    })
    .map_err(XPathError::new_err)
}

/// Beautiful Soup's `limit`: None or 0 for no limit.
fn bs4_limit(limit: Option<i64>) -> PyResult<Option<usize>> {
    match limit {
        None | Some(0) => Ok(None),
        Some(n) if n > 0 => Ok(Some(n as usize)),
        Some(n) => Err(pyo3::exceptions::PyValueError::new_err(format!(
            "limit must be 0 (no limit) or more, not {n}"
        ))),
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Descendants,
    Children,
    Parents,
    NextSiblings,
    PreviousSiblings,
    Following,
    Preceding,
}

impl Node {
    fn criteria(
        name: Option<&Bound<'_, PyAny>>,
        attrs: Option<&Bound<'_, PyAny>>,
        string: Option<&Bound<'_, PyAny>>,
        class_: Option<&Bound<'_, PyAny>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Criteria> {
        let mut filters = Vec::new();
        let mut string = string.cloned();
        // `attrs` that isn't a dict is a class filter, as in Beautiful Soup.
        let attrs = match attrs.filter(|a| !a.is_none()) {
            Some(a) => match a.cast::<PyDict>() {
                Ok(d) => Some(d.clone()),
                Err(_) => {
                    filters.push(("class".to_string(), Filter::from_py(Some(a))?));
                    None
                }
            },
            None => None,
        };
        for dict in [attrs.as_ref(), kwargs].into_iter().flatten() {
            for (k, v) in dict.iter() {
                let key = k.extract::<String>()?;
                // `text` is Beautiful Soup's old name for `string`.
                if key == "text" && string.is_none() {
                    string = Some(v);
                    continue;
                }
                // None for an attribute means it must be absent.
                let filter = if v.is_none() {
                    Filter::Present(false)
                } else {
                    Filter::from_py(Some(&v))?
                };
                filters.push((key, filter));
            }
        }
        if let Some(c) = class_ {
            filters.push(("class".to_string(), Filter::from_py(Some(c))?));
        }
        Ok(Criteria {
            name: Filter::from_py(name)?,
            attrs: filters,
            string: Filter::from_py(string.as_ref())?,
        })
    }

    /// Nodes in `direction` from this one that pass `criteria`, at most
    /// `limit` of them, nearest first.
    fn search(
        &self,
        py: Python<'_>,
        criteria: &Criteria,
        direction: Direction,
        limit: Option<usize>,
    ) -> PyResult<Vec<Node>> {
        let me = self.get();
        let candidates: Box<dyn Iterator<Item = netweir_dom::Node<'_>>> = match direction {
            Direction::Descendants => Box::new(me.descendants()),
            Direction::Children => Box::new(me.children()),
            Direction::Parents => Box::new(std::iter::successors(me.parent(), |p| p.parent())),
            Direction::NextSiblings => Box::new(std::iter::successors(me.next_sibling(), |s| {
                s.next_sibling()
            })),
            Direction::PreviousSiblings => {
                Box::new(std::iter::successors(me.prev_sibling(), |s| {
                    s.prev_sibling()
                }))
            }
            Direction::Following => Box::new(std::iter::successors(me.next_in_order(), |n| {
                n.next_in_order()
            })),
            Direction::Preceding => Box::new(std::iter::successors(me.prev_in_order(), |n| {
                n.prev_in_order()
            })),
        };
        let as_python = |n: netweir_dom::Node<'_>| -> PyResult<Py<PyAny>> {
            Ok(Py::new(py, self.sibling(n.id()))?.into_any())
        };
        let mut found = Vec::new();
        for node in candidates {
            if limit.is_some_and(|l| found.len() >= l) {
                break;
            }
            if criteria.matches(py, node, &as_python)? {
                found.push(self.sibling(node.id()));
            }
        }
        Ok(found)
    }
}

macro_rules! finders {
    ($( $one:ident, $all:ident, $dir:expr; )*) => {
        #[pymethods]
        impl Node {
            $(
                #[pyo3(signature = (name=None, attrs=None, string=None, class_=None, **kwargs))]
                fn $one(
                    &self,
                    py: Python<'_>,
                    name: Option<&Bound<'_, PyAny>>,
                    attrs: Option<&Bound<'_, PyAny>>,
                    string: Option<&Bound<'_, PyAny>>,
                    class_: Option<&Bound<'_, PyAny>>,
                    kwargs: Option<&Bound<'_, PyDict>>,
                ) -> PyResult<Option<Node>> {
                    let criteria = Node::criteria(name, attrs, string, class_, kwargs)?;
                    Ok(self.search(py, &criteria, $dir, Some(1))?.into_iter().next())
                }

                #[pyo3(signature = (name=None, attrs=None, string=None, limit=None, class_=None, **kwargs))]
                #[allow(clippy::too_many_arguments)]
                fn $all(
                    &self,
                    py: Python<'_>,
                    name: Option<&Bound<'_, PyAny>>,
                    attrs: Option<&Bound<'_, PyAny>>,
                    string: Option<&Bound<'_, PyAny>>,
                    limit: Option<i64>,
                    class_: Option<&Bound<'_, PyAny>>,
                    kwargs: Option<&Bound<'_, PyDict>>,
                ) -> PyResult<Vec<Node>> {
                    let criteria = Node::criteria(name, attrs, string, class_, kwargs)?;
                    self.search(py, &criteria, $dir, bs4_limit(limit)?)
                }
            )*
        }
    };
}

finders! {
    find_parent, find_parents, Direction::Parents;
    find_next_sibling, find_next_siblings, Direction::NextSiblings;
    find_previous_sibling, find_previous_siblings, Direction::PreviousSiblings;
    find_next, find_all_next, Direction::Following;
    find_previous, find_all_previous, Direction::Preceding;
}

#[pymethods]
impl Node {
    // --- parsel-style queries -----------------------------------------

    /// Runs a CSS query. With `track`, the element is followed by that
    /// name through redesigns (see netweir.track).
    #[pyo3(signature = (query, track=None))]
    fn css(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        query: &str,
        track: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        if let Some(name) = track {
            return tracked_through_python(py, slf, "css", query, name);
        }
        let me = slf.get();
        let entries = css_entries(py, &me.doc, vec![me.id], query)?;
        Ok(Py::new(py, Selection::of(me.doc.clone(), entries, me.namespaces()))?.into_any())
    }

    /// The query's results, following the element as `name` on `site`
    /// through redesigns: when the query finds it, its fingerprint is
    /// saved in `store`; when it doesn't, the most similar element is
    /// used if it scores at least `threshold`. Used by css(track=...) and
    /// xpath(track=...).
    #[allow(clippy::too_many_arguments)]
    fn tracked(
        &self,
        py: Python<'_>,
        kind: &str,
        query: &str,
        name: &str,
        store: &TrackStore,
        site: &str,
        threshold: f64,
    ) -> PyResult<Selection> {
        let tracker = Tracker::new(name, kind, query).map_err(|e| match kind {
            "css" => SelectorError::new_err(e),
            _ => XPathError::new_err(e),
        })?;
        let (doc, id, tracks) = (self.doc.clone(), self.id, store.tracks());
        let site = site.to_string();
        let (entries, tracking) = py
            .detach(move || {
                let _reading = doc.read();
                let ctx = TrackContext {
                    tracks: &tracks,
                    site: &site,
                    threshold,
                };
                // SAFETY: the id came from this Node's document.
                tracker
                    .run(unsafe { doc.node(id) }, &ctx)
                    .map(|(hits, tracking)| (entries_of(hits), tracking))
            })
            .map_err(XPathError::new_err)?;
        let (relocated, score) = match tracking {
            Tracking::Matched => (false, Some(1.0)),
            Tracking::Relocated(s) => (true, Some(s)),
            // How close the nearest candidate came.
            Tracking::Lost(best) => (false, Some(best)),
            Tracking::Unknown => (false, None),
        };
        Ok(Selection {
            doc: self.doc.clone(),
            entries,
            relocated,
            score,
            namespaces: Mutex::new(self.namespaces()),
        })
    }

    /// Runs an XPath 1.0 query with this node as the context. Keyword
    /// arguments bind `$variables`; `namespaces` (a dict of prefix to
    /// URI) binds prefixes for this query, besides those registered with
    /// `register_namespace`; `track` follows the element through
    /// redesigns, as css(track=...) does.
    #[pyo3(signature = (query, track=None, namespaces=None, **vars))]
    fn xpath(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        query: &str,
        track: Option<&str>,
        namespaces: Option<&Bound<'_, PyAny>>,
        vars: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        if let Some(name) = track {
            if vars.is_some_and(|v| !v.is_empty()) {
                return Err(PyTypeError::new_err(
                    "track= can't be combined with $variables",
                ));
            }
            if namespaces.is_some_and(|n| !n.is_none()) {
                return Err(PyTypeError::new_err(
                    "track= can't be combined with namespaces=; register them with register_namespace()",
                ));
            }
            return tracked_through_python(py, slf, "xpath", query, name);
        }
        let me = slf.get();
        let inherited = me.namespaces();
        let all = query_namespaces(&inherited, namespaces)?;
        let entries = xpath_entries(py, &me.doc, vec![me.id], query, vars, all)?;
        Ok(Py::new(py, Selection::of(me.doc.clone(), entries, inherited))?.into_any())
    }

    /// Binds `prefix` to the namespace `uri` for XPath queries from this
    /// node, and from the nodes and selections they give, as parsel's
    /// `register_namespace` does.
    fn register_namespace(&self, prefix: &str, uri: &str) -> PyResult<()> {
        check_prefix(prefix)?;
        self.bind(prefix.to_string(), uri.to_string());
        Ok(())
    }

    /// Takes every element and attribute of the document out of its
    /// namespace, so plain names match them: `//entry` rather than
    /// `//atom:entry`. Changes the whole document, for every node of it,
    /// as parsel's `remove_namespaces` does. Nothing happens to HTML.
    fn remove_namespaces(&self, py: Python<'_>) -> PyResult<()> {
        // SAFETY: no ids are involved; other threads read through `Held`.
        self.edit(py, |doc| unsafe { doc.remove_namespaces() })
    }

    /// "xml" for a node of an XML document, "html" otherwise, as parsel's
    /// `Selector.type`.
    #[getter(r#type)]
    fn doc_type(&self) -> &'static str {
        if self.doc.is_xml() { "xml" } else { "html" }
    }

    // --- Beautiful Soup style search ----------------------------------

    /// The first descendant (or child, with recursive=False) that matches.
    #[pyo3(signature = (name=None, attrs=None, recursive=true, string=None, class_=None, **kwargs))]
    #[allow(clippy::too_many_arguments)]
    fn find(
        &self,
        py: Python<'_>,
        name: Option<&Bound<'_, PyAny>>,
        attrs: Option<&Bound<'_, PyAny>>,
        recursive: bool,
        string: Option<&Bound<'_, PyAny>>,
        class_: Option<&Bound<'_, PyAny>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Option<Node>> {
        let criteria = Node::criteria(name, attrs, string, class_, kwargs)?;
        let direction = if recursive {
            Direction::Descendants
        } else {
            Direction::Children
        };
        Ok(self
            .search(py, &criteria, direction, Some(1))?
            .into_iter()
            .next())
    }

    /// Every descendant (or child, with recursive=False) that matches.
    #[pyo3(signature = (name=None, attrs=None, recursive=true, string=None, limit=None, class_=None, **kwargs))]
    #[allow(clippy::too_many_arguments)]
    fn find_all(
        &self,
        py: Python<'_>,
        name: Option<&Bound<'_, PyAny>>,
        attrs: Option<&Bound<'_, PyAny>>,
        recursive: bool,
        string: Option<&Bound<'_, PyAny>>,
        limit: Option<i64>,
        class_: Option<&Bound<'_, PyAny>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<Node>> {
        let criteria = Node::criteria(name, attrs, string, class_, kwargs)?;
        let direction = if recursive {
            Direction::Descendants
        } else {
            Direction::Children
        };
        self.search(py, &criteria, direction, bs4_limit(limit)?)
    }

    /// Elements matching a CSS selector, as a list.
    fn select(&self, py: Python<'_>, css: &str) -> PyResult<Vec<Node>> {
        let entries = css_entries(py, &self.doc, vec![self.id], css)?;
        Ok(entries
            .into_iter()
            .filter_map(|e| match e {
                Entry::Node(id) => Some(self.sibling(id)),
                Entry::Value(_) => None,
            })
            .collect())
    }

    fn select_one(&self, py: Python<'_>, css: &str) -> PyResult<Option<Node>> {
        Ok(self.select(py, css)?.into_iter().next())
    }

    // --- what the node is ---------------------------------------------

    /// "element", "text", "comment", "document" or "doctype".
    #[getter]
    fn kind(&self) -> &'static str {
        kind_name(self.get().kind())
    }

    /// Lowercase tag name; None for anything but an element.
    #[getter]
    fn tag(&self) -> Option<&str> {
        self.get().tag()
    }

    /// Beautiful Soup's name for `tag`.
    #[getter]
    fn name(&self) -> Option<&str> {
        self.get().tag()
    }

    /// The text below this node, without the contents of scripts, styles
    /// and templates (as Beautiful Soup's `.text`).
    #[getter(text)]
    fn all_text(&self, py: Python<'_>) -> String {
        let doc = self.doc.clone();
        let id = self.id;
        // SAFETY: `id` belongs to `doc`.
        py.detach(move || {
            let _reading = doc.read();
            unsafe { doc.node(id) }.readable_text_pieces().concat()
        })
    }

    /// The structured data the page carries about itself: a dict with
    /// "json_ld", "microdata", "opengraph", "twitter" and "dublin_core".
    /// Relative URLs in microdata are made absolute against `base_url`.
    #[pyo3(signature = (base_url=None))]
    fn metadata(&self, py: Python<'_>, base_url: Option<&str>) -> PyResult<Py<PyAny>> {
        let doc = self.doc.clone();
        let id = self.id;
        let base = base_url.and_then(|b| url::Url::parse(b).ok());
        // SAFETY: `id` belongs to `doc`.
        let value = py.detach(move || {
            let _reading = doc.read();
            netweir_dom::metadata::metadata(unsafe { doc.node(id) }, base.as_ref())
        });
        crate::browser::to_python(py, &value)
    }

    /// Data kept in the `<script>` elements at and below this node. With
    /// `name`, the value assigned to it (`window.name = {...}`,
    /// `var name = [...]`, `"name": {...}`, or a JSON script with that id),
    /// or None. Without, a list of every object and array in every script.
    #[pyo3(signature = (name=None))]
    fn js_data(&self, py: Python<'_>, name: Option<String>) -> PyResult<Py<PyAny>> {
        let doc = self.doc.clone();
        let id = self.id;
        let wanted = name.clone();
        // SAFETY: `id` belongs to `doc`.
        let found = py.detach(move || {
            let _reading = doc.read();
            netweir_dom::js::scripts(unsafe { doc.node(id) }, wanted.as_deref())
        });
        match (name, found.first()) {
            (None, _) => crate::helpers::list(py, &found),
            (Some(_), Some(value)) => crate::helpers::to_python(py, value),
            (Some(_), None) => Ok(py.None()),
        }
    }

    /// A short CSS selector that finds this element again, and only it,
    /// in this document; None for anything but an element.
    fn css_path(&self, py: Python<'_>) -> Option<String> {
        let (doc, id) = (self.doc.clone(), self.id);
        py.detach(move || {
            let _reading = doc.read();
            // SAFETY: `id` belongs to `doc`.
            netweir_dom::css_path::css_path(unsafe { doc.node(id) })
        })
    }

    /// The element holding the main content at and below this node (an
    /// article, a post, a product's description), or None when no part of
    /// it stands out, as on a listing.
    fn main_content(&self, py: Python<'_>) -> Option<Node> {
        let (doc, id) = (self.doc.clone(), self.id);
        // SAFETY: `id` belongs to `doc`.
        let found = py.detach(move || {
            let _reading = doc.read();
            netweir_dom::content::main_content(unsafe { doc.node(id) }).map(|m| m.node.id())
        });
        found.map(|id| self.sibling(id))
    }

    /// The node as Markdown, links and images absolute against
    /// `base_url`. With main_only=True, only the main content without its
    /// boilerplate, or None when there's no main content.
    #[pyo3(signature = (base_url=None, main_only=false))]
    fn markdown(&self, py: Python<'_>, base_url: Option<&str>, main_only: bool) -> Option<String> {
        let (doc, id) = (self.doc.clone(), self.id);
        let base = base_url.and_then(|b| url::Url::parse(b).ok());
        py.detach(move || {
            let _reading = doc.read();
            // SAFETY: `id` belongs to `doc`.
            let node = unsafe { doc.node(id) };
            match main_only {
                true => netweir_dom::content::main_content(node).map(|m| m.markdown(base.as_ref())),
                false => Some(netweir_dom::markdown::markdown(node, base.as_ref())),
            }
        })
    }

    /// What the page holds: "product", "product-list", "job-posting",
    /// "job-posting-navigation", "article", "article-list",
    /// "forum-thread", or "other" when it looks like none of them. `url` is
    /// the page's, which its links are relative to.
    #[pyo3(signature = (url=None))]
    fn page_type(&self, py: Python<'_>, url: Option<&str>) -> &'static str {
        let (doc, id) = (self.doc.clone(), self.id);
        let page = url.and_then(|u| url::Url::parse(u).ok());
        py.detach(move || {
            let _reading = doc.read();
            // SAFETY: `id` belongs to `doc`.
            let node = unsafe { doc.node(id) };
            let now = netweir_dom::date::now_utc();
            let (found, _) = netweir_dom::auto::page_type(node, page.as_ref(), None, now);
            found.map_or("other", netweir_dom::auto::Schema::name)
        })
    }

    /// The page's main repeated region (table rows, cards, list entries)
    /// as a netweir.Records, or None when nothing repeats. `url` is the
    /// page's, which links and images are made absolute against.
    #[pyo3(signature = (url=None))]
    fn records(&self, py: Python<'_>, url: Option<&str>) -> PyResult<Py<PyAny>> {
        let (doc, id) = (self.doc.clone(), self.id);
        let page = url.and_then(|u| url::Url::parse(u).ok());
        let found = py.detach(move || {
            let _reading = doc.read();
            // SAFETY: `id` belongs to `doc`.
            netweir_dom::records::records(unsafe { doc.node(id) }, page.as_ref())
        });
        let Some(r) = found else {
            return Ok(py.None());
        };
        let columns: Vec<(String, String)> = r
            .columns
            .into_iter()
            .map(|c| (c.name, c.selector))
            .collect();
        let rows = crate::browser::to_python(
            py,
            &serde_json::Value::Array(r.rows.into_iter().map(serde_json::Value::Object).collect()),
        )?;
        Ok(py
            .import("netweir._helpers")?
            .getattr("_records")?
            .call1((r.selector, columns, rows))?
            .unbind())
    }

    /// The text a reader sees, a blank line between blocks and a line per
    /// list item and table row. With main_only=True, only the main
    /// content without its boilerplate, or None when there's no main
    /// content.
    #[pyo3(signature = (main_only=false))]
    fn clean_text(&self, py: Python<'_>, main_only: bool) -> Option<String> {
        let (doc, id) = (self.doc.clone(), self.id);
        py.detach(move || {
            let _reading = doc.read();
            // SAFETY: `id` belongs to `doc`.
            let node = unsafe { doc.node(id) };
            match main_only {
                true => netweir_dom::content::main_content(node).map(|m| m.clean_text()),
                false => Some(netweir_dom::markdown::clean_text(node)),
            }
        })
    }

    /// The article on the page: its main content's text and HTML, and the
    /// title, byline, date, lead image and language the page gives for
    /// it, as a netweir.Article.
    #[pyo3(signature = (base_url=None))]
    fn article(&self, py: Python<'_>, base_url: Option<&str>) -> PyResult<Py<PyAny>> {
        let (doc, id) = (self.doc.clone(), self.id);
        let base = base_url.and_then(|b| url::Url::parse(b).ok());
        // SAFETY: `id` belongs to `doc`.
        let a = py.detach(move || {
            let _reading = doc.read();
            netweir_dom::content::article(unsafe { doc.node(id) }, base.as_ref())
        });
        // The date is read, and the dataclass made, in Python.
        Ok(py
            .import("netweir._helpers")?
            .getattr("_article")?
            .call1((
                a.title,
                a.byline,
                a.published,
                a.image,
                a.language,
                a.text,
                a.html,
            ))?
            .unbind())
    }

    /// The node as HTML.
    #[getter]
    fn html(&self, py: Python<'_>) -> String {
        let doc = self.doc.clone();
        let id = self.id;
        // SAFETY: `id` belongs to `doc`.
        py.detach(move || {
            let _reading = doc.read();
            unsafe { doc.node(id) }.html()
        })
    }

    /// The text of a node whose only content is one piece of text, else None.
    #[getter]
    fn string(&self) -> Option<String> {
        single_string(*self.get())
    }

    /// Text below this node (scripts, styles and templates left out), pieces
    /// joined by `separator`; with strip=True each piece is trimmed and empty
    /// ones are dropped.
    #[pyo3(name = "get_text", signature = (separator="", strip=false))]
    fn joined_text(&self, separator: &str, strip: bool) -> String {
        let pieces: Vec<&str> = self
            .get()
            .readable_text_pieces()
            .into_iter()
            .map(|t| if strip { t.trim() } else { t })
            .filter(|t| !strip || !t.is_empty())
            .collect();
        pieces.join(separator)
    }

    // --- attributes -----------------------------------------------------

    #[getter]
    fn attrs<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (k, v) in self.get().attrs() {
            dict.set_item(k, v)?;
        }
        Ok(dict)
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.get().attr(name)
    }

    /// An attribute's value, or `default` if the element doesn't have it.
    #[pyo3(name = "get", signature = (name, default=None))]
    fn get_attr(&self, name: &str, default: Option<Py<PyAny>>, py: Python<'_>) -> Py<PyAny> {
        match self.get().attr(name) {
            Some(v) => PyString::new(py, v).into_any().unbind(),
            None => default.unwrap_or_else(|| py.None()),
        }
    }

    fn __getitem__(&self, name: &str) -> PyResult<&str> {
        self.get()
            .attr(name)
            .ok_or_else(|| PyKeyError::new_err(name.to_string()))
    }

    // --- navigation -----------------------------------------------------

    #[getter]
    fn parent(&self) -> Option<Node> {
        self.wrap(self.get().parent())
    }

    /// Ancestors, nearest first.
    #[getter]
    fn parents(&self) -> Vec<Node> {
        self.wrap_all(std::iter::successors(self.get().parent(), |p| p.parent()))
    }

    /// Child nodes of every kind, in order.
    #[getter]
    fn children(&self) -> Vec<Node> {
        self.wrap_all(self.get().children())
    }

    /// Beautiful Soup's name for `children`.
    #[getter]
    fn contents(&self) -> Vec<Node> {
        self.children()
    }

    /// Every node below this one, in document order.
    #[getter]
    fn descendants(&self) -> Vec<Node> {
        self.wrap_all(self.get().descendants())
    }

    #[getter]
    fn next_sibling(&self) -> Option<Node> {
        self.wrap(self.get().next_sibling())
    }

    #[getter]
    fn previous_sibling(&self) -> Option<Node> {
        self.wrap(self.get().prev_sibling())
    }

    #[getter]
    fn next_siblings(&self) -> Vec<Node> {
        self.wrap_all(std::iter::successors(self.get().next_sibling(), |s| {
            s.next_sibling()
        }))
    }

    #[getter]
    fn previous_siblings(&self) -> Vec<Node> {
        self.wrap_all(std::iter::successors(self.get().prev_sibling(), |s| {
            s.prev_sibling()
        }))
    }

    /// The next node in document order, of any kind.
    #[getter]
    fn next_element(&self) -> Option<Node> {
        self.wrap(self.get().next_in_order())
    }

    #[getter]
    fn previous_element(&self) -> Option<Node> {
        self.wrap(self.get().prev_in_order())
    }

    // --- elements like this one ----------------------------------------

    /// The other elements on the page that look like this one: same tag,
    /// same tags on the way up, and at least `threshold` of their
    /// attribute names and classes in common. In document order.
    #[pyo3(signature = (threshold=netweir_dom::similar::SIMILAR))]
    fn find_similar(&self, py: Python<'_>, threshold: f64) -> PyResult<Selection> {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(PyValueError::new_err(format!(
                "threshold must be between 0 and 1, not {threshold}"
            )));
        }
        let (doc, id) = (self.doc.clone(), self.id);
        let entries = py.detach(move || {
            let _reading = doc.read();
            // SAFETY: `id` belongs to `doc`.
            let found = unsafe { doc.node(id) }.similar(threshold);
            found.into_iter().map(|n| Entry::Node(n.id())).collect()
        });
        Ok(Selection::of(self.doc.clone(), entries, self.namespaces()))
    }

    /// The node's text read as JSON (a `<script type="application/json">`,
    /// say), searched with a JMESPath expression.
    fn jmespath(&self, py: Python<'_>, expression: &str) -> PyResult<Py<PyAny>> {
        let text = self.get().text();
        Ok(py
            .import("netweir._jmespath")?
            .getattr("search_text")?
            .call1((expression, text))?
            .unbind())
    }

    // --- editing ----------------------------------------------------------

    /// Removes the node from the page. A handle to it, or to anything in
    /// it, still reads what was removed.
    fn decompose(&self, py: Python<'_>) -> PyResult<()> {
        let id = self.id;
        // SAFETY: `id` belongs to the document the edit runs on.
        self.edit(py, move |doc| unsafe { doc.remove(id) })
    }

    /// Removes the node from the page and returns it, to keep or put back
    /// elsewhere.
    fn extract(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<Node>> {
        slf.get().decompose(py)?;
        Ok(slf.clone().unbind())
    }

    /// Puts the nodes or strings given where this node is, and returns
    /// this node, now removed.
    #[pyo3(signature = (*items))]
    fn replace_with(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        items: Vec<Bound<'_, PyAny>>,
    ) -> PyResult<Py<Node>> {
        let me = slf.get();
        let with = me.insertable(&items)?;
        let id = me.id;
        // SAFETY: every id belongs to the document the edit runs on.
        me.edit(py, move |doc| unsafe { doc.replace(id, &contents(&with)) })?;
        Ok(slf.clone().unbind())
    }

    /// Replaces the node with what's in it, and returns it, now removed
    /// and empty.
    #[pyo3(name = "unwrap")]
    fn unwrap_node(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<Node>> {
        let id = slf.get().id;
        // SAFETY: `id` belongs to the document the edit runs on.
        slf.get().edit(py, move |doc| unsafe { doc.unwrap(id) })?;
        Ok(slf.clone().unbind())
    }

    /// Adds a node or a string as the last child. A node already on the
    /// page moves.
    fn append(&self, py: Python<'_>, child: Bound<'_, PyAny>) -> PyResult<()> {
        let items = self.insertable(&[child])?;
        let id = self.id;
        // SAFETY: every id belongs to the document the edit runs on.
        self.edit(py, move |doc| unsafe {
            doc.insert(id, None, &contents(&items))
        })
    }

    /// Adds a node or a string at `position` among the children (text
    /// included, as in `contents`); past the end appends, negative counts
    /// from the end.
    fn insert(&self, py: Python<'_>, position: isize, child: Bound<'_, PyAny>) -> PyResult<()> {
        let items = self.insertable(&[child])?;
        let id = self.id;
        // The position is found inside the edit, so an edit on another
        // thread can't move the children in between.
        // SAFETY: every id belongs to the document the edit runs on.
        self.edit(py, move |doc| unsafe {
            doc.insert(id, Some(position), &contents(&items))
        })
    }

    /// A new element for this page, not yet placed: add it with append(),
    /// insert() or replace_with(). Attributes come from `attrs` and
    /// keyword arguments (`class_` for class); `string` is its text.
    #[pyo3(signature = (name, attrs=None, string=None, **kwattrs))]
    fn new_tag(
        &self,
        py: Python<'_>,
        name: &str,
        attrs: Option<&Bound<'_, PyDict>>,
        string: Option<String>,
        kwattrs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Node> {
        // Values as Beautiful Soup writes them: a list joined with spaces,
        // None as an attribute with no value, anything else as its str().
        // Keyword names lose one trailing "_" (class_, for_), as find_all's
        // do; names in `attrs` are taken as they are.
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (dict, keyword) in [(attrs, false), (kwattrs, true)] {
            for (k, v) in dict.into_iter().flat_map(|d| d.iter()) {
                let mut k: String = k.extract()?;
                if keyword && k.len() > 1 && k.ends_with('_') {
                    k.pop();
                }
                let value = if v.is_none() {
                    String::new()
                } else if v.is_instance_of::<PyList>() || v.is_instance_of::<PyTuple>() {
                    let parts: Vec<String> = v
                        .try_iter()?
                        .map(|p| Ok(p?.str()?.to_string()))
                        .collect::<PyResult<_>>()?;
                    parts.join(" ")
                } else {
                    v.str()?.to_string()
                };
                pairs.push((k, value));
            }
        }
        let tag = name.to_string();
        let el = self.edit(py, move |doc| {
            let attrs: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            // SAFETY: the new node belongs to the document the edit runs on.
            let el = unsafe { doc.create_element(&tag, &attrs)? };
            if let Some(text) = string {
                unsafe { doc.set_text(el, &text)? };
            }
            Ok(el)
        })?;
        Ok(self.sibling(el))
    }

    /// Replaces everything in the element with one piece of text.
    #[setter(string)]
    fn set_string(&self, py: Python<'_>, text: String) -> PyResult<()> {
        let id = self.id;
        // SAFETY: `id` belongs to the document the edit runs on.
        self.edit(py, move |doc| unsafe { doc.set_text(id, &text) })
    }

    /// The node as indented HTML, a tag or piece of text to a line.
    fn prettify(&self, py: Python<'_>) -> String {
        let (doc, id) = (self.doc.clone(), self.id);
        py.detach(move || {
            let _reading = doc.read();
            // SAFETY: `id` belongs to `doc`.
            unsafe { doc.node(id) }.pretty()
        })
    }

    // --- identity -------------------------------------------------------

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .cast::<Node>()
            .is_ok_and(|o| Arc::ptr_eq(&self.doc, &o.get().doc) && self.id == o.get().id)
    }

    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (Arc::as_ptr(&self.doc) as usize).hash(&mut h);
        self.id.hash(&mut h);
        h.finish()
    }

    /// An element as HTML; text and comments as their text, as Beautiful
    /// Soup's `str()` gives them.
    fn __str__(&self) -> String {
        let n = self.get();
        match n.kind() {
            NodeKind::Comment => n.data().unwrap_or_default().to_string(),
            _ => node_string(*n),
        }
    }

    fn __repr__(&self) -> String {
        let n = self.get();
        match (n.kind(), n.tag()) {
            (NodeKind::Element, Some(tag)) => format!("<Node {tag}>"),
            (NodeKind::Text, _) => format!("<Node text {:?}>", n.data().unwrap_or("")),
            (kind, _) => format!("<Node {}>", kind_name(kind)),
        }
    }
}

impl Selection {
    /// The entry as `get()` gives it. The caller holds a `Reading`.
    fn string_of(&self, entry: &Entry) -> String {
        match entry {
            // SAFETY: ids in a Selection all come from its `doc`.
            Entry::Node(id) => node_string(unsafe { self.doc.node(*id) }),
            Entry::Value(v) => v.clone(),
        }
    }

    fn strings(&self) -> Vec<String> {
        let _reading = self.doc.read();
        self.entries.iter().map(|e| self.string_of(e)).collect()
    }

    /// Ids of the entries a query can start from; strings and text can't.
    fn scopes(&self, what: &str) -> PyResult<Vec<NodeId>> {
        let _reading = self.doc.read();
        self.entries
            .iter()
            .map(|e| match e {
                // SAFETY: ids in a Selection all come from its `doc`.
                Entry::Node(id) if unsafe { self.doc.node(*id) }.kind() != NodeKind::Text => {
                    Ok(*id)
                }
                _ => Err(PyTypeError::new_err(format!(
                    "{what}() runs on elements; this selection holds text or attribute values"
                ))),
            })
            .collect()
    }

    fn item(&self, py: Python<'_>, entry: &Entry) -> PyResult<Py<PyAny>> {
        let reading = self.doc.read();
        Ok(match entry {
            // SAFETY: ids in a Selection all come from its `doc`.
            Entry::Node(id) if unsafe { self.doc.node(*id) }.kind() != NodeKind::Text => {
                Py::new(py, Node::with(self.doc.clone(), *id, self.ns()))?.into_any()
            }
            other => {
                let text = self.string_of(other);
                drop(reading);
                PyString::new(py, &text).into_any().unbind()
            }
        })
    }
}

#[pymethods]
impl Selection {
    /// The first result as a string, or `default` if there are none.
    #[pyo3(signature = (default=None))]
    fn get(&self, py: Python<'_>, default: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        match self.entries.first() {
            Some(e) => {
                let text = {
                    let _reading = self.doc.read();
                    self.string_of(e)
                };
                Ok(PyString::new(py, &text).into_any().unbind())
            }
            None => Ok(default.unwrap_or_else(|| py.None())),
        }
    }

    /// Every result as a string.
    fn getall(&self, py: Python<'_>) -> Vec<String> {
        // Releasing the GIL costs more than serialising a few nodes.
        if self.entries.len() < 16 {
            return self.strings();
        }
        py.detach(|| self.strings())
    }

    /// parsel's older name for `getall`.
    fn extract(&self, py: Python<'_>) -> Vec<String> {
        self.getall(py)
    }

    /// parsel's older name for `get`.
    #[pyo3(signature = (default=None))]
    fn extract_first(&self, py: Python<'_>, default: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        self.get(py, default)
    }

    /// Runs `query` below every element in this selection.
    fn css(&self, py: Python<'_>, query: &str) -> PyResult<Selection> {
        let entries = css_entries(py, &self.doc, self.scopes("css")?, query)?;
        Ok(Selection::of(self.doc.clone(), entries, self.ns()))
    }

    /// Binds `prefix` to `uri` for queries from this selection, and from
    /// the nodes in it, as on a parsel Selector (XMLFeedSpider hands each
    /// node to parse_node as a selection of one).
    fn register_namespace(&self, prefix: &str, uri: &str) -> PyResult<()> {
        check_prefix(prefix)?;
        bind(&self.namespaces, prefix.to_string(), uri.to_string());
        Ok(())
    }

    /// Takes every element and attribute of the document out of its
    /// namespace, as `Node.remove_namespaces()` does.
    fn remove_namespaces(&self, py: Python<'_>) -> PyResult<()> {
        let doc = self.doc.clone();
        // SAFETY: no ids are involved; other threads read through `Held`.
        py.detach(move || unsafe { doc.remove_namespaces() })
            .map_err(edit_error)
    }

    /// Runs an XPath query from every element in this selection, with the
    /// namespaces its nodes have and any given in `namespaces`.
    #[pyo3(signature = (query, namespaces=None, **vars))]
    fn xpath(
        &self,
        py: Python<'_>,
        query: &str,
        namespaces: Option<&Bound<'_, PyAny>>,
        vars: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Selection> {
        let all = query_namespaces(&self.ns(), namespaces)?;
        let entries = xpath_entries(py, &self.doc, self.scopes("xpath")?, query, vars, all)?;
        Ok(Selection::of(self.doc.clone(), entries, self.ns()))
    }

    /// Every match of `pattern` in every result, as parsel does it: whole
    /// matches, the groups when the pattern has them, or only the first
    /// match's `extract` group. Entities other than `&lt;` and `&amp;` are
    /// decoded unless `replace_entities` is false.
    #[pyo3(signature = (pattern, replace_entities=true))]
    fn re<'py>(
        &self,
        py: Python<'py>,
        pattern: &Bound<'py, PyAny>,
        replace_entities: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        py.import("netweir._regex")?.getattr("extract")?.call1((
            pattern,
            self.getall(py),
            replace_entities,
        ))
    }

    /// The first `re()` match, or `default`.
    #[pyo3(signature = (pattern, default=None, replace_entities=true))]
    fn re_first(
        &self,
        py: Python<'_>,
        pattern: &Bound<'_, PyAny>,
        default: Option<Py<PyAny>>,
        replace_entities: bool,
    ) -> PyResult<Py<PyAny>> {
        let all = self.re(py, pattern, replace_entities)?;
        match all.try_iter()?.next() {
            Some(first) => Ok(first?.unbind()),
            None => Ok(default.unwrap_or_else(|| py.None())),
        }
    }

    /// Each result read as JSON (an element's text, or the string) and
    /// searched with a JMESPath expression: a list of what each gives,
    /// leaving out results that give nothing (None).
    fn jmespath(&self, py: Python<'_>, expression: &str) -> PyResult<Py<PyAny>> {
        let texts: Vec<String> = {
            let _reading = self.doc.read();
            self.entries
                .iter()
                .map(|e| match e {
                    // SAFETY: ids in a Selection all come from its `doc`.
                    Entry::Node(id) => unsafe { self.doc.node(*id) }.text(),
                    Entry::Value(v) => v.clone(),
                })
                .collect()
        };
        Ok(py
            .import("netweir._jmespath")?
            .getattr("search_texts")?
            .call1((expression, texts))?
            .unbind())
    }

    /// With track=: whether the selector missed and these results were
    /// found by similarity instead.
    #[getter]
    fn relocated(&self) -> bool {
        self.relocated
    }

    /// With track=: 1.0 when the selector matched, the similarity (0 to 1)
    /// when the results were relocated, the nearest candidate's score when
    /// nothing was similar enough, and None when there was nothing to
    /// look for.
    #[getter]
    fn score(&self) -> Option<f64> {
        self.score
    }

    /// The first element's attributes; empty if there is none.
    #[getter]
    fn attrib<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        if let Some(Entry::Node(id)) = self.entries.first() {
            let _reading = self.doc.read();
            // SAFETY: ids in a Selection all come from its `doc`.
            for (k, v) in unsafe { self.doc.node(*id) }.attrs() {
                dict.set_item(k, v)?;
            }
        }
        Ok(dict)
    }

    fn __len__(&self) -> usize {
        self.entries.len()
    }

    fn __getitem__(&self, py: Python<'_>, index: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        if let Ok(slice) = index.cast::<PySlice>() {
            let ix = slice.indices(self.entries.len() as isize)?;
            let mut picked = Vec::new();
            let mut i = ix.start;
            for _ in 0..ix.slicelength {
                picked.push(self.entries[i as usize].clone());
                i += ix.step;
            }
            return Ok(Py::new(py, Selection::of(self.doc.clone(), picked, self.ns()))?.into_any());
        }
        let i: isize = index.extract()?;
        let len = self.entries.len() as isize;
        let at = if i < 0 { i + len } else { i };
        if at < 0 || at >= len {
            return Err(PyIndexError::new_err("selection index out of range"));
        }
        self.item(py, &self.entries[at as usize])
    }

    fn __repr__(&self) -> String {
        format!("<Selection of {}>", self.entries.len())
    }
}
