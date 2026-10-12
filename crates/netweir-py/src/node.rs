//! `Node` and `Selection`: the parsel-style query API (css, xpath, get,
//! getall, re) and the Beautiful Soup style one (find_all and navigation).

use std::sync::Arc;

use netweir_dom::extract::{TrackContext, Tracker, Tracking};
use netweir_dom::{Document, Hit, NodeId, NodeKind, Query, XPath, XValue};
use pyo3::exceptions::{PyIndexError, PyKeyError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PySlice, PyString};

use crate::filter::{Criteria, Filter, single_string};
use crate::rules::TrackStore;
use crate::{SelectorError, XPathError};

/// A node in a parsed document: an element, a piece of text, a comment, or
/// the document itself.
#[pyclass(frozen, module = "netweir")]
pub(crate) struct Node {
    doc: Arc<Document>,
    id: NodeId,
}

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
}

impl Node {
    pub(crate) fn root_of(doc: Arc<Document>) -> Node {
        let id = doc.root().id();
        Node { doc, id }
    }

    pub(crate) fn document(&self) -> &Arc<Document> {
        &self.doc
    }

    pub(crate) fn node_id(&self) -> NodeId {
        self.id
    }

    pub(crate) fn get(&self) -> netweir_dom::Node<'_> {
        // SAFETY: `id` was taken from a node of `doc`, which this Node keeps alive.
        unsafe { self.doc.node(self.id) }
    }

    fn sibling(&self, id: NodeId) -> Node {
        Node {
            doc: self.doc.clone(),
            id,
        }
    }

    fn wrap(&self, node: Option<netweir_dom::Node<'_>>) -> Option<Node> {
        node.map(|n| self.sibling(n.id()))
    }

    fn wrap_all<'a>(&self, nodes: impl Iterator<Item = netweir_dom::Node<'a>>) -> Vec<Node> {
        nodes.map(|n| self.sibling(n.id())).collect()
    }
}

fn kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Element => "element",
        NodeKind::Text => "text",
        NodeKind::Comment => "comment",
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
    let query = Query::new(css).map_err(|e| SelectorError::new_err(e.to_string()))?;
    let doc = doc.clone();
    Ok(py.detach(move || {
        let mut out = Vec::new();
        for id in scopes {
            // SAFETY: every scope id came from `doc`.
            out.extend(entries_of(query.run(unsafe { doc.node(id) })));
        }
        out
    }))
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

fn xpath_entries(
    py: Python<'_>,
    doc: &Arc<Document>,
    scopes: Vec<NodeId>,
    query: &str,
    vars: Option<&Bound<'_, PyDict>>,
) -> PyResult<Vec<Entry>> {
    let xpath = XPath::new(query).map_err(|e| XPathError::new_err(e.to_string()))?;
    let vars = xpath_vars(vars)?;
    let doc = doc.clone();
    py.detach(move || {
        let mut out = Vec::new();
        for id in scopes {
            // SAFETY: every scope id came from `doc`.
            let hits = xpath
                .run_with(unsafe { doc.node(id) }, &vars)
                .map_err(|e| e.to_string())?;
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
        Ok(Py::new(
            py,
            Selection {
                doc: me.doc.clone(),
                entries,
                relocated: false,
                score: None,
            },
        )?
        .into_any())
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
        })
    }

    /// Runs an XPath 1.0 query with this node as the context. Keyword
    /// arguments bind `$variables`; `track` follows the element through
    /// redesigns, as css(track=...) does.
    #[pyo3(signature = (query, track=None, **vars))]
    fn xpath(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        query: &str,
        track: Option<&str>,
        vars: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        if let Some(name) = track {
            if vars.is_some_and(|v| !v.is_empty()) {
                return Err(PyTypeError::new_err(
                    "track= can't be combined with $variables",
                ));
            }
            return tracked_through_python(py, slf, "xpath", query, name);
        }
        let me = slf.get();
        let entries = xpath_entries(py, &me.doc, vec![me.id], query, vars)?;
        Ok(Py::new(
            py,
            Selection {
                doc: me.doc.clone(),
                entries,
                relocated: false,
                score: None,
            },
        )?
        .into_any())
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
        py.detach(move || unsafe { doc.node(id) }.readable_text_pieces().concat())
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
        let found =
            py.detach(move || netweir_dom::js::scripts(unsafe { doc.node(id) }, wanted.as_deref()));
        match (name, found.first()) {
            (None, _) => crate::helpers::list(py, &found),
            (Some(_), Some(value)) => crate::helpers::to_python(py, value),
            (Some(_), None) => Ok(py.None()),
        }
    }

    /// The node as HTML.
    #[getter]
    fn html(&self, py: Python<'_>) -> String {
        let doc = self.doc.clone();
        let id = self.id;
        // SAFETY: `id` belongs to `doc`.
        py.detach(move || unsafe { doc.node(id) }.html())
    }

    /// The text of a node whose only content is one piece of text, else None.
    #[getter]
    fn string(&self) -> Option<String> {
        single_string(self.get())
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
            _ => node_string(n),
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
    fn string_of(&self, entry: &Entry) -> String {
        match entry {
            // SAFETY: ids in a Selection all come from its `doc`.
            Entry::Node(id) => node_string(unsafe { self.doc.node(*id) }),
            Entry::Value(v) => v.clone(),
        }
    }

    /// Ids of the entries a query can start from; strings and text can't.
    fn scopes(&self, what: &str) -> PyResult<Vec<NodeId>> {
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
        Ok(match entry {
            // SAFETY: ids in a Selection all come from its `doc`.
            Entry::Node(id) if unsafe { self.doc.node(*id) }.kind() != NodeKind::Text => Py::new(
                py,
                Node {
                    doc: self.doc.clone(),
                    id: *id,
                },
            )?
            .into_any(),
            other => PyString::new(py, &self.string_of(other))
                .into_any()
                .unbind(),
        })
    }
}

#[pymethods]
impl Selection {
    /// The first result as a string, or `default` if there are none.
    #[pyo3(signature = (default=None))]
    fn get(&self, py: Python<'_>, default: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        match self.entries.first() {
            Some(e) => Ok(PyString::new(py, &self.string_of(e)).into_any().unbind()),
            None => Ok(default.unwrap_or_else(|| py.None())),
        }
    }

    /// Every result as a string.
    fn getall(&self, py: Python<'_>) -> Vec<String> {
        // Releasing the GIL costs more than serialising a few nodes.
        if self.entries.len() < 16 {
            return self.entries.iter().map(|e| self.string_of(e)).collect();
        }
        py.detach(|| self.entries.iter().map(|e| self.string_of(e)).collect())
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
        Ok(Selection {
            doc: self.doc.clone(),
            entries,
            relocated: false,
            score: None,
        })
    }

    /// Runs an XPath query from every element in this selection.
    #[pyo3(signature = (query, **vars))]
    fn xpath(
        &self,
        py: Python<'_>,
        query: &str,
        vars: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Selection> {
        let entries = xpath_entries(py, &self.doc, self.scopes("xpath")?, query, vars)?;
        Ok(Selection {
            doc: self.doc.clone(),
            entries,
            relocated: false,
            score: None,
        })
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
            return Ok(Py::new(
                py,
                Selection {
                    doc: self.doc.clone(),
                    entries: picked,
                    relocated: false,
                    score: None,
                },
            )?
            .into_any());
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
