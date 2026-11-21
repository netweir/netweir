//! What a query reads a document through: [`Tree`], which any tree can
//! implement, so XPath doesn't depend on one parser's node layout.

/// A node's id in a [`Tree`]: a u32 below [`Tree::len`].
pub type NodeId = u32;

/// A node's kind, numbered as the DOM's `nodeType` numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeType {
    Element = 0x01,
    Text = 0x03,
    Cdata = 0x04,
    ProcessingInstruction = 0x07,
    Comment = 0x08,
    Document = 0x09,
    Doctype = 0x0A,
    /// Anything XPath has no node for, such as a fragment or a shadow
    /// root. Indexed as a root, it's the top of a tree, as a document is.
    Other = 0x0B,
}

/// What [`crate::Index`] groups elements by for name tests. Elements with
/// the same name lowercased share a group however the tree names them, so
/// a query's name is looked up once, not compared on every element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKey<'t> {
    /// A number the tree gives every element of one name, as an HTML
    /// parser numbers the tags it knows. It saves the index reading and
    /// hashing the name of every element.
    Known(u16),
    /// The element's local name.
    Name(&'t str),
}

/// An attribute as XPath's data model has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr<'t> {
    /// The name as written, with any prefix: what `name()` gives.
    pub qualified_name: &'t str,
    /// The name without its prefix: what `local-name()` gives.
    pub local_name: &'t str,
    /// "" for no namespace.
    pub namespace_uri: &'t str,
    pub value: &'t str,
}

/// A tree XPath can query. Every method takes a node id below
/// [`Tree::len`]; a method about elements gives "", 0 or None for a node
/// of another kind.
pub trait Tree: Sync {
    /// How many node ids there are, including those of removed nodes.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn kind(&self, n: NodeId) -> NodeType;
    fn parent(&self, n: NodeId) -> Option<NodeId>;
    fn first_child(&self, n: NodeId) -> Option<NodeId>;
    fn next_sibling(&self, n: NodeId) -> Option<NodeId>;
    fn prev_sibling(&self, n: NodeId) -> Option<NodeId>;

    /// An element's name as written, with any prefix: what `name()`
    /// gives. Lowercase in HTML.
    fn qualified_name(&self, n: NodeId) -> &str;
    /// An element's name without its prefix: what `local-name()` gives.
    fn local_name(&self, n: NodeId) -> &str;
    /// An element's namespace URI; "" for no namespace.
    fn namespace_uri(&self, n: NodeId) -> &str;
    /// What [`crate::Index`] keys an element's name tests on.
    fn tag_key(&self, n: NodeId) -> TagKey<'_>;

    /// How many attributes an element has. Namespace declarations aren't
    /// attributes: in XPath's data model, `xmlns` and `xmlns:*` are
    /// namespace nodes, so a tree leaves them out of these.
    fn attribute_count(&self, n: NodeId) -> u32;
    /// An element's `i`th attribute, in document order. A query reads
    /// attributes by place, so this should take constant time.
    fn attribute(&self, n: NodeId, i: u32) -> Option<Attr<'_>>;

    /// A text, CDATA section, comment or processing instruction's text.
    fn data(&self, n: NodeId) -> &str;
    /// A processing instruction's target.
    fn pi_target(&self, n: NodeId) -> Option<&str>;

    /// Whether names compare exactly (XML), rather than ignoring ASCII
    /// case with prefixes left as part of the name (HTML).
    fn is_xml(&self) -> bool;
    /// The value `id()` finds an element by: HTML's `id`, or XML's
    /// `xml:id` or an attribute the DTD declares an ID.
    fn id(&self, n: NodeId) -> Option<&str>;
    /// The language an element declares, if it declares one, for
    /// `lang()`: `xml:lang`, or HTML's `lang`.
    fn lang(&self, n: NodeId) -> Option<&str>;
}
