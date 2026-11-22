//! XPath over a tree of the test's own, not netweir-html's: names,
//! namespaces, attributes, text, `id()` and `lang()` all come through the
//! `Tree` trait, so this builds and runs without the `html` feature.

use std::time::{Duration, Instant};

use netweir_xpath::{
    Attr, Doc, Index, Item, NoExtensions, NodeId, NodeType, TagKey, Tree, Value, XML_NS, XPath,
};

struct Node {
    kind: NodeType,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    /// Its place among its parent's children.
    at: usize,
    /// An element's prefix, local name and namespace URI.
    name: (&'static str, &'static str, &'static str),
    attrs: Vec<Attr<'static>>,
    /// The `xmlns` declarations, as (prefix, URI).
    decls: Vec<(&'static str, &'static str)>,
    /// Text, or a processing instruction's target and data.
    data: &'static str,
    target: Option<&'static str>,
}

/// A document laid out by hand, as an XML parser of its own would hold it:
///
/// ```xml
/// <r:doc xmlns:r="urn:r" xmlns="urn:d" xml:lang="en-GB">
///   <r:item xmlns:s="urn:s" xml:id="a" r:kind="x" plain="1">hello</r:item>
///   <Item xmlns="" xmlns:r="urn:r2" xml:lang="fr">bonjour</Item>
///   <?pi data?><!--c-->
/// </r:doc>
/// ```
///
/// The `xmlns` declarations are left out of the attributes, as the trait
/// asks, and given as declarations.
struct Hand {
    nodes: Vec<Node>,
    qnames: Vec<String>,
}

/// A node of `kind` under `parent`, with nothing else set.
fn blank(kind: NodeType, parent: Option<NodeId>) -> Node {
    Node {
        kind,
        parent,
        children: Vec::new(),
        at: 0,
        name: ("", "", ""),
        attrs: Vec::new(),
        decls: Vec::new(),
        data: "",
        target: None,
    }
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
        let node = blank;
        let mut hand = Hand::empty();
        let mut add = |n: Node| hand.add(n);
        let doc = add(Node {
            name: ("r", "doc", "urn:r"),
            attrs: vec![attr("xml:lang", XML_NS, "en-GB")],
            decls: vec![("r", "urn:r"), ("", "urn:d")],
            ..node(NodeType::Element, Some(0))
        });
        let item = add(Node {
            name: ("r", "item", "urn:r"),
            attrs: vec![
                attr("xml:id", XML_NS, "a"),
                attr("r:kind", "urn:r", "x"),
                attr("plain", "", "1"),
            ],
            decls: vec![("s", "urn:s")],
            ..node(NodeType::Element, Some(doc))
        });
        add(Node {
            data: "hello",
            ..node(NodeType::Text, Some(item))
        });
        let other = add(Node {
            name: ("", "Item", ""),
            attrs: vec![attr("xml:lang", XML_NS, "fr")],
            decls: vec![("", ""), ("r", "urn:r2")],
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
        hand
    }

    /// A tree of only a document node.
    fn empty() -> Hand {
        Hand {
            nodes: vec![blank(NodeType::Document, None)],
            qnames: vec![String::new()],
        }
    }

    fn add(&mut self, mut n: Node) -> NodeId {
        let id = self.nodes.len() as NodeId;
        if let Some(p) = n.parent {
            let siblings = &mut self.nodes[p as usize].children;
            n.at = siblings.len();
            siblings.push(id);
        }
        self.qnames.push(match n.name {
            ("", local, _) => local.to_string(),
            (p, local, _) => format!("{p}:{local}"),
        });
        self.nodes.push(n);
        id
    }

    fn node(&self, n: NodeId) -> &Node {
        &self.nodes[n as usize]
    }

    /// The node's place among its parent's children, and those children.
    fn siblings(&self, n: NodeId) -> Option<(usize, &[NodeId])> {
        let node = self.node(n);
        Some((node.at, &self.node(node.parent?).children))
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

    fn namespace_declaration_count(&self, n: NodeId) -> u32 {
        self.node(n).decls.len() as u32
    }

    fn namespace_declaration(&self, n: NodeId, i: u32) -> Option<(&str, &str)> {
        self.node(n).decls.get(i as usize).copied()
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

/// Each result as text: an element's name, `@name=value`, `ns:prefix=uri`,
/// or the scalar.
fn get(query: &str) -> Vec<String> {
    get_in(&Hand::new(), query)
}

fn get_in(tree: &Hand, query: &str) -> Vec<String> {
    let index = Index::build(tree, &[0]);
    let doc = Doc::new(tree, &index);
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
                Item::Ns { decl, .. } => {
                    let (prefix, uri) = doc.namespace_at(decl);
                    format!("ns:{prefix}={uri}")
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

const XML: &str = "ns:xml=http://www.w3.org/XML/1998/namespace";

#[test]
fn namespace_axis_has_what_is_in_scope() {
    // `xml` first, then the nearest binding of each prefix, those of the
    // outermost element first and each element's last written first, as
    // libxml2 and lxml order them.
    assert_eq!(get("/r:doc/namespace::*"), [XML, "ns:=urn:d", "ns:r=urn:r"]);
    assert_eq!(
        get("//r:item/namespace::*"),
        [XML, "ns:=urn:d", "ns:r=urn:r", "ns:s=urn:s"]
    );
    // xmlns="" takes the default away, and xmlns:r rebinds r.
    assert_eq!(get("//Item/namespace::*"), [XML, "ns:r=urn:r2"]);
    assert_eq!(get("//Item/namespace::node()"), [XML, "ns:r=urn:r2"]);
    assert_eq!(get("//r:item/namespace::s"), ["ns:s=urn:s"]);
    assert_eq!(get("//Item/namespace::xml"), [XML]);
    assert!(get("//Item/namespace::s").is_empty());
    assert_eq!(get("/r:doc/namespace::*[2]"), ["ns:=urn:d"]);
    assert_eq!(get("/r:doc/namespace::*[last()]"), ["ns:r=urn:r"]);
    // Only elements have namespace nodes, and only on this axis.
    assert!(get("/namespace::*").is_empty());
    assert!(get("//text()/namespace::*").is_empty());
    assert!(get("//@plain/namespace::*").is_empty());
    assert!(get("/r:doc/namespace::text()").is_empty());
    assert!(get("/r:doc/namespace::r:*").is_empty());
    assert!(get("/r:doc/namespace::r:r").is_empty());
    assert!(get("/r:doc/child::r").is_empty());
    assert_eq!(one("count(//namespace::*)"), "9");
    assert_eq!(get("//*[namespace::s]"), ["r:item"]);
    assert_eq!(get("//*[namespace::r = 'urn:r2']"), ["Item"]);
}

#[test]
fn namespace_nodes_have_names_values_and_parents() {
    // The name is the prefix, "" for the default; the value is the URI.
    assert_eq!(one("name(//r:item/namespace::s)"), "s");
    assert_eq!(one("local-name(//r:item/namespace::s)"), "s");
    assert_eq!(one("namespace-uri(//r:item/namespace::s)"), "");
    assert_eq!(one("name(/r:doc/namespace::*[. = 'urn:d'])"), "");
    assert_eq!(one("string(//r:item/namespace::s)"), "urn:s");
    assert_eq!(one("string(//Item/namespace::r)"), "urn:r2");
    assert_eq!(one("string(/r:doc/namespace::xml)"), XML_NS);
    assert_eq!(get("//r:item/namespace::s/.."), ["r:item"]);
    assert_eq!(get("//r:item/namespace::s/parent::r:item"), ["r:item"]);
    assert_eq!(get("//r:item/namespace::*/.."), ["r:item"]);
    assert_eq!(get("//r:item/namespace::s/self::node()"), ["ns:s=urn:s"]);
    assert_eq!(
        get("//r:item/namespace::s/ancestor::*"),
        ["r:doc", "r:item"]
    );
    assert_eq!(
        get("//r:item/namespace::s/ancestor-or-self::node()"),
        ["", "r:doc", "r:item", "ns:s=urn:s"]
    );
    assert!(get("//r:item/namespace::s/child::node()").is_empty());
    assert!(get("//r:item/namespace::s/attribute::*").is_empty());
    // The element's descendants follow its namespace nodes.
    assert_eq!(get("//r:item/namespace::s/following::*"), ["Item"]);
    assert_eq!(get("//Item/namespace::r/preceding::*"), ["r:item"]);
    assert_eq!(get("//r:item/namespace::s/following::text()[1]"), [""]);
    assert_eq!(one("count(//r:item/namespace::s[lang('en')])"), "1");
}

#[test]
fn namespace_nodes_sort_between_element_and_attributes() {
    assert_eq!(
        get("/r:doc/@* | /r:doc/namespace::r | /r:doc"),
        ["r:doc", "ns:r=urn:r", "@xml:lang=en-GB"]
    );
    assert_eq!(
        get("//r:item/@plain | /r:doc/namespace::* | //r:item/namespace::xml"),
        [XML, "ns:=urn:d", "ns:r=urn:r", XML, "@plain=1"]
    );
    // The same namespace node twice is one node; one of another element
    // with the same prefix and URI is another.
    assert_eq!(one("count(/r:doc/namespace::* | /r:doc/namespace::r)"), "3");
    assert_eq!(
        one("count(/r:doc/namespace::r | //r:item/namespace::r)"),
        "2"
    );
    assert_eq!(
        get("(//r:item/namespace::s | /r:doc/namespace::r | //Item)/.."),
        ["r:doc", "r:item"]
    );
}

/// A tree of `depth` nested elements, each declaring what `decls(level)`
/// gives, under a document node.
fn nested(depth: usize, decls: impl Fn(usize) -> Vec<(&'static str, &'static str)>) -> Hand {
    let mut tree = Hand::empty();
    let mut parent = 0;
    for level in 0..depth {
        parent = tree.add(Node {
            name: ("", "e", ""),
            decls: decls(level),
            ..blank(NodeType::Element, Some(parent))
        });
    }
    tree
}

/// One element declaring `width` prefixes and carrying as many attributes.
fn wide(width: usize) -> Hand {
    let mut tree = Hand::empty();
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    tree.add(Node {
        name: ("", "w", ""),
        decls: (0..width)
            .map(|i| (leak(format!("p{i}")), leak(format!("urn:{i}"))))
            .collect(),
        attrs: (0..width)
            .map(|i| attr(leak(format!("a{i}")), "", "v"))
            .collect(),
        ..blank(NodeType::Element, Some(0))
    });
    tree
}

fn leaked(prefix: &str, i: usize) -> &'static str {
    Box::leak(format!("{prefix}{i}").into_boxed_str())
}

/// How long `query` takes on `tree`, best of three, and what it counts.
fn time(tree: &Hand, query: &str) -> (Duration, String) {
    let mut best = Duration::MAX;
    let mut got = String::new();
    for _ in 0..3 {
        let start = Instant::now();
        got = get_in(tree, query).concat();
        best = best.min(start.elapsed());
    }
    (best, got)
}

/// Every level rebinding the default namespace and `p`.
fn rebinding(depth: usize) -> Hand {
    nested(depth, |_| vec![("", "urn:d"), ("p", "urn:p")])
}

/// Every level declaring a prefix of its own.
fn adding(depth: usize) -> Hand {
    nested(depth, |i| vec![(leaked("p", i), leaked("urn:", i))])
}

/// Half as deep as `rebinding`, then as many leaves under the deepest,
/// each declaring `q`.
fn comb(size: usize) -> Hand {
    let mut tree = rebinding(size / 2);
    let deepest = (size / 2) as NodeId;
    for _ in 0..size / 2 {
        tree.add(Node {
            name: ("", "leaf", ""),
            decls: vec![("q", "urn:q")],
            ..blank(NodeType::Element, Some(deepest))
        });
    }
    tree
}

/// Builds a tree of a given size.
type Build = fn(usize) -> Hand;

#[test]
fn namespace_axis_stays_linear() {
    // Deep trees and a wide element: ten times the size should take about
    // ten times as long, not a hundred. Each answer is `a * size + b`.
    let cases: [(Build, &str, usize, usize); 10] = [
        (rebinding, "count(//*/namespace::*)", 3, 0),
        (rebinding, "count(//*/namespace::p)", 1, 0),
        (rebinding, "count(//*[namespace::p = 'urn:p'])", 1, 0),
        (rebinding, "count(//namespace::*/..)", 1, 0),
        (comb, "count(//leaf/namespace::*)", 2, 0),
        (adding, "count(/descendant::*[last()]/namespace::*)", 1, 1),
        (adding, "count(/descendant::*[last()]/namespace::p0)", 0, 1),
        (wide, "count(/w/namespace::*)", 1, 1),
        (wide, "count(/w/namespace::* | /w/@*)", 2, 1),
        (
            wide,
            "count(/w/@*[local-name() = 'a7']) + count(/w/@a7)",
            0,
            2,
        ),
    ];
    let want = |a: usize, b: usize, n: usize| (a * n + b).to_string();
    for (build, query, a, b) in cases {
        let (small, large) = (build(2_000), build(20_000));
        let (t1, got1) = time(&small, query);
        let (t2, got2) = time(&large, query);
        assert_eq!(got1, want(a, b, 2_000), "{query}");
        assert_eq!(got2, want(a, b, 20_000), "{query}");
        // A floor, so a fast small run doesn't make timer noise a failure.
        let ratio = t2.as_secs_f64() / t1.as_secs_f64().max(1e-3);
        assert!(ratio < 20.0, "{query}: {t1:?}, then {t2:?}");
    }
}

#[test]
fn namespace_work_counts_against_the_limit() {
    let tree = rebinding(2_000);
    let index = Index::build(&tree, &[0]);
    let run = |limit| {
        XPath::new("count(//*/namespace::*)")
            .unwrap()
            .evaluate_within(Doc::new(&tree, &index), 0, &[], &[], &NoExtensions, limit)
            .map(|v| matches!(v, Value::Num(n) if n == 6_000.0))
    };
    let reason = run(5_000).unwrap_err().reason;
    assert!(reason.contains("more than 5000 steps"), "{reason}");
    // A few units for each namespace node and declaration.
    assert_eq!(run(80_000), Ok(true));
}
