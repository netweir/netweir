use std::ffi::{c_char, c_int, c_void};
use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::ffi::{self, RawDocument, RawNode, str_at};
use crate::index::Index;

/// A parsed HTML document.
///
/// Built by lexbor following the HTML5 spec, so a page parses the way a
/// browser would parse it: missing `<html>`, `<head>`, `<body>` and `<tbody>`
/// elements are added, and misnested tags are repaired.
pub struct Document {
    raw: *mut RawDocument,
    /// Selector runs take this lock. lexbor does not promise that its matcher
    /// is safe with concurrent readers of one document, so we don't assume it.
    pub(crate) select_lock: Mutex<()>,
    /// A flat, document-order index of every node, built on first use.
    index: OnceLock<Index>,
}

// SAFETY: the tree is never modified after `parse` returns. Every other
// access is a read through `Node`, except selector matching, which is
// serialised by `select_lock`.
unsafe impl Send for Document {}
unsafe impl Sync for Document {}

impl Document {
    pub fn parse(html: &str) -> Document {
        Document::parse_within(html, Duration::MAX).expect("an unlimited budget cannot run out")
    }

    /// Like [`Document::parse`], but gives up once `budget` has passed.
    ///
    /// The HTML5 spec makes some inputs, such as thousands of unclosed
    /// `<div>`s, take time quadratic in their nesting depth. A 400 KB page of
    /// them takes seconds. Browsers cap the depth; lexbor does not. A crawler
    /// fetching pages it does not control should parse with a budget.
    pub fn parse_within(html: &str, budget: Duration) -> Result<Document, ParseTimeout> {
        // Known limit: the budget is checked between 16 KB chunks, so a parse can
        // overrun it by the time one chunk takes. It bounds time, not memory:
        // a crafted page of distinct formatting tags makes the HTML5 adoption
        // agency clone thousands of elements per paragraph (64 KB reached
        // 6.7 GB in a test). A node cap checked between chunks comes with the
        // crawler.
        parse_in_chunks(html, budget, 16 * 1024)
    }

    /// The document's index. Building it writes each node's `user` field
    /// once, inside the OnceLock, before any reader can see the result.
    pub(crate) fn index(&self) -> &Index {
        self.index
            .get_or_init(|| Index::build(unsafe { ffi::nw_root(self.raw) }))
    }

    fn order_of(&self, raw: *mut RawNode) -> u32 {
        self.index();
        unsafe { ffi::nw_order(raw) as u32 }
    }

    /// The document node. Its children are the doctype and `<html>`.
    pub fn root(&self) -> Node<'_> {
        Node {
            doc: self,
            raw: unsafe { ffi::nw_root(self.raw) },
        }
    }

    /// Rebuilds a node from an id returned by [`Node::id`].
    ///
    /// # Safety
    /// `id` must come from a node of this same document.
    pub unsafe fn node(&self, id: NodeId) -> Node<'_> {
        Node {
            doc: self,
            raw: id.0 as *mut RawNode,
        }
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        unsafe { ffi::nw_destroy(self.raw) }
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Document")
    }
}

fn parse_in_chunks(html: &str, budget: Duration, chunk: usize) -> Result<Document, ParseTimeout> {
    let start = Instant::now();
    let raw = unsafe { ffi::nw_chunk_begin() };
    // lexbor only fails when it cannot allocate. Rust aborts on allocation
    // failure, so netweir treats lexbor's the same way.
    assert!(!raw.is_null(), "lexbor failed to allocate a document");
    let doc = Document {
        raw,
        select_lock: Mutex::new(()),
        index: OnceLock::new(),
    };
    let mut rest = html;
    while !rest.is_empty() {
        if start.elapsed() > budget {
            return Err(ParseTimeout {
                budget,
                parsed: html.len() - rest.len(),
            });
        }
        let mut cut = rest.len().min(chunk);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (head, tail) = rest.split_at(cut);
        let status = unsafe { ffi::nw_chunk(doc.raw, head.as_ptr().cast(), head.len()) };
        assert_eq!(status, 0, "lexbor failed to allocate while parsing");
        rest = tail;
    }
    assert_eq!(
        unsafe { ffi::nw_chunk_end(doc.raw, has_template(html) as c_int) },
        0,
        "lexbor failed to allocate while parsing"
    );
    Ok(doc)
}

/// Whether `html` has a `<template` tag, in any case: the only way a
/// template gets into a parsed document.
fn has_template(html: &str) -> bool {
    // Faster than checking every `<`, of which a page has hundreds of
    // thousands; `<te` in each case is rare.
    let bytes = html.as_bytes();
    [b"<te", b"<tE", b"<Te", b"<TE"].iter().any(|start| {
        memchr::memmem::find_iter(bytes, start).any(|i| {
            bytes
                .get(i + 1..i + 9)
                .is_some_and(|name| name.eq_ignore_ascii_case(b"template"))
        })
    })
}

