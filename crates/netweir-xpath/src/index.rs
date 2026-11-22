//! A flat index of a document's nodes in document order. A node's
//! descendants are the contiguous range after it, so scanning them is a
//! pass over small arrays rather than a walk through the tree's links.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::tree::{NodeId, NodeType, TagKey, Tree};

/// The node kinds, as the index stores them: the DOM's nodeType numbers.
pub const ELEMENT: u8 = NodeType::Element as u8;
pub const TEXT: u8 = NodeType::Text as u8;
pub const CDATA: u8 = NodeType::Cdata as u8;
pub const PI: u8 = NodeType::ProcessingInstruction as u8;
pub const COMMENT: u8 = NodeType::Comment as u8;
pub const DOCUMENT: u8 = NodeType::Document as u8;
pub const DOCTYPE: u8 = NodeType::Doctype as u8;

/// The index of one version of a tree. Its owner builds it once and keeps
/// it until the tree changes: every query reads it, none builds it.
pub struct Index {
    raw: Vec<u32>,
    /// Each node's parent's position; u32::MAX for the root.
    parent: Vec<u32>,
    /// One past the last descendant of each node.
    end: Vec<u32>,
    kind: Vec<u8>,
    /// u32, as node ids are: one for every node adds up on a big page.
    /// 0 for a node that isn't an element.
    tag: Vec<u32>,
    /// Each node's position, by node id; u32::MAX for one not indexed.
    order: Vec<u32>,
    /// Tag ids by lowercase name, from 1.
    names: HashMap<String, usize>,
    /// Positions of the elements carrying each `id`, built on first use.
    ids: OnceLock<HashMap<String, Vec<u32>>>,
    /// The namespace declarations, built on the namespace axis's first use.
    scopes: OnceLock<Scopes>,
}

/// Where a tree declares namespaces, for the namespace axis. Each node
/// that declares some is a block, and every other node is in the block of
/// the nearest one above it, so working out what's in scope at a node
/// climbs only through nodes that declare something.
pub(crate) struct Scopes {
    /// The block of the node at each position; u32::MAX for none.
    block_at: Vec<u32>,
    pub(crate) blocks: Vec<Block>,
    /// Every declaration, numbered as the namespace axis orders them.
    pub(crate) decls: Vec<Decl>,
    /// How many different prefixes there are.
    pub(crate) prefixes: usize,
}

/// A node's declarations: `decls[first..first + len]`, and the block
/// above it.
pub(crate) struct Block {
    pub(crate) first: u32,
    pub(crate) len: u32,
    pub(crate) parent: u32,
}

pub(crate) struct Decl {
    /// The declaring node, and the declaration's place among its own.
    pub(crate) node: NodeId,
    pub(crate) index: u32,
    /// The prefix, numbered from 0 by [`Scopes`].
    pub(crate) prefix: u32,
    /// `xmlns=""` or `xmlns:p=""`, which takes a binding away.
    pub(crate) undeclares: bool,
}

impl Scopes {
    /// The block in effect at position `at`; u32::MAX for none.
    pub(crate) fn block_at(&self, at: u32) -> u32 {
        self.block_at.get(at as usize).copied().unwrap_or(u32::MAX)
    }
}

/// The tag id of the lowercase name `key`, given the next free one if new.
fn tag_id(names: &mut HashMap<String, usize>, key: String) -> u32 {
    let next = names.len() + 1;
    *names.entry(key).or_insert(next) as u32
}

