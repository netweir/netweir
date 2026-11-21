//! XPath over a tree of the test's own, not netweir-html's: names,
//! namespaces, attributes, text, `id()` and `lang()` all come through the
//! `Tree` trait, so this builds and runs without the `html` feature.

use netweir_xpath::{
    Attr, Doc, Index, Item, NoExtensions, NodeId, NodeType, TagKey, Tree, Value, XML_NS, XPath,
};

struct Node {
    kind: NodeType,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    /// An element's prefix, local name and namespace URI.
    name: (&'static str, &'static str, &'static str),
    attrs: Vec<Attr<'static>>,
    /// Text, or a processing instruction's target and data.
    data: &'static str,
    target: Option<&'static str>,
}

/// A document laid out by hand, as an XML parser of its own would hold it:
///
/// ```xml
/// <r:doc xmlns:r="urn:r" xml:lang="en-GB">
///   <r:item xml:id="a" r:kind="x" plain="1">hello</r:item>
///   <Item xml:lang="fr">bonjour</Item>
///   <?pi data?><!--c-->
/// </r:doc>
/// ```
///
/// The `xmlns:r` declaration is left out of the attributes, as the trait
/// asks.
struct Hand {
    nodes: Vec<Node>,
    qnames: Vec<String>,
}

fn attr(
    qualified_name: &'static str,
    namespace_uri: &'static str,
    value: &'static str,
) -> Attr<'static> {
    Attr {
        qualified_name,
        local_name: qualified_name.rsplit(':').next().unwrap(),
        namespace_uri,
        value,
    }
}

impl Hand {
    fn new() -> Hand {
        let node = |kind, parent: Option<NodeId>| Node {
            kind,
            parent,
            children: Vec::new(),
            name: ("", "", ""),
            attrs: Vec::new(),
            data: "",
            target: None,
        };
        let mut nodes = vec![node(NodeType::Document, None)];
        let mut add = |n: Node| -> NodeId {
            let id = nodes.len() as NodeId;
            if let Some(p) = n.parent {
                nodes[p as usize].children.push(id);
            }
            nodes.push(n);
            id
        };
        let doc = add(Node {
            name: ("r", "doc", "urn:r"),
            attrs: vec![attr("xml:lang", XML_NS, "en-GB")],
            ..node(NodeType::Element, Some(0))
        });
        let item = add(Node {
            name: ("r", "item", "urn:r"),
            attrs: vec![
                attr("xml:id", XML_NS, "a"),
                attr("r:kind", "urn:r", "x"),
                attr("plain", "", "1"),
            ],
            ..node(NodeType::Element, Some(doc))
        });
        add(Node {
            data: "hello",
            ..node(NodeType::Text, Some(item))
        });
        let other = add(Node {
            name: ("", "Item", ""),
            attrs: vec![attr("xml:lang", XML_NS, "fr")],
            ..node(NodeType::Element, Some(doc))
        });
        add(Node {
            data: "bonjour",
            ..node(NodeType::Text, Some(other))
        });
        add(Node {
            data: "data",
            target: Some("pi"),
            ..node(NodeType::ProcessingInstruction, Some(doc))
        });
        add(Node {
            data: "c",
            ..node(NodeType::Comment, Some(doc))
        });
        let qnames = nodes
            .iter()
            .map(|n| match n.name {
                ("", local, _) => local.to_string(),
                (p, local, _) => format!("{p}:{local}"),
            })
            .collect();
        Hand { nodes, qnames }
    }

    fn node(&self, n: NodeId) -> &Node {
        &self.nodes[n as usize]
    }

    /// The node's place among its parent's children, and those children.
    fn siblings(&self, n: NodeId) -> Option<(usize, &[NodeId])> {
        let parent = self.node(self.node(n).parent?);
        let at = parent.children.iter().position(|&c| c == n)?;
        Some((at, &parent.children))
    }

    fn attr_named(&self, n: NodeId, name: &str) -> Option<&str> {
        let attrs = &self.node(n).attrs;
        attrs
            .iter()
            .find(|a| a.qualified_name == name)
            .map(|a| a.value)
    }
}

impl Tree for Hand {
    fn len(&self) -> usize {
        self.nodes.len()
    }

    fn kind(&self, n: NodeId) -> NodeType {
        self.node(n).kind
    }

    fn parent(&self, n: NodeId) -> Option<NodeId> {
        self.node(n).parent
    }

    fn first_child(&self, n: NodeId) -> Option<NodeId> {
        self.node(n).children.first().copied()
    }

    fn next_sibling(&self, n: NodeId) -> Option<NodeId> {
        let (at, all) = self.siblings(n)?;
        all.get(at + 1).copied()
    }

    fn prev_sibling(&self, n: NodeId) -> Option<NodeId> {
        let (at, all) = self.siblings(n)?;
        all.get(at.checked_sub(1)?).copied()
    }

    fn qualified_name(&self, n: NodeId) -> &str {
        &self.qnames[n as usize]
    }