/// Returned by [`Document::parse_within`] when parsing ran out of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTimeout {
    pub budget: Duration,
    /// Bytes parsed before giving up.
    pub parsed: usize,
}

impl fmt::Display for ParseTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parsing took longer than {:?} (gave up after {} bytes)",
            self.budget, self.parsed
        )
    }
}

impl std::error::Error for ParseTimeout {}

/// A node's identity, stable for the life of its document. Lets code that
/// cannot hold a borrow (such as the Python bindings) refer to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Doctype,
    Element,
    Text,
    Comment,
    Other,
}

#[derive(Clone, Copy)]
pub struct Node<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) raw: *mut RawNode,
}

impl<'a> Node<'a> {
    pub(crate) fn from_raw(doc: &'a Document, raw: *mut RawNode) -> Node<'a> {
        Node { doc, raw }
    }

    pub(crate) fn wrap(doc: &'a Document, raw: *mut RawNode) -> Option<Node<'a>> {
        (!raw.is_null()).then_some(Node { doc, raw })
    }

    pub fn id(&self) -> NodeId {
        NodeId(self.raw as usize)
    }

    pub fn kind(&self) -> NodeKind {
        match unsafe { ffi::nw_type(self.raw) } {
            ffi::ELEMENT => NodeKind::Element,
            ffi::TEXT | ffi::CDATA => NodeKind::Text,
            ffi::COMMENT => NodeKind::Comment,
            ffi::DOCUMENT => NodeKind::Document,
            ffi::DOCTYPE => NodeKind::Doctype,
            _ => NodeKind::Other,
        }
    }

    /// The lowercase tag name, for elements.
    pub fn tag(&self) -> Option<&'a str> {
        if self.kind() != NodeKind::Element {
            return None;
        }
        let mut len = 0;
        Some(unsafe { str_at(ffi::nw_tag(self.raw, &mut len), len) })
    }

    pub fn attr(&self, name: &str) -> Option<&'a str> {
        if self.kind() != NodeKind::Element {
            return None;
        }
        let mut len = 0;
        let ptr = unsafe { ffi::nw_get_attr(self.raw, name.as_ptr().cast(), name.len(), &mut len) };
        (!ptr.is_null()).then(|| unsafe { str_at(ptr, len) })
    }

    /// Every attribute as `(name, value)`, in source order.
    pub fn attrs(&self) -> Vec<(&'a str, &'a str)> {
        let mut out = Vec::new();
        if self.kind() != NodeKind::Element {
            return out;
        }
        let mut a = unsafe { ffi::nw_first_attr(self.raw) };
        while !a.is_null() {
            let (mut nl, mut vl) = (0, 0);
            let name = unsafe { str_at(ffi::nw_attr_name(a, &mut nl), nl) };
            let value = unsafe { str_at(ffi::nw_attr_value(a, &mut vl), vl) };
            out.push((name, value));
            a = unsafe { ffi::nw_next_attr(a) };
        }
        out
    }

