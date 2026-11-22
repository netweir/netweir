//! XPath 1.0 over any document tree that implements [`Tree`], written
//! from the W3C recommendation: every axis, node test and function of the
//! core library, with the number, string and boolean rules of sections 3
//! and 4.
//!
//! With the default `html` feature, netweir-html's [`Document`] is a
//! [`Tree`], read as lxml reads an HTML page, which is what scrapers
//! expect: element and attribute names are the parser's lowercase ones
//! (SVG's `clippath`, `viewbox`) in no namespace, so `//div` and `@href`
//! match, `//DIV` and `@HREF` don't, and a prefixed name (`svg:rect`)
//! matches nothing. An XML document built into the same tree is read
//! through [`XmlTree`], which takes its namespaces from the host. In XML,
//! names with a prefix take theirs from the prefixes a query is run with.
//!
//! A query reads the document through an [`Index`], which its owner
//! builds once per version of the tree. Functions beyond the core library
//! come from the caller through [`Extensions`].
//!
//! Shadow roots are left out, as `document.evaluate` leaves them out: the
//! XPath data model has no shadow trees, so the axes follow the node tree
//! and never cross into a host's shadow root or out of one. A shadow tree
//! is queried on its own by indexing its root as one of [`Index::build`]'s
//! roots, where it's a root node as a template's contents are: `/` from
//! inside it is the shadow root, and its host isn't an ancestor.

mod eval;
#[cfg(feature = "html")]
mod html;
mod index;
mod lexer;
mod parser;
mod tree;

use std::borrow::Cow;
use std::fmt;

#[cfg(feature = "html")]
use netweir_html::Document;

pub use eval::{Args, WORK_LIMIT};
#[cfg(feature = "html")]
pub use html::{Namespaces, XmlTree};
pub use index::{CDATA, COMMENT, DOCTYPE, DOCUMENT, ELEMENT, Index, PI, TEXT};
pub use tree::{Attr, NodeId, NodeType, TagKey, Tree};

/// The namespace the `xml` prefix is always bound to.
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPathError {
    pub query: String,
    pub reason: String,
}

impl fmt::Display for XPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid XPath {:?}: {}", self.query, self.reason)
    }
}

impl std::error::Error for XPathError {}

/// A value a caller can bind to a `$variable`.
#[derive(Debug, Clone, PartialEq)]
pub enum XValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

/// The document a query runs over: its tree and the tree's index.
#[derive(Clone, Copy)]
pub struct Doc<'a> {
    pub(crate) tree: &'a dyn Tree,
    pub(crate) index: &'a Index,
    /// The tree's [`Tree::is_xml`], read once rather than on every name.
    xml: bool,
}

impl<'a> Doc<'a> {
    /// `index` must be of `tree` as it is now.
    pub fn new(tree: &'a dyn Tree, index: &'a Index) -> Doc<'a> {
        Doc {
            tree,
            index,
            xml: tree.is_xml(),
        }
    }

    /// An HTML page. `index` must be of `tree` as it is now.
    #[cfg(feature = "html")]
    pub fn html(tree: &'a Document, index: &'a Index) -> Doc<'a> {
        Doc::new(tree, index)
    }

    pub fn tree(&self) -> &'a dyn Tree {
        self.tree
    }

    pub fn is_xml(&self) -> bool {
        self.xml
    }

    /// An attribute's value by name: exact in XML, and in HTML compared
    /// ignoring ASCII case, as HTML names are lowercase.
    pub fn attr(&self, node: NodeId, name: &str) -> Option<&'a str> {
        let (tree, xml) = (self.tree, self.xml);
        (0..tree.attribute_count(node))
            .filter_map(|i| tree.attribute(node, i))
            .find(|a| {
                if xml {
                    a.qualified_name == name
                } else {
                    a.qualified_name.eq_ignore_ascii_case(name)
                }
            })
            .map(|a| a.value)
    }

    /// The name (as written, with any prefix) and value of the `index`th
    /// attribute of `owner`, as an [`Item::Attr`] names it.
    pub fn attr_at(&self, owner: NodeId, index: u32) -> (&'a str, &'a str) {
        self.tree
            .attribute(owner, index)
            .map_or(("", ""), |a| (a.qualified_name, a.value))
    }

    /// The prefix ("" for the default namespace) and URI of the namespace
    /// node an [`Item::Ns`] with this `decl` names.
    pub fn namespace_at(&self, decl: u32) -> (&'a str, &'a str) {
        if decl == XML_DECL {
            return ("xml", XML_NS);
        }
        let (scopes, _) = self.index.scopes(self.tree);
        scopes
            .decls
            .get(decl as usize)
            .and_then(|d| self.tree.namespace_declaration(d.node, d.index))
            .unwrap_or(("", ""))
    }
}

/// The `decl` of an [`Item::Ns`] for the `xml` namespace, which every
/// element has.
pub const XML_DECL: u32 = u32::MAX;