    fn local_name(&self, n: NodeId) -> &str {
        self.node(n).name.1
    }

    fn namespace_uri(&self, n: NodeId) -> &str {
        self.node(n).name.2
    }

    fn tag_key(&self, n: NodeId) -> TagKey<'_> {
        TagKey::Name(self.node(n).name.1)
    }

    fn attribute_count(&self, n: NodeId) -> u32 {
        self.node(n).attrs.len() as u32
    }

    fn attribute(&self, n: NodeId, i: u32) -> Option<Attr<'_>> {
        self.node(n).attrs.get(i as usize).copied()
    }

    fn data(&self, n: NodeId) -> &str {
        self.node(n).data
    }

    fn pi_target(&self, n: NodeId) -> Option<&str> {
        self.node(n).target
    }

    fn is_xml(&self) -> bool {
        true
    }

    fn id(&self, n: NodeId) -> Option<&str> {
        self.attr_named(n, "xml:id")
    }

    fn lang(&self, n: NodeId) -> Option<&str> {
        self.attr_named(n, "xml:lang")
    }
}

/// Each result as text: an element's name, `@name=value`, or the scalar.
fn get(query: &str) -> Vec<String> {
    let tree = Hand::new();
    let index = Index::build(&tree, &[0]);
    let doc = Doc::new(&tree, &index);
    let ns = [("r".to_string(), "urn:r".to_string())];
    let value = XPath::new(query)
        .and_then(|x| x.evaluate(doc, 0, &[], &ns, &NoExtensions))
        .unwrap_or_else(|e| panic!("{e}"));
    match value {
        Value::Nodes(items) => items
            .iter()
            .map(|i| match *i {
                Item::Node(n) => tree.qualified_name(n).to_string(),
                Item::Attr { owner, index } => {
                    let (name, value) = doc.attr_at(owner, index);
                    format!("@{name}={value}")
                }
            })
            .collect(),
        Value::Str(s) => vec![s.into_owned()],
        Value::Num(n) => vec![n.to_string()],
        Value::Bool(b) => vec![b.to_string()],
    }
}

fn one(query: &str) -> String {
    let all = get(query);
    assert_eq!(all.len(), 1, "{query}: {all:?}");
    all.into_iter().next().unwrap()
}

#[test]
fn names_and_namespaces_come_from_the_tree() {
    assert_eq!(get("//r:*"), ["r:doc", "r:item"]);
    assert_eq!(get("//r:item"), ["r:item"]);
    assert!(get("//item").is_empty(), "no prefix is no namespace");
    assert_eq!(get("//Item"), ["Item"]);
    assert!(get("//ITEM").is_empty(), "XML names compare exactly");
    assert_eq!(one("name(//r:item)"), "r:item");
    assert_eq!(one("local-name(//r:item)"), "item");
    assert_eq!(one("namespace-uri(//r:item)"), "urn:r");
    assert_eq!(one("namespace-uri(//Item)"), "");
    assert_eq!(get("//Item/preceding-sibling::*"), ["r:item"]);
    assert_eq!(get("//r:item/following-sibling::node()"), ["Item", "", ""]);
    assert_eq!(get("//r:item/.."), ["r:doc"]);
}

#[test]
fn attributes_come_from_the_tree() {
    assert_eq!(get("//r:item/@*"), ["@xml:id=a", "@r:kind=x", "@plain=1"]);
    // The tree leaves out xmlns:r, so only xml:lang is left.
    assert_eq!(get("/r:doc/@*"), ["@xml:lang=en-GB"]);
    assert_eq!(get("//r:item/@r:kind"), ["@r:kind=x"]);
    assert!(get("//r:item/@kind").is_empty());
    assert_eq!(one("local-name(//r:item/@r:kind)"), "kind");
    assert_eq!(one("namespace-uri(//r:item/@r:kind)"), "urn:r");
    assert_eq!(one("count(//@r:*)"), "1");
    assert_eq!(get("//*[@plain = '1']"), ["r:item"]);
    assert_eq!(one("string(//@xml:id)"), "a");
}

#[test]
fn text_comments_and_instructions_come_from_the_tree() {
    assert_eq!(one("string(/)"), "hellobonjour");
    assert_eq!(one("string(//r:item)"), "hello");
    assert_eq!(one("string(//processing-instruction('pi'))"), "data");
    assert_eq!(one("name(//processing-instruction())"), "pi");
    assert_eq!(one("string(//comment())"), "c");
    assert_eq!(one("count(//text())"), "2");
}

#[test]
fn id_and_lang_ask_the_tree() {
    assert_eq!(get("id('a')"), ["r:item"]);
    assert_eq!(get("id('b a')"), ["r:item"]);
    assert!(get("id('plain')").is_empty());
    assert_eq!(get("//*[lang('en')]"), ["r:doc", "r:item"]);
    assert_eq!(get("//*[lang('fr')]"), ["Item"]);
    assert_eq!(one("count(//text()[lang('en-gb')])"), "1");
}