    /// The node's own text, for text and comment nodes.
    pub fn data(&self) -> Option<&'a str> {
        match self.kind() {
            NodeKind::Text | NodeKind::Comment => {
                let mut len = 0;
                Some(unsafe { str_at(ffi::nw_char_data(self.raw, &mut len), len) })
            }
            _ => None,
        }
    }

    /// lexbor's id for this node's name. Two elements share an id exactly
    /// when their lowercase names are equal.
    pub(crate) fn tag_id(&self) -> usize {
        unsafe { ffi::nw_tag_id(self.raw) }
    }

    /// The id elements named `name` (compared lowercased) carry in this
    /// node's document; `None` if no element has that name.
    pub(crate) fn tag_id_named(&self, name: &str) -> Option<usize> {
        let id = unsafe { ffi::nw_tag_id_named(self.raw, name.as_ptr().cast(), name.len()) };
        (id != 0).then_some(id)
    }

    pub fn next_sibling(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_next(self.raw) })
    }

    pub fn prev_sibling(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_prev(self.raw) })
    }

    pub fn first_child(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_first_child(self.raw) })
    }

    pub fn last_child(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_last_child(self.raw) })
    }

    /// The node after this one in document order: its first child, or the
    /// next sibling of it or its nearest ancestor that has one.
    pub fn next_in_order(&self) -> Option<Node<'a>> {
        if let Some(child) = self.first_child() {
            return Some(child);
        }
        let mut at = *self;
        loop {
            if let Some(next) = at.next_sibling() {
                return Some(next);
            }
            at = at.parent()?;
        }
    }

    /// The node before this one in document order: the last node inside its
    /// previous sibling, or else its parent.
    pub fn prev_in_order(&self) -> Option<Node<'a>> {
        let Some(mut at) = self.prev_sibling() else {
            return self.parent();
        };
        while let Some(last) = at.last_child() {
            at = last;
        }
        Some(at)
    }

    /// The node's position in document order: smaller comes first.
    pub fn order(&self) -> u32 {
        self.doc.order_of(self.raw)
    }

    /// The node as HTML: an element with its attributes and everything in
    /// it, a text node escaped, a comment as `<!--...-->`.
    pub fn html(&self) -> String {
        extern "C" fn push(data: *const c_char, len: usize, ctx: *mut c_void) {
            // SAFETY: lexbor passes `len` valid bytes; ctx is the Vec below.
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
            unsafe { (*ctx.cast::<Vec<u8>>()).extend_from_slice(bytes) }
        }
        let mut out: Vec<u8> = Vec::new();
        // Only fails when lexbor cannot allocate; return what was written.
        unsafe { ffi::nw_serialize(self.raw, push, (&mut out as *mut Vec<u8>).cast()) };
        // lexbor writes UTF-8; anything else would be memory corruption.
        String::from_utf8(out).unwrap_or_default()
    }

    pub fn parent(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_parent(self.raw) })
    }

    pub fn children(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let doc = self.doc;
        std::iter::successors(
            Node::wrap(doc, unsafe { ffi::nw_first_child(self.raw) }),
            move |n| Node::wrap(doc, unsafe { ffi::nw_next(n.raw) }),
        )
    }

    /// Every node below this one, in document order.
    pub fn descendants(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let doc = self.doc;
        let top = self.raw;
        let mut next = unsafe { ffi::nw_first_child(top) };
        std::iter::from_fn(move || {
            let current = Node::wrap(doc, next)?;
            next = unsafe { ffi::nw_first_child(current.raw) };
            let mut up = current.raw;
            while next.is_null() && up != top {
                next = unsafe { ffi::nw_next(up) };
                up = unsafe { ffi::nw_parent(up) };
            }
            Some(current)
        })
    }

    /// The text nodes below this node, in order, leaving out what is code
    /// rather than reading matter: the contents of `<script>`, `<style>`
    /// and `<template>` (Beautiful Soup's `get_text()` does the same). On a
    /// script or style element itself, gives its own text.
    pub fn readable_text_pieces(&self) -> Vec<&'a str> {
        const CODE: [&str; 3] = ["script", "style", "template"];
        if self.tag().is_some_and(|t| CODE.contains(&t)) || self.kind() == NodeKind::Text {
            return self.text_pieces();
        }
        let mut out = Vec::new();
        let mut next = self.first_child();
        while let Some(n) = next {
            let skip = n.tag().is_some_and(|t| CODE.contains(&t));
            if !skip && n.kind() == NodeKind::Text {
                out.extend(n.data());
            }
            // Descend unless skipping; otherwise move on past this subtree.
            next = if !skip { n.first_child() } else { None }.or_else(|| {
                let mut at = n;
                loop {
                    if at.raw == self.raw {
                        return None;
                    }
                    if let Some(s) = at.next_sibling() {
                        return Some(s);
                    }
                    at = at.parent()?;
                    if at.raw == self.raw {
                        return None;
                    }
                }
            });
        }
        out
    }

    /// Every text node's text below (or at) this node, in order.
    fn text_pieces(&self) -> Vec<&'a str> {
        std::iter::once(*self)
            .chain(self.descendants())
            .filter(|n| n.kind() == NodeKind::Text)
            .filter_map(|n| n.data())
            .collect()
    }

    /// All text below this node, joined. Comments are not text.
    pub fn text(&self) -> String {
        if let (NodeKind::Text, Some(d)) = (self.kind(), self.data()) {
            return d.to_string();
        }
        self.descendants()
            .filter(|n| n.kind() == NodeKind::Text)
            .filter_map(|n| n.data())
            .collect()
    }
}

impl PartialEq for Node<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            NodeKind::Element => write!(f, "<{}>", self.tag().unwrap_or("?")),
            NodeKind::Text => write!(f, "{:?}", self.data().unwrap_or("")),
            kind => write!(f, "{kind:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(doc: &Document) -> Vec<String> {
        doc.root()
            .descendants()
            .map(|n| format!("{:?}{:?}", n, n.attrs()))
            .collect()
    }

    #[test]
    fn tiny_chunks_build_the_same_tree_as_one_chunk() {
        // 7-byte chunks split tags, entities, attribute values and multi-byte
        // characters at every offset.
        let html = "<!doctype html><p class=\"a b\">é€😀 &amp; &eacute;</p>\
                    <script>if (a < b) {}</script><textarea>x<y</textarea>\
                    <table>t<tr><td>c</table><svg><path d=\"M0 0\"/></svg>"
            .repeat(50);
        let one = parse_in_chunks(&html, Duration::MAX, usize::MAX).unwrap();
        let small = parse_in_chunks(&html, Duration::MAX, 7).unwrap();
        assert_eq!(shape(&one), shape(&small));
    }
}
