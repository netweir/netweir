//! A flat index of a document's nodes in document order. A node's
//! descendants are the contiguous range after it, so scanning them is a
//! pass over small arrays rather than a walk through lexbor's linked nodes.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::document::{Document, Node};
use crate::ffi::{self, RawNode};

pub(crate) struct Index {
    raw: Vec<*mut RawNode>,
    /// Each node's parent's position; u32::MAX for the root.
    parent: Vec<u32>,
    /// One past the last descendant of each node.
    end: Vec<u32>,
    kind: Vec<u8>,
    tag: Vec<usize>,
    /// Positions of the elements carrying each `id`, built on first use.
    ids: OnceLock<HashMap<String, Vec<u32>>>,
}

// SAFETY: the pointers are into a document that outlives the index (the
// Document owns it) and never frees a node before it is dropped. An edit
// replaces the index rather than changing it.
unsafe impl Send for Index {}
unsafe impl Sync for Index {}

impl Index {
    /// Indexes the tree under each of `roots` in turn: the document, then
    /// any subtrees removed from it or built apart from it, so every node
    /// a caller can hold has a position.
    pub(crate) fn build(roots: &[*mut RawNode]) -> Index {
        let mut starts = Vec::with_capacity(roots.len());
        let mut n = 0;
        for &root in roots {
            starts.push(n);
            n = unsafe { ffi::nw_number(root, n) };
        }
        let mut raw: Vec<*mut RawNode> = vec![std::ptr::null_mut(); n];
        let mut parent = vec![0u32; n];
        let mut kind = vec![0u8; n];
        let mut tag = vec![0usize; n];
        for (&root, &at) in roots.iter().zip(&starts) {
            unsafe {
                ffi::nw_index_fill(
                    root,
                    raw.as_mut_ptr().add(at),
                    parent.as_mut_ptr().add(at),
                    kind.as_mut_ptr().add(at),
                    tag.as_mut_ptr().add(at),
                )
            };
        }
        // Children come after their parent, so one backward pass carries
        // each subtree's end up to its parent.
        let mut end: Vec<u32> = (1..=n as u32).collect();
        for i in (1..n).rev() {
            let p = parent[i] as usize;
            if p < n && end[i] > end[p] {
                end[p] = end[i];
            }
        }
        Index {
            raw,
            parent,
            end,
            kind,
            tag,
            ids: OnceLock::new(),
        }
    }

    /// The elements whose `id` is `id`, in document order.
    pub(crate) fn with_id<'a>(&self, doc: &'a Document, id: &str) -> Vec<Node<'a>> {
        let ids = self.ids.get_or_init(|| {
            let mut ids: HashMap<String, Vec<u32>> = HashMap::new();
            for (j, &raw) in self.raw.iter().enumerate() {
                if let Some(v) = Node::from_raw(doc, raw).attr("id") {
                    ids.entry(v.to_string()).or_default().push(j as u32);
                }
            }
            ids
        });
        ids.get(id)
            .map(|at| {
                at.iter()
                    .map(|&j| Node::from_raw(doc, self.raw[j as usize]))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Appends to `out` the nodes below `at` (from `at` itself if
    /// `include_self`) that pass `keep(kind, tag)`, in document order.
    pub(crate) fn descendants<'a>(
        &self,
        doc: &'a Document,
        at: u32,
        include_self: bool,
        keep: impl Fn(u8, usize) -> bool,
        out: &mut Vec<Node<'a>>,
    ) {
        let start = if include_self { at } else { at + 1 } as usize;
        for j in start..self.end[at as usize] as usize {
            if keep(self.kind[j], self.tag[j]) {
                out.push(Node::from_raw(doc, self.raw[j]));
            }
        }
    }

    /// The nodes in `at`'s tree, other than `at`, of its kind and tag and
    /// with the same tags all the way up, that pass `keep`, in document
    /// order. Tags are compared as lexbor's ids, so the scan is integer
    /// work until `keep` runs.
    pub(crate) fn same_path<'a>(
        &self,
        doc: &'a Document,
        at: u32,
        keep: impl Fn(Node<'a>) -> bool,
    ) -> Vec<Node<'a>> {
        let a = at as usize;
        let mut top = a;
        while self.parent[top] != u32::MAX {
            top = self.parent[top] as usize;
        }
        let mut out = Vec::new();
        for j in top..self.end[top] as usize {
            if j != a
                && self.tag[j] == self.tag[a]
                && self.kind[j] == self.kind[a]
                && self.same_ancestors(j, a)
            {
                let node = Node::from_raw(doc, self.raw[j]);
                if keep(node) {
                    out.push(node);
                }
            }
        }
        out
    }

    /// Whether the ancestors of `x` and `y` have the same tags, level by
    /// level, up to the root of their tree.
    fn same_ancestors(&self, mut x: usize, mut y: usize) -> bool {
        loop {
            let (px, py) = (self.parent[x], self.parent[y]);
            // Siblings and cousins meet at a shared ancestor, and the rest
            // of the way up is the same for both.
            if px == py {
                return true;
            }
            if px == u32::MAX || py == u32::MAX {
                return false;
            }
            (x, y) = (px as usize, py as usize);
            if self.tag[x] != self.tag[y] || self.kind[x] != self.kind[y] {
                return false;
            }
        }
    }

    /// One past the last descendant of `at`.
    pub(crate) fn end_of(&self, at: u32) -> u32 {
        self.end[at as usize]
    }

    /// The position of the parent of the node at `at`.
    pub(crate) fn parent_of(&self, at: u32) -> u32 {
        self.parent[at as usize]
    }

    /// Appends to `out` the children of `at` that pass `keep(kind, tag)`.
    pub(crate) fn children<'a>(
        &self,
        doc: &'a Document,
        at: u32,
        keep: impl Fn(u8, usize) -> bool,
        out: &mut Vec<Node<'a>>,
    ) {
        let stop = self.end[at as usize] as usize;
        let mut c = at as usize + 1;
        while c < stop {
            if keep(self.kind[c], self.tag[c]) {
                out.push(Node::from_raw(doc, self.raw[c]));
            }
            c = self.end[c] as usize;
        }
    }
}
