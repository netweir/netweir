//! netweir-html's tree as a [`Tree`]: an HTML page as lxml reads one, and
//! through [`XmlTree`] an XML document built into the same tree.

use netweir_html::{AttrNs, Document, Kind, Ns};

use crate::tree::{Attr, NodeId, NodeType, TagKey, Tree};

/// An HTML page. Names are the parser's lowercase ones in no namespace,
/// and a colon is part of a name (`fb:like`), as in lxml.
impl Tree for Document {
    fn len(&self) -> usize {
        Document::len(self)
    }

    fn kind(&self, n: NodeId) -> NodeType {
        match Document::kind(self, n) {
            Kind::Element { .. } => NodeType::Element,
            Kind::Text(_) => NodeType::Text,
            Kind::Cdata(_) => NodeType::Cdata,
            Kind::ProcessingInstruction { .. } => NodeType::ProcessingInstruction,
            Kind::Comment(_) => NodeType::Comment,
            Kind::Document => NodeType::Document,
            Kind::Doctype { .. } => NodeType::Doctype,
            Kind::Fragment | Kind::ShadowRoot(_) => NodeType::Other,
        }
    }

    fn parent(&self, n: NodeId) -> Option<NodeId> {
        Document::parent(self, n)
    }

    fn first_child(&self, n: NodeId) -> Option<NodeId> {
        Document::first_child(self, n)
    }

    fn next_sibling(&self, n: NodeId) -> Option<NodeId> {
        Document::next_sibling(self, n)
    }

    fn prev_sibling(&self, n: NodeId) -> Option<NodeId> {
        Document::prev_sibling(self, n)
    }

    fn qualified_name(&self, n: NodeId) -> &str {
        self.lower_name(n).unwrap_or("")
    }

    fn local_name(&self, n: NodeId) -> &str {
        self.lower_name(n).unwrap_or("")
    }

    fn namespace_uri(&self, _: NodeId) -> &str {
        ""
    }

    fn tag_key(&self, n: NodeId) -> TagKey<'_> {
        if let Some(t) = self.known_tag(n) {
            return TagKey::Known(t.into());
        }
        let name = self.lower_name(n).unwrap_or("");
        // Only XML has prefixes: in HTML, <o:p> is an element called "o:p".
        TagKey::Name(match Document::kind(self, n) {
            Kind::Element { ns: Ns::Xml, .. } => {
                name.split_once(':').map_or(name, |(_, local)| local)
            }
            _ => name,
        })
    }

    fn attribute_count(&self, n: NodeId) -> u32 {
        self.attr_count(n) as u32
    }

    fn attribute(&self, n: NodeId, i: u32) -> Option<Attr<'_>> {
        self.attr_at(n, i as usize).map(|(_, name, value)| Attr {
            qualified_name: name,
            local_name: name,
            namespace_uri: "",
            value,
        })
    }

    fn data(&self, n: NodeId) -> &str {
        match Document::kind(self, n) {
            Kind::Text(t) | Kind::Cdata(t) | Kind::Comment(t) => t,
            Kind::ProcessingInstruction { data, .. } => data,
            _ => "",
        }
    }

    fn pi_target(&self, n: NodeId) -> Option<&str> {
        match Document::kind(self, n) {
            Kind::ProcessingInstruction { target, .. } => Some(target),
            _ => None,
        }
    }

    fn is_xml(&self) -> bool {
        false
    }

    fn id(&self, n: NodeId) -> Option<&str> {
        attr_ignoring_case(self, n, "id")
    }

    fn lang(&self, n: NodeId) -> Option<&str> {
        attr_ignoring_case(self, n, "xml:lang").or_else(|| attr_ignoring_case(self, n, "lang"))
    }
}

/// HTML names are lowercase, so a name is looked up ignoring case.
fn attr_ignoring_case<'t>(tree: &'t Document, n: NodeId, name: &str) -> Option<&'t str> {
    tree.attrs(n)
        .find(|(_, k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, _, v)| v)
}

/// The namespaces of an XML document built into a netweir-html tree,
/// which the tree leaves to its host. Ids are the host's; 0 is no
/// namespace.
pub trait Namespaces: Sync {
    /// The namespace id of an element.
    fn ns_of(&self, element: NodeId) -> u32;
    /// The URI with id `id`; "" for 0.
    fn uri(&self, id: u32) -> &str;
}

/// An XML document built into a netweir-html tree: its elements are
/// [`Ns::Xml`] with their names as written, its attributes' namespaces are
/// [`AttrNs::Host`] ids, and `names` holds what those ids stand for. The
/// host keeps namespace declarations out of the attributes.
#[derive(Clone, Copy)]
pub struct XmlTree<'a> {
    tree: &'a Document,
    names: &'a dyn Namespaces,
}

impl<'a> XmlTree<'a> {
    pub fn new(tree: &'a Document, names: &'a dyn Namespaces) -> XmlTree<'a> {
        XmlTree { tree, names }
    }
}

fn local_part(name: &str) -> &str {
    name.split_once(':').map_or(name, |(_, local)| local)
}

impl XmlTree<'_> {
    fn attr(&self, n: NodeId, name: &str) -> Option<&str> {
        self.tree
            .attrs(n)
            .find(|(_, k, _)| *k == name)
            .map(|(_, _, v)| v)
    }
}

impl Tree for XmlTree<'_> {
    fn len(&self) -> usize {
        self.tree.len()
    }

    fn kind(&self, n: NodeId) -> NodeType {
        Tree::kind(self.tree, n)
    }

    fn parent(&self, n: NodeId) -> Option<NodeId> {
        self.tree.parent(n)
    }

    fn first_child(&self, n: NodeId) -> Option<NodeId> {
        self.tree.first_child(n)
    }

    fn next_sibling(&self, n: NodeId) -> Option<NodeId> {
        self.tree.next_sibling(n)
    }

    fn prev_sibling(&self, n: NodeId) -> Option<NodeId> {
        self.tree.prev_sibling(n)
    }

    fn qualified_name(&self, n: NodeId) -> &str {
        self.tree.lower_name(n).unwrap_or("")
    }

    fn local_name(&self, n: NodeId) -> &str {
        local_part(self.tree.lower_name(n).unwrap_or(""))
    }

    fn namespace_uri(&self, n: NodeId) -> &str {
        self.names.uri(self.names.ns_of(n))
    }

    fn tag_key(&self, n: NodeId) -> TagKey<'_> {
        self.tree.tag_key(n)
    }

    fn attribute_count(&self, n: NodeId) -> u32 {
        self.tree.attribute_count(n)
    }

    fn attribute(&self, n: NodeId, i: u32) -> Option<Attr<'_>> {
        self.tree
            .attr_at(n, i as usize)
            .map(|(ns, name, value)| Attr {
                qualified_name: name,
                local_name: local_part(name),
                namespace_uri: match ns {
                    AttrNs::Host(id) => self.names.uri(id),
                    _ => "",
                },
                value,
            })
    }

    fn data(&self, n: NodeId) -> &str {
        self.tree.data(n)
    }

    fn pi_target(&self, n: NodeId) -> Option<&str> {
        self.tree.pi_target(n)
    }

    fn is_xml(&self) -> bool {
        true
    }

    fn id(&self, n: NodeId) -> Option<&str> {
        self.attr(n, "id")
    }

    fn lang(&self, n: NodeId) -> Option<&str> {
        self.attr(n, "xml:lang").or_else(|| self.attr(n, "lang"))
    }
}