/// A node in XPath's data model: a tree node, or an attribute or
/// namespace node (which the tree keeps on its element rather than as a
/// node of its own). Twelve bytes: a node-set can hold every node of a big
/// page, so its name, namespace and value are read from the tree
/// ([`Tree::attribute`], [`Doc::namespace_at`]), not kept here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Item {
    Node(NodeId),
    Attr {
        owner: NodeId,
        /// Its place among the element's attributes.
        index: u32,
    },
    /// A namespace in scope at an element, from `namespace::`.
    Ns {
        /// The element: the namespace node's parent.
        owner: NodeId,
        /// Which declaration binds it, numbered by the index;
        /// [`XML_DECL`] for the `xml` namespace.
        decl: u32,
    },
}

const _: () = assert!(std::mem::size_of::<Item>() == 12);

/// What an expression evaluates to. A node-set is in document order,
/// without duplicates.
#[derive(Debug, Clone)]
pub enum Value<'a> {
    Nodes(Vec<Item>),
    Str(Cow<'a, str>),
    Num(f64),
    Bool(bool),
}

/// Functions beyond XPath 1.0's core library, which a caller provides. A
/// core function of the same name wins.
pub trait Extensions: Sync {
    /// Works out `name()`, or None if this has no function of that name.
    fn call<'a>(&self, name: &str, args: &Args<'_, 'a>) -> Option<Result<Value<'a>, String>>;

    /// Whether `name()` only ever gives a number or a yes or no, so a
    /// predicate's call of it can be kept per node (see `memoize`).
    fn scalar(&self, _name: &str) -> bool {
        false
    }
}

/// No functions beyond the core library.
pub struct NoExtensions;

impl Extensions for NoExtensions {
    fn call<'a>(&self, _: &str, _: &Args<'_, 'a>) -> Option<Result<Value<'a>, String>> {
        None
    }
}

/// A compiled XPath expression. It holds no state of any document, so one
/// can be shared by many threads.
#[derive(Debug, Clone)]
pub struct XPath {
    src: String,
    expr: parser::Expr,
    /// The `$variables` the expression uses, found once here rather than
    /// on every run.
    variables: Vec<String>,
}

impl XPath {
    pub fn new(src: &str) -> Result<XPath, XPathError> {
        XPath::with_extensions(src, &NoExtensions)
    }

    /// [`XPath::new`], for running with `extensions`.
    pub fn with_extensions(src: &str, extensions: &dyn Extensions) -> Result<XPath, XPathError> {
        let tokens = lexer::tokenize(src)?;
        let mut expr = parser::parse(src, tokens)?;
        parser::memoize(&mut expr, &|name| extensions.scalar(name));
        let mut variables = Vec::new();
        expr.variables(&mut variables);
        Ok(XPath {
            src: src.to_string(),
            expr,
            variables,
        })
    }

    /// The expression as written.
    pub fn source(&self) -> &str {
        &self.src
    }

    /// Evaluates with `scope` as the context node, `vars` giving the
    /// `$variables` and `namespaces` binding prefixes to URIs as
    /// `(prefix, uri)` (a later pair for a prefix wins). `extensions`
    /// should be what the expression was compiled with.
    pub fn evaluate<'a>(
        &self,
        doc: Doc<'a>,
        scope: NodeId,
        vars: &[(String, XValue)],
        namespaces: &[(String, String)],
        extensions: &dyn Extensions,
    ) -> Result<Value<'a>, XPathError> {
        self.evaluate_within(doc, scope, vars, namespaces, extensions, WORK_LIMIT)
    }

    /// [`XPath::evaluate`], giving up after `limit` units of work rather
    /// than [`WORK_LIMIT`].
    pub fn evaluate_within<'a>(
        &self,
        doc: Doc<'a>,
        scope: NodeId,
        vars: &[(String, XValue)],
        namespaces: &[(String, String)],
        extensions: &dyn Extensions,
        limit: u64,
    ) -> Result<Value<'a>, XPathError> {
        if let Some(missing) = self
            .variables
            .iter()
            .find(|v| !vars.iter().any(|(k, _)| k == *v))
        {
            return Err(XPathError {
                query: self.src.clone(),
                reason: format!("no value given for ${missing}"),
            });
        }
        // An index of another document, or one older than a node in it,
        // would send the walk past its tables.
        let index = doc.index;
        let at = index.order(scope);
        if (scope as usize) >= doc.tree.len()
            || !index.covers(doc.tree)
            || at == u32::MAX
            || index.node_at(at) != scope
        {
            return Err(XPathError {
                query: self.src.clone(),
                reason: format!(
                    "node {scope} isn't in the index: it was built for another document, \
                     or before the node was made, or without the node's tree"
                ),
            });
        }
        eval::run(&self.expr, doc, scope, vars, namespaces, extensions, limit).map_err(|reason| {
            XPathError {
                query: self.src.clone(),
                reason,
            }
        })
    }
}

// A compiled expression is shared between threads; this fails the build
// if a change ever stops that.
const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<XPath>();
    shareable::<Index>();
};