impl Index {
    /// Indexes the tree under each of `roots` in turn: the document, then
    /// any subtrees removed from it or built apart from it, so every node
    /// a caller can hold has a position.
    ///
    /// Generic, unlike a query's reading of the tree: building reads every
    /// node, and a call through `dyn` for each read showed in benchmarks.
    pub fn build<T: Tree + ?Sized>(tree: &T, roots: &[NodeId]) -> Index {
        let mut raw = Vec::with_capacity(tree.len());
        let mut parent = Vec::with_capacity(tree.len());
        let mut kind = Vec::with_capacity(tree.len());
        let mut tag = Vec::with_capacity(tree.len());
        let mut order = vec![u32::MAX; tree.len()];
        let mut names: HashMap<String, usize> = HashMap::new();
        // Tag ids by the tree's own tag number, 0 for one not yet seen.
        let mut known: Vec<u32> = Vec::new();
        for &root in roots {
            // Preorder by the tree's links, keeping each node's parent's
            // position on a stack of the open ancestors.
            let mut ancestors: Vec<u32> = Vec::new();
            let mut at = Some(root);
            while let Some(n) = at {
                let pos = raw.len() as u32;
                order[n as usize] = pos;
                raw.push(n);
                parent.push(ancestors.last().copied().unwrap_or(u32::MAX));
                let k = tree.kind(n) as u8;
                kind.push(k);
                // From the lowercase name alone, as tag_id_named works it
                // out: an XML <title> shares HTML title's id.
                tag.push(if k != ELEMENT {
                    0
                } else {
                    match tree.tag_key(n) {
                        TagKey::Known(t) => {
                            let t = t as usize;
                            match known.get(t) {
                                Some(&id) if id != 0 => id,
                                _ => {
                                    let key = tree.local_name(n).to_ascii_lowercase();
                                    let id = tag_id(&mut names, key);
                                    if known.len() <= t {
                                        known.resize(t + 1, 0);
                                    }
                                    known[t] = id;
                                    id
                                }
                            }
                        }
                        TagKey::Name(name) => tag_id(&mut names, name.to_ascii_lowercase()),
                    }
                });
                at = match tree.first_child(n) {
                    Some(child) => {
                        ancestors.push(pos);
                        Some(child)
                    }
                    None => {
                        let mut up = n;
                        loop {
                            if up == root {
                                break None;
                            }
                            if let Some(s) = tree.next_sibling(up) {
                                break Some(s);
                            }
                            up = tree
                                .parent(up)
                                .expect("a node inside the root has a parent");
                            ancestors.pop();
                        }
                    }
                };
            }
        }
        let n = raw.len();
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
            order,
            names,
            ids: OnceLock::new(),
            scopes: OnceLock::new(),
        }
    }

    /// The namespace declarations of `tree`, which must be the tree this
    /// indexes, and whether this call is the one that read them: one pass
    /// over the nodes, done once per index.
    pub(crate) fn scopes(&self, tree: &dyn Tree) -> (&Scopes, bool) {
        let mut built = false;
        let scopes = self.scopes.get_or_init(|| {
            built = true;
            let mut block_at = Vec::with_capacity(self.raw.len());
            let mut blocks = Vec::new();
            let mut decls = Vec::new();
            let mut prefixes: HashMap<&str, u32> = HashMap::new();
            for (pos, &n) in self.raw.iter().enumerate() {
                let up = match self.parent[pos] {
                    u32::MAX => u32::MAX,
                    p => block_at[p as usize],
                };
                let count = tree.namespace_declaration_count(n);
                if count == 0 {
                    block_at.push(up);
                    continue;
                }
                let first = decls.len() as u32;
                // Last written first: the order libxml2, and so lxml, gives
                // a node's namespaces in.
                for i in (0..count).rev() {
                    let Some((prefix, uri)) = tree.namespace_declaration(n, i) else {
                        continue;
                    };
                    // `xml` is always in scope, bound to XML_NS, and never
                    // declared twice.
                    if prefix == "xml" {
                        continue;
                    }
                    let next = prefixes.len() as u32;
                    decls.push(Decl {
                        node: n,
                        index: i,
                        prefix: *prefixes.entry(prefix).or_insert(next),
                        undeclares: uri.is_empty(),
                    });
                }
                blocks.push(Block {
                    first,
                    len: decls.len() as u32 - first,
                    parent: up,
                });
                block_at.push(blocks.len() as u32 - 1);
            }
            Scopes {
                block_at,
                blocks,
                decls,
                prefixes: prefixes.len(),
            }
        });
        (scopes, built)
    }

    /// Whether the index has a place for every node `tree` has, as it does
    /// for the document it was built from until a node is made in it.
    pub fn covers(&self, tree: &dyn Tree) -> bool {
        self.order.len() == tree.len()
    }

    /// How many nodes are indexed.
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// A node's position; u32::MAX if it was made after the index was.
    pub fn order(&self, id: NodeId) -> u32 {
        self.order.get(id as usize).copied().unwrap_or(u32::MAX)
    }

    /// The node at position `at`.
    pub fn node_at(&self, at: u32) -> NodeId {
        self.raw[at as usize]
    }

    /// The kind ([`ELEMENT`], [`TEXT`] and so on) at position `at`.
    pub fn kind_at(&self, at: u32) -> u8 {
        self.kind[at as usize]
    }

    /// The tag id at position `at`.
    pub fn tag_at(&self, at: u32) -> usize {
        self.tag.get(at as usize).map_or(0, |&t| t as usize)
    }

    /// The tag id elements named `name` (compared lowercased) carry, if
    /// some element in the index has that name.
    pub fn tag_id_named(&self, name: &str) -> Option<usize> {
        self.names.get(&name.to_ascii_lowercase()).copied()
    }

    /// The elements whose `id` is `id`, in document order. `id_of` reads a
    /// node's `id` attribute; the first call's is used for every call.
    pub fn with_id<'t>(&self, id: &str, id_of: impl Fn(NodeId) -> Option<&'t str>) -> Vec<NodeId> {
        let ids = self.ids.get_or_init(|| {
            let mut ids: HashMap<String, Vec<u32>> = HashMap::new();
            for (j, &raw) in self.raw.iter().enumerate() {
                if let Some(v) = id_of(raw) {
                    ids.entry(v.to_string()).or_default().push(j as u32);
                }
            }
            ids
        });
        ids.get(id)
            .map(|at| at.iter().map(|&j| self.raw[j as usize]).collect())
            .unwrap_or_default()
    }

    /// Appends to `out` the nodes below `at` (from `at` itself if
    /// `include_self`) that pass `keep(kind, tag)`, in document order.
    pub fn descendants(
        &self,
        at: u32,
        include_self: bool,
        keep: impl Fn(u8, usize) -> bool,
        out: &mut Vec<NodeId>,
    ) {
        let start = if include_self { at } else { at + 1 } as usize;
        for j in start..self.end[at as usize] as usize {
            if keep(self.kind[j], self.tag[j] as usize) {
                out.push(self.raw[j]);
            }
        }
    }

    /// One past the last descendant of `at`.
    pub fn end_of(&self, at: u32) -> u32 {
        self.end[at as usize]
    }

    /// The position of the parent of the node at `at`; u32::MAX for a
    /// root.
    pub fn parent_of(&self, at: u32) -> u32 {
        self.parent[at as usize]
    }

    /// Appends to `out` the children of `at` that pass `keep(kind, tag)`.
    pub fn children(&self, at: u32, keep: impl Fn(u8, usize) -> bool, out: &mut Vec<NodeId>) {
        let stop = self.end[at as usize] as usize;
        let mut c = at as usize + 1;
        while c < stop {
            if keep(self.kind[c], self.tag[c] as usize) {
                out.push(self.raw[c]);
            }
            c = self.end[c] as usize;
        }
    }
}
