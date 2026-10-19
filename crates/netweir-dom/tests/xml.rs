//! XML documents: parsing, namespaces in XPath, writing XML back out, and
//! the limits that keep a hostile document from costing more than it's
//! worth. Expected answers are what lxml gives through Scrapy's selectors.

use std::time::Duration;

use netweir_dom::xml::{XmlOptions, html_entity};
use netweir_dom::{Document, Hit, NodeKind, ParseXmlError, Query, XPath, XmlError, XmlNodes};

const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<?xml-stylesheet type="text/xsl" href="/feed.xsl"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/"
     xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title>Example &amp; Co</title>
    <link>https://example.com/</link>
    <!-- two items -->
    <item>
      <title>First</title>
      <link>https://example.com/1</link>
      <dc:creator>Ann</dc:creator>
      <content:encoded><![CDATA[<p>Hello <b>there</b></p>]]></content:encoded>
      <guid isPermaLink="false">a1</guid>
    </item>
    <item>
      <title>Second</title>
      <link>https://example.com/2</link>
      <dc:creator>Bob</dc:creator>
    </item>
  </channel>
</rss>"#;

const ATOM: &str = r#"<feed xmlns="http://www.w3.org/2005/Atom" xml:lang="en">
  <title>Atom feed</title>
  <entry>
    <title type="html">One</title>
    <link rel="alternate" href="https://example.com/one"/>
    <id>urn:uuid:1</id>
  </entry>
  <entry>
    <title>Two</title>
    <link rel="alternate" href="https://example.com/two"/>
  </entry>
</feed>"#;

const SOAP: &str = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <soap:Body>
    <GetPriceResponse xmlns="urn:Prices">
      <Price xsi:type="decimal">12.50</Price>
      <price>lowercase is another element</price>
    </GetPriceResponse>
  </soap:Body>
</soap:Envelope>"#;

const ATOM_NS: &str = "http://www.w3.org/2005/Atom";

fn ns(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(p, u)| (p.to_string(), u.to_string()))
        .collect()
}

fn xpath_in(doc: &Document, query: &str, namespaces: &[(&str, &str)]) -> Vec<String> {
    XPath::new(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
        .run_in(doc.root(), &[], &ns(namespaces))
        .unwrap_or_else(|e| panic!("{query}: {e}"))
        .iter()
        .map(Hit::to_string_value)
        .collect()
}

fn xpath(doc: &Document, query: &str) -> Vec<String> {
    xpath_in(doc, query, &[])
}

fn recover() -> XmlOptions {
    XmlOptions {
        recover: true,
        budget: Duration::MAX,
    }
}

fn bad(xml: &str) -> XmlError {
    match Document::parse_xml(xml) {
        Ok(doc) => panic!("parsed: {}", doc.root().html()),
        Err(e) => e,
    }
}

// --- reading -----------------------------------------------------------------

#[test]
fn an_rss_feed_is_read_as_written() {
    let doc = Document::parse_xml(RSS).unwrap();
    assert!(doc.is_xml());
    assert_eq!(xpath(&doc, "//item/title/text()"), ["First", "Second"]);
    assert_eq!(xpath(&doc, "/rss/channel/title/text()"), ["Example & Co"]);
    // HTML would make <link> a void element and lose its URL.
    assert_eq!(
        xpath(&doc, "//item/link/text()"),
        ["https://example.com/1", "https://example.com/2"]
    );
    assert_eq!(
        xpath(&doc, "//item[1]/link"),
        [concat!(
            r#"<link xmlns:dc="http://purl.org/dc/elements/1.1/""#,
            r#" xmlns:content="http://purl.org/rss/1.0/modules/content/">"#,
            "https://example.com/1</link>"
        )]
    );
    assert_eq!(xpath(&doc, "//guid/@isPermaLink"), ["false"]);
    assert_eq!(xpath(&doc, "count(//item)"), ["2.0"]);
}

#[test]
fn cdata_is_text_and_joins_the_text_beside_it() {
    let doc = Document::parse_xml("<a>x<![CDATA[<y> & ]]>&lt;z</a>").unwrap();
    assert_eq!(xpath(&doc, "/a/text()"), ["x<y> & <z"]);
    assert_eq!(xpath(&doc, "/a"), ["<a>x&lt;y&gt; &amp; &lt;z</a>"]);
}

#[test]
fn prefixed_names_need_their_namespace() {
    let doc = Document::parse_xml(RSS).unwrap();
    let dc = [("dc", "http://purl.org/dc/elements/1.1/")];
    assert_eq!(xpath_in(&doc, "//dc:creator/text()", &dc), ["Ann", "Bob"]);
    // Any prefix will do, as long as it's bound to the same URI.
    assert_eq!(
        xpath_in(&doc, "//x:creator/text()", &[("x", dc[0].1)]),
        ["Ann", "Bob"]
    );
    let err = XPath::new("//dc:creator")
        .unwrap()
        .run(doc.root())
        .unwrap_err();
    assert!(
        err.reason.contains("undefined namespace prefix \"dc\""),
        "{err}"
    );
    // A namespace nothing in the document uses matches nothing.
    assert!(xpath_in(&doc, "//x:creator", &[("x", "urn:other")]).is_empty());
    assert_eq!(xpath_in(&doc, "count(//dc:*)", &dc), ["2.0"]);
}

#[test]
fn a_default_namespace_is_a_namespace() {
    let doc = Document::parse_xml(ATOM).unwrap();
    // As in lxml: a name without a prefix is in no namespace.
    assert!(xpath(&doc, "//entry").is_empty());
    let atom = [("atom", ATOM_NS)];
    assert_eq!(
        xpath_in(&doc, "//atom:entry/atom:title/text()", &atom),
        ["One", "Two"]
    );
    assert_eq!(
        xpath_in(&doc, "//atom:link/@href", &atom),
        ["https://example.com/one", "https://example.com/two"]
    );
    assert_eq!(xpath_in(&doc, "name(//atom:entry)", &atom), ["entry"]);
    assert_eq!(
        xpath_in(&doc, "namespace-uri(//atom:entry)", &atom),
        [ATOM_NS]
    );
    // The xml prefix is always bound.
    assert_eq!(xpath(&doc, "/*/@xml:lang"), ["en"]);
    assert_eq!(xpath(&doc, "count(/*[lang('en')])"), ["1.0"]);
}

#[test]
fn names_keep_their_case_and_prefix() {
    let doc = Document::parse_xml(SOAP).unwrap();
    let ns = [
        ("soap", "http://schemas.xmlsoap.org/soap/envelope/"),
        ("p", "urn:Prices"),
        ("xsi", "http://www.w3.org/2001/XMLSchema-instance"),
    ];
    assert_eq!(xpath_in(&doc, "name(/*)", &ns), ["soap:Envelope"]);
    assert_eq!(xpath_in(&doc, "local-name(/*)", &ns), ["Envelope"]);
    assert!(xpath_in(&doc, "/soap:envelope", &ns).is_empty());
    assert_eq!(
        xpath_in(
            &doc,
            "/soap:Envelope/soap:Body/p:GetPriceResponse/p:Price/text()",
            &ns
        ),
        ["12.50"]
    );
    assert_eq!(
        xpath_in(&doc, "//p:price/text()", &ns),
        ["lowercase is another element"]
    );
    assert_eq!(xpath_in(&doc, "//p:Price/@xsi:type", &ns), ["decimal"]);
    assert_eq!(xpath_in(&doc, "name(//p:Price/@*)", &ns), ["xsi:type"]);
    assert_eq!(xpath_in(&doc, "local-name(//p:Price/@*)", &ns), ["type"]);
    assert_eq!(
        xpath_in(&doc, "namespace-uri(//p:Price/@*)", &ns),
        [ns[2].1]
    );
    // An attribute without a prefix is in no namespace.
    assert!(xpath_in(&doc, "//p:Price/@type", &ns).is_empty());
    assert!(xpath_in(&doc, "//p:Price/@p:type", &ns).is_empty());
    let price = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("Price"))
        .unwrap();
    assert_eq!(price.qualified_name(), "Price");
    assert_eq!(price.namespace(), Some("urn:Prices"));
    assert_eq!(price.attrs(), [("xsi:type", "decimal")]);
    assert_eq!(price.attr("xsi:type"), Some("decimal"));
    assert_eq!(price.attr("type"), None);
}

#[test]
fn namespace_declarations_are_not_attributes() {
    let doc = Document::parse_xml(ATOM).unwrap();
    assert_eq!(xpath(&doc, "count(/*/@*)"), ["1.0"]);
    assert_eq!(
        doc.root().first_child().unwrap().attrs(),
        [("xml:lang", "en")]
    );
}

#[test]
fn an_element_written_out_declares_the_namespaces_it_inherits() {
    let doc = Document::parse_xml(ATOM).unwrap();
    let atom = [("atom", ATOM_NS)];
    assert_eq!(
        xpath_in(&doc, "//atom:entry[2]", &atom),
        [concat!(
            r#"<entry xmlns="http://www.w3.org/2005/Atom">"#,
            "\n    <title>Two</title>\n    ",
            r#"<link rel="alternate" href="https://example.com/two"/>"#,
            "\n  </entry>"
        )]
    );
    let doc = Document::parse_xml(RSS).unwrap();
    let dc = [("dc", "http://purl.org/dc/elements/1.1/")];
    assert_eq!(
        xpath_in(&doc, "//dc:creator[1]", &dc)[0],
        r#"<dc:creator xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:content="http://purl.org/rss/1.0/modules/content/">Ann</dc:creator>"#
    );
}

#[test]
fn writing_escapes_what_it_must() {
    let doc =
        Document::parse_xml(r#"<a t="1 &lt; 2 &amp; &quot;x&quot;&#10;y">a &gt; b</a>"#).unwrap();
    assert_eq!(
        doc.root().html(),
        r#"<a t="1 &lt; 2 &amp; &quot;x&quot;&#10;y">a &gt; b</a>"#
    );
}

#[test]
fn attribute_values_are_normalised() {
    let doc = Document::parse_xml("<a t=\"one\ttwo\r\nthree\nfour\" u='&#9;tab'/>").unwrap();
    assert_eq!(xpath(&doc, "/a/@t"), ["one two three four"]);
    assert_eq!(xpath(&doc, "/a/@u"), ["\ttab"]);
}

#[test]
fn line_ends_become_newlines() {
    let doc = Document::parse_xml("<a>one\r\ntwo\rthree</a>").unwrap();
    assert_eq!(xpath(&doc, "/a/text()"), ["one\ntwo\nthree"]);
}

#[test]
fn comments_and_processing_instructions_are_kept() {
    let doc = Document::parse_xml(RSS).unwrap();
    assert_eq!(xpath(&doc, "//comment()"), ["<!-- two items -->"]);
    assert_eq!(
        xpath(&doc, "/processing-instruction('xml-stylesheet')"),
        [r#"<?xml-stylesheet type="text/xsl" href="/feed.xsl"?>"#]
    );
    assert_eq!(
        xpath(&doc, "string(/processing-instruction())"),
        [r#"type="text/xsl" href="/feed.xsl""#]
    );
    assert_eq!(
        xpath(&doc, "name(/processing-instruction())"),
        ["xml-stylesheet"]
    );
    assert!(xpath(&doc, "/processing-instruction('other')").is_empty());
    let pi = doc.root().first_child().unwrap();
    assert_eq!(pi.kind(), NodeKind::ProcessingInstruction);
    assert_eq!(pi.pi_target(), Some("xml-stylesheet"));
}

#[test]
fn the_document_writes_its_top_level_nodes_a_line_each() {
    let doc = Document::parse_xml("<?pi x?>\n<!--c-->\n<r/>\n<!--after-->").unwrap();
    assert_eq!(doc.root().html(), "<?pi x?>\n<!--c-->\n<r/>\n<!--after-->");
}

#[test]
fn an_empty_document_is_an_error_except_when_recovering() {
    for empty in ["", "  \n", "\u{feff}"] {
        assert_eq!(bad(empty).reason, "Document is empty");
        let (doc, repairs) = Document::parse_xml_with(empty, recover()).unwrap();
        assert!(repairs.is_empty());
        assert!(doc.root().first_child().is_none());
        assert!(xpath(&doc, "//*").is_empty());
    }
}

#[test]
fn a_byte_order_mark_is_skipped() {
    let doc = Document::parse_xml("\u{feff}<?xml version=\"1.0\"?><a/>").unwrap();
    assert_eq!(doc.root().html(), "<a/>");
}

#[test]
fn css_matches_xml_names_ignoring_namespaces_and_case() {
    let doc = Document::parse_xml(ATOM).unwrap();
    let titles = Query::new("entry > title::text")
        .unwrap()
        .strings(doc.root());
    assert_eq!(titles, ["One", "Two"]);
    let hrefs = Query::new("link::attr(href)").unwrap().strings(doc.root());
    assert_eq!(
        hrefs,
        ["https://example.com/one", "https://example.com/two"]
    );
    let doc = Document::parse_xml(SOAP).unwrap();
    assert_eq!(Query::new("Price").unwrap().strings(doc.root()).len(), 2);
}

#[test]
fn remove_namespaces_lets_plain_names_match() {
    let doc = Document::parse_xml(SOAP).unwrap();
    unsafe { doc.remove_namespaces() }.unwrap();
    assert_eq!(
        xpath(&doc, "/Envelope/Body/GetPriceResponse/Price/text()"),
        ["12.50"]
    );
    assert_eq!(xpath(&doc, "//Price/@type"), ["decimal"]);
    assert_eq!(xpath(&doc, "name(/*)"), ["Envelope"]);
    assert_eq!(
        xpath(&doc, "//Price"),
        [r#"<Price type="decimal">12.50</Price>"#]
    );
    assert!(!doc.root().html().contains("xmlns"));
    let doc = Document::parse_xml(ATOM).unwrap();
    unsafe { doc.remove_namespaces() }.unwrap();
    assert_eq!(xpath(&doc, "//entry/title/text()"), ["One", "Two"]);
    // xml:lang loses its prefix too, as in parsel.
    assert_eq!(xpath(&doc, "/feed/@lang"), ["en"]);
}

#[test]
fn remove_namespaces_keeps_the_prefixed_one_of_two_same_names() {
    let doc =
        Document::parse_xml(r#"<a xmlns:x="urn:x" n="plain" x:n="prefixed" o="1"/>"#).unwrap();
    unsafe { doc.remove_namespaces() }.unwrap();
    assert_eq!(
        doc.root().first_child().unwrap().attrs(),
        [("n", "prefixed"), ("o", "1")]
    );
}

#[test]
fn remove_namespaces_does_nothing_to_html() {
    let doc = Document::parse("<p>x</p>");
    unsafe { doc.remove_namespaces() }.unwrap();
    assert!(!doc.is_xml());
}

#[test]
fn new_elements_in_xml_keep_their_case() {
    let doc = Document::parse_xml("<Root/>").unwrap();
    let root = doc.root().first_child().unwrap();
    let el = unsafe { doc.create_element("NewThing", &[("Kind", "x")]) }.unwrap();
    unsafe { doc.insert(root.id(), None, &[el]) }.unwrap();
    assert_eq!(doc.root().html(), r#"<Root><NewThing Kind="x"/></Root>"#);
    assert!(unsafe { doc.create_element("a:b", &[]) }.is_err());
    assert!(unsafe { doc.create_element("a", &[("x", "1"), ("x", "2")]) }.is_err());
}

#[test]
fn text_and_pretty_printing() {
    let doc = Document::parse_xml(RSS).unwrap();
    let item = doc
        .root()
        .descendants()
        .find(|n| n.tag() == Some("item"))
        .unwrap();
    assert!(item.text().contains("First"));
    assert!(
        item.readable_text_pieces()
            .contains(&"<p>Hello <b>there</b></p>")
    );
    let pretty = Document::parse_xml("<a><b x='1'>t</b><c/></a>")
        .unwrap()
        .root()
        .pretty();
    assert_eq!(pretty, "<a>\n <b x=\"1\">\n  t\n </b>\n <c/>\n</a>\n");
}

#[test]
fn an_html_document_rejects_unbound_prefixes_and_matches_none() {
    let doc = Document::parse("<p>x</p>");
    let err = XPath::new("//a:p").unwrap().run(doc.root()).unwrap_err();
    assert!(err.reason.contains("undefined namespace prefix"), "{err}");
    let hits = XPath::new("//a:p")
        .unwrap()
        .run_in(doc.root(), &[], &ns(&[("a", "urn:a")]))
        .unwrap();
    assert!(hits.is_empty());
}

// --- not well-formed -----------------------------------------------------------

#[test]
fn malformed_documents_say_what_and_where() {
    let e = bad("<a>\n  <b></c>\n</a>");
    assert_eq!((e.line, e.column), (2, 6), "{e}");
    assert!(e.reason.contains("</c> doesn't close <b>"), "{e}");
    assert!(
        e.to_string()
            .starts_with("not well-formed XML at line 2, column 6:"),
        "{e}"
    );

    for (xml, says) in [
        ("<a><b></a>", "doesn't close <b>"),
        ("<a>", "ends inside <a>"),
        ("<a><b>", "ends inside <a><b>"),
        ("<a/><b/>", "a document has one root"),
        ("<a/>text", "after the root element"),
        ("text<a/>", "outside the root element"),
        ("</a>", "never started"),
        ("<x:a/>", "prefix x of <x:a> isn't declared"),
        (
            "<a x:b='1'/>",
            "prefix x of the attribute x:b isn't declared",
        ),
        ("<a b='1' b='2'/>", "b"),
        (
            r#"<a xmlns:p="u" xmlns:q="u" p:b="1" q:b="2"/>"#,
            "the same attribute twice",
        ),
        ("<a>\u{1}</a>", "U+0001"),
        ("<a>&#0;</a>", "doesn't allow"),
        ("<a>&#xZZ;</a>", "isn't a valid character reference"),
        ("<a b='<'/>", "`<` isn't allowed"),
        ("<a>&nbsp;</a>", "&nbsp; isn't declared"),
        ("<a>& b</a>", ""),
        ("<1a/>", "isn't a valid element name"),
        ("<a xmlns:xml='urn:x'/>", "xml prefix"),
        ("<a xmlns:p=''/>", "can't be empty"),
        ("<a><!-- a -- b --></a>", ""),
        ("<?xml version='2.0'?><a/>", "isn't 1.0 or 1.1"),
        ("<a/><?xml version='1.0'?>", "only come at the very start"),
        ("<a/> <?xml version='1.0'?>", "only come at the very start"),
        (
            "<!-- c --><?xml version='1.0'?><a/>",
            "only come at the very start",
        ),
        ("<!-- only a comment -->", "no root element"),
        ("<a/><!DOCTYPE a>", "after the root element"),
        ("", "Document is empty"),
        (" \n\t", "Document is empty"),
        ("<a>]]></a>", ""),
    ] {
        let e = bad(xml);
        assert!(e.reason.contains(says), "{xml:?}: {e}");
    }
}

#[test]
fn an_error_column_counts_characters() {
    let e = bad("<a>é</b>");
    assert_eq!((e.line, e.column), (1, 5), "{e}");
}

// --- hostile documents -----------------------------------------------------------

#[test]
fn declared_entities_are_expanded() {
    let doc = Document::parse_xml(
        r#"<!DOCTYPE r [
  <!ENTITY co "Example &amp; Co">
  <!ENTITY bold "<b>bold &co;</b>">
  <!ENTITY nbsp "&#160;">
]>
<r t="&co;!">x&nbsp;&bold;y</r>"#,
    )
    .unwrap();
    assert_eq!(xpath(&doc, "/r/@t"), ["Example & Co!"]);
    assert_eq!(xpath(&doc, "/r/text()"), ["x\u{a0}", "y"]);
    assert_eq!(xpath(&doc, "/r/b/text()"), ["bold Example & Co"]);
}

#[test]
fn a_billion_laughs_is_refused() {
    let mut xml = String::from("<?xml version=\"1.0\"?><!DOCTYPE lolz [<!ENTITY lol \"lol\">");
    for i in 1..10 {
        let prev = if i == 1 {
            "&lol;".repeat(10)
        } else {
            format!("&lol{};", i - 1).repeat(10)
        };
        xml.push_str(&format!("<!ENTITY lol{i} \"{prev}\">"));
    }
    xml.push_str("]><lolz>&lol9;</lolz>");
    let e = bad(&xml);
    assert!(e.reason.contains("billion laughs"), "{e}");
    // In an attribute too.
    let attr = xml.replace("<lolz>&lol9;</lolz>", "<lolz a='&lol9;'/>");
    assert!(bad(&attr).reason.contains("billion laughs"));
}

#[test]
fn one_big_entity_used_many_times_is_refused() {
    // The "quadratic blowup": no nesting, just repetition.
    let big = "x".repeat(100_000);
    let uses = "&big;".repeat(2_000);
    let xml = format!("<!DOCTYPE r [<!ENTITY big \"{big}\">]><r>{uses}</r>");
    assert!(bad(&xml).reason.contains("billion laughs"));
}

#[test]
fn entities_that_refer_to_themselves_are_refused() {
    let e = bad(r#"<!DOCTYPE r [<!ENTITY a "&b;"><!ENTITY b "&a;">]><r>&a;</r>"#);
    assert!(e.reason.contains("refers to itself"), "{e}");
    let e = bad(r#"<!DOCTYPE r [<!ENTITY a "x&a;">]><r a="&a;"/>"#);
    assert!(e.reason.contains("refers to itself"), "{e}");
}

#[test]
fn entities_nest_only_so_deep() {
    let mut xml = String::from("<!DOCTYPE r [<!ENTITY e0 \"x\">");
    for i in 1..=20 {
        xml.push_str(&format!("<!ENTITY e{i} \"&e{};\">", i - 1));
    }
    xml.push_str("]><r>&e20;</r>");
    assert!(bad(&xml).reason.contains("nest more than 16 deep"));
}

#[test]
fn external_entities_are_never_fetched() {
    // XXE: the classic file read, and a request out.
    for decl in [
        r#"<!ENTITY xxe SYSTEM "file:///etc/passwd">"#,
        r#"<!ENTITY xxe PUBLIC "-//x//EN" "http://127.0.0.1:1/x">"#,
    ] {
        let xml = format!("<!DOCTYPE r [{decl}]><r>&xxe;</r>");
        let e = bad(&xml);
        assert!(e.reason.contains("never fetched"), "{e}");
        let xml = format!("<!DOCTYPE r [{decl}]><r a='&xxe;'/>");
        assert!(bad(&xml).reason.contains("never fetched"));
    }
    // An external DTD is not read either, so what it would declare isn't.
    let e = bad(r#"<!DOCTYPE r SYSTEM "file:///etc/xml/catalog"><r>&amp;&ext;</r>"#);
    assert!(e.reason.contains("&ext; isn't declared"), "{e}");
    // Declared and unused, an external entity does no harm.
    assert!(
        Document::parse_xml(r#"<!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]><r/>"#)
            .is_ok()
    );
}

#[test]
fn parameter_entities_are_not_expanded() {
    let e = bad(
        r#"<!DOCTYPE r [<!ENTITY % ext SYSTEM "http://127.0.0.1:1/evil.dtd"> %ext; <!ENTITY a "x">]><r>&a;</r>"#,
    );
    assert!(e.reason.contains("&a; isn't declared"), "{e}");
}

#[test]
fn an_entity_must_hold_whole_elements() {
    let e = bad(r#"<!DOCTYPE r [<!ENTITY open "<b>">]><r>&open;</b></r>"#);
    assert!(e.reason.contains("leaves <b> open"), "{e}");
    let e = bad(r#"<!DOCTYPE r [<!ENTITY close "</r>">]><r>&close;"#);
    assert!(e.reason.contains("didn't start"), "{e}");
    let e = bad(r#"<!DOCTYPE r [<!ENTITY lt2 "<">]><r a="&lt2;"/>"#);
    assert!(e.reason.contains("contains `<`"), "{e}");
}

#[test]
fn elements_nest_only_so_deep() {
    let deep = |n: usize| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
    let doc = Document::parse_xml(&deep(2048)).unwrap();
    // Writing and printing the deepest allowed document is no recursion.
    assert_eq!(doc.root().html().len(), deep(2048).len() - 3);
    assert!(!doc.root().pretty().is_empty());
    let e = bad(&deep(2049));
    assert!(e.reason.contains("nest more than 2048 deep"), "{e}");
}

#[test]
fn parsing_gives_up_when_out_of_time() {
    let xml = format!("<r>{}</r>", "<a>x</a>".repeat(100_000));
    match Document::parse_xml_within(&xml, Duration::ZERO) {
        Err(ParseXmlError::Timeout(t)) => assert!(t.parsed < xml.len()),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("parsed"),
    }
    assert!(Document::parse_xml_within(&xml, Duration::from_secs(60)).is_ok());
}

// --- one node at a time ------------------------------------------------------------

const SITEMAP: &str = r#"<?xml version="1.0"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
        xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
  <url><loc>https://example.com/a</loc>
    <image:image><image:loc>https://example.com/a.jpg</image:loc></image:image></url>
  <url><loc>https://example.com/b</loc></url>
</urlset>"#;

const SM: &str = "http://www.sitemaps.org/schemas/sitemap/0.9";

#[test]
fn nodes_are_read_one_at_a_time() {
    let nodes: Vec<Document> = XmlNodes::new(
        SITEMAP.into(),
        "n:url",
        &ns(&[("n", SM)]),
        XmlOptions::default(),
    )
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap();
    assert_eq!(nodes.len(), 2);
    let n = [("n", SM)];
    assert_eq!(
        xpath_in(&nodes[0], "/n:url/n:loc/text()", &n),
        ["https://example.com/a"]
    );
    assert_eq!(
        xpath_in(&nodes[1], "/n:url/n:loc/text()", &n),
        ["https://example.com/b"]
    );
    // Each keeps the declarations it had in scope.
    assert_eq!(
        nodes[1].root().html(),
        r#"<url xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:image="http://www.google.com/schemas/sitemap-image/1.1"><loc>https://example.com/b</loc></url>"#
    );
    assert_eq!(
        xpath_in(
            &nodes[0],
            "//i:loc/text()",
            &[("i", "http://www.google.com/schemas/sitemap-image/1.1")]
        ),
        ["https://example.com/a.jpg"]
    );
}

#[test]
fn a_node_name_without_a_prefix_is_in_no_namespace() {
    let none: Vec<_> = XmlNodes::new(SITEMAP.into(), "url", &[], XmlOptions::default())
        .unwrap()
        .collect();
    assert!(none.is_empty());
    let rss: Vec<Document> = XmlNodes::new(RSS.into(), "item", &[], XmlOptions::default())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rss.len(), 2);
    assert_eq!(
        xpath_in(
            &rss[0],
            "//dc:creator/text()",
            &[("dc", "http://purl.org/dc/elements/1.1/")]
        ),
        ["Ann"]
    );
}

#[test]
fn nested_matches_belong_to_the_outer_one() {
    let docs: Vec<Document> = XmlNodes::new(
        "<r><x>1<x>2</x></x><x>3</x></r>".into(),
        "x",
        &[],
        XmlOptions::default(),
    )
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap();
    let texts: Vec<String> = docs.iter().map(|d| d.root().text()).collect();
    assert_eq!(texts, ["12", "3"]);
}

#[test]
fn a_broken_document_ends_the_nodes_with_an_error() {
    let mut nodes = XmlNodes::new(
        "<r><x>1</x><x>2</y></r>".into(),
        "x",
        &[],
        XmlOptions::default(),
    )
    .unwrap();
    assert!(nodes.next().unwrap().is_ok());
    let e = nodes.next().unwrap().unwrap_err();
    assert!(e.reason.contains("</y> doesn't close <x>"), "{e}");
    assert!(nodes.next().is_none());
    // Cut short after the last node is still an error.
    let mut nodes = XmlNodes::new("<r><x>1</x>".into(), "x", &[], XmlOptions::default()).unwrap();
    assert!(nodes.next().unwrap().is_ok());
    assert!(
        nodes
            .next()
            .unwrap()
            .unwrap_err()
            .reason
            .contains("ends inside <r>")
    );
}

#[test]
fn a_node_tag_prefix_not_given_is_the_documents() {
    // As Scrapy's xmliter_lxml: a prefix the spider doesn't bind means
    // what the document binds it to where the element is.
    let mut nodes =
        XmlNodes::new(SITEMAP.into(), "image:image", &[], XmlOptions::default()).unwrap();
    let (doc, binding) = nodes.next_node().unwrap().unwrap();
    assert_eq!(
        binding,
        Some((
            "image".to_string(),
            "http://www.google.com/schemas/sitemap-image/1.1".to_string()
        ))
    );
    assert!(doc.root().html().starts_with("<image:image"));
    assert!(nodes.next().is_none());
    // A prefix neither gives matches nothing.
    let none: Vec<_> = XmlNodes::new(SITEMAP.into(), "n:url", &[], XmlOptions::default())
        .unwrap()
        .collect();
    assert!(none.is_empty());
    assert!(XmlNodes::new(SITEMAP.into(), "1x", &[], XmlOptions::default()).is_err());
}

#[test]
fn entities_work_one_node_at_a_time_too() {
    let xml = r#"<!DOCTYPE r [<!ENTITY e "<b>x</b>">]><r><i>&e;</i><i>&e;&e;</i></r>"#;
    let docs: Vec<Document> = XmlNodes::new(xml.into(), "i", &[], XmlOptions::default())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(docs[1].root().html(), "<i><b>x</b><b>x</b></i>");
    let laughs = r#"<!DOCTYPE r [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;"><!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;"><!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;"><!ENTITY e "&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;"><!ENTITY f "&e;&e;&e;&e;&e;&e;&e;&e;&e;&e;"><!ENTITY g "&f;&f;&f;&f;&f;&f;&f;&f;&f;&f;">]><r><i>&g;</i></r>"#;
    let result: Result<Vec<Document>, XmlError> =
        XmlNodes::new(laughs.into(), "i", &[], XmlOptions::default())
            .unwrap()
            .collect();
    assert!(result.unwrap_err().reason.contains("billion laughs"));
}

// --- found in review ----------------------------------------------------------------

fn ns_attrs(n: usize) -> String {
    let attrs: Vec<String> = (0..n).map(|i| format!("p:a{i}=\"1\"")).collect();
    format!("<a xmlns:p=\"u\" {}/>", attrs.join(" "))
}

#[test]
fn many_attributes_take_linear_time() {
    // 30,000 attributes took 26 seconds when every one was compared with
    // every other.
    let xml = ns_attrs(30_000);
    let start = std::time::Instant::now();
    let doc = Document::parse_xml_within(&xml, Duration::from_secs(20)).unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(xpath(&doc, "count(/a/@*)"), ["30000.0"]);
    // Duplicates are still found among them.
    let twice = xml.replace("p:a29999=", "p:a0=");
    assert!(bad(&twice).reason.contains("p:a0 twice"));
}

#[test]
fn the_budget_runs_out_inside_one_start_tag() {
    let xml = ns_attrs(200_000);
    let start = std::time::Instant::now();
    match Document::parse_xml_within(&xml, Duration::from_millis(1)) {
        Err(ParseXmlError::Timeout(_)) => {}
        other => panic!("{:?}", other.map(|d| d.root().html().len())),
    }
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn the_budget_runs_out_inside_entity_expansion() {
    // Few events, much text: the clock is read by bytes expanded too.
    let big = "x".repeat(50_000);
    let xml = format!(
        "<!DOCTYPE r [<!ENTITY big \"{big}\">]><r>{}</r>",
        "&big;".repeat(20)
    );
    assert!(Document::parse_xml(&xml).is_ok());
    assert!(matches!(
        Document::parse_xml_within(&xml, Duration::ZERO),
        Err(ParseXmlError::Timeout(_))
    ));
}

#[test]
fn reading_one_node_has_a_budget() {
    let xml = format!("<r><x>1</x>{}</r>", ns_attrs(200_000));
    let options = XmlOptions {
        recover: false,
        budget: Duration::from_millis(1),
    };
    let mut nodes = XmlNodes::new(xml, "x", &[], options).unwrap();
    assert!(nodes.next().unwrap().is_ok());
    let e = nodes.next().unwrap().unwrap_err();
    assert!(e.reason.contains("took longer than"), "{e}");
}

#[test]
fn css_attribute_selectors_work_on_xml_names_html_also_has() {
    // XHTML served as application/xml, and feeds' href, type and title.
    let doc = Document::parse_xml(concat!(
        r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>"#,
        r#"<a href="/p/1" title="One" type="text/html">One</a><img src="/i.png" alt="x"/>"#,
        r#"<link HREF="/upper"/><b ID="caps" id="low">b</b></body></html>"#
    ))
    .unwrap();
    let css = |q: &str| Query::new(q).unwrap().strings(doc.root());
    assert_eq!(css("a[href]::attr(href)"), ["/p/1"]);
    assert_eq!(css("a[href*='/p/']::text"), ["One"]);
    assert_eq!(css("img[src]::attr(alt)"), ["x"]);
    assert_eq!(css("[title=One]::text"), ["One"]);
    assert_eq!(css("[type]::text"), ["One"]);
    // CSS attribute names ignore case, as in HTML.
    assert_eq!(css("[href='/upper']::attr(HREF)"), ["/upper"]);
    // Both of id and ID are kept, and XPath tells them apart.
    assert_eq!(xpath(&doc, "//*[local-name()='b']/@ID"), ["caps"]);
    assert_eq!(xpath(&doc, "//*[local-name()='b']/@id"), ["low"]);
    assert_eq!(css("#low::text"), ["b"]);
    // CSS attribute names ignore case for these too, as for any other.
    let doc = Document::parse_xml(r#"<r><x ID="up">1</x><y Class="c">2</y><z id="lo">3</z></r>"#)
        .unwrap();
    let css = |q: &str| Query::new(q).unwrap().strings(doc.root());
    assert_eq!(css("[ID]::text"), ["1", "3"]);
    assert_eq!(css("[id]::text"), ["1", "3"]);
    assert_eq!(css("[ID=up]::text"), ["1"]);
    assert_eq!(css("[class]::text"), ["2"]);
    assert_eq!(css("[CLASS=c]::text"), ["2"]);
    // #id and .class are the attributes named exactly so.
    assert_eq!(css("#up::text"), Vec::<String>::new());
    assert_eq!(css("#lo::text"), ["3"]);
    assert_eq!(css(".c::text"), Vec::<String>::new());
    // With id and ID both there, in either order, [id] reads the id, as
    // parsel does; ID is left to XPath.
    let doc = Document::parse_xml(concat!(
        r#"<r><x ID="up" id="lo">1</x><x id="lo" ID="up">2</x>"#,
        r#"<y Class="c" class="d">3</y><y class="d" Class="c">4</y></r>"#
    ))
    .unwrap();
    let css = |q: &str| Query::new(q).unwrap().strings(doc.root());
    assert_eq!(css("[id=lo]::text"), ["1", "2"]);
    assert_eq!(css("[id=up]::text"), Vec::<String>::new());
    assert_eq!(css("#lo::text"), ["1", "2"]);
    assert_eq!(css("[class=d]::text"), ["3", "4"]);
    assert_eq!(css("[class~=c]::text"), Vec::<String>::new());
    assert_eq!(css(".d::text"), ["3", "4"]);
    assert_eq!(xpath(&doc, "//x/@ID"), ["up", "up"]);
}

#[test]
fn nodes_completed_inside_one_entity_are_all_given() {
    let xml = r#"<!DOCTYPE r [<!ENTITY two "<i>1</i><i>2</i>">]><r>&two;<i>3</i></r>"#;
    let texts: Vec<String> = XmlNodes::new(xml.into(), "i", &[], XmlOptions::default())
        .unwrap()
        .map(|d| d.unwrap().root().text())
        .collect();
    assert_eq!(texts, ["1", "2", "3"]);
}

#[test]
fn an_empty_cdata_section_is_an_empty_text_node() {
    // As lxml: '' from text(), and an end tag rather than <d/>.
    let doc = Document::parse_xml("<r><d><![CDATA[]]></d><e></e></r>").unwrap();
    assert_eq!(xpath(&doc, "/r/d/text()"), [""]);
    assert_eq!(xpath(&doc, "/r/d"), ["<d></d>"]);
    assert_eq!(xpath(&doc, "/r/e"), ["<e/>"]);
    // Read node by node it's nothing, as in Scrapy's iternodes.
    let mut nodes = XmlNodes::new(
        "<r><d><![CDATA[]]></d></r>".into(),
        "d",
        &[],
        XmlOptions::default(),
    )
    .unwrap();
    assert_eq!(nodes.next().unwrap().unwrap().root().html(), "<d/>");
}

#[test]
fn whitespace_may_come_before_the_xml_declaration() {
    let doc = Document::parse_xml("\n\n  <?xml version=\"1.0\"?>\n<r>x</r>").unwrap();
    assert_eq!(xpath(&doc, "/r/text()"), ["x"]);
}

#[test]
fn html_entities_are_looked_up_by_name() {
    assert_eq!(html_entity("nbsp").as_deref(), Some("\u{a0}"));
    assert_eq!(html_entity("eacute").as_deref(), Some("é"));
    assert_eq!(html_entity("notanentity"), None);
    assert_eq!(html_entity(""), None);
}

// --- recover mode ------------------------------------------------------------------

fn recovered(xml: &str) -> (Document, netweir_dom::xml::Repairs) {
    Document::parse_xml_with(xml, recover()).unwrap_or_else(|e| panic!("{xml:?}: {e}"))
}

#[test]
fn recover_mode_repairs_what_feeds_get_wrong_and_says_so() {
    let item = |t: &str| format!("<rss><channel><item><title>{t}</title></item></channel></rss>");
    for (xml, want, says) in [
        (
            item("a&nbsp;b"),
            "a\u{a0}b",
            "&nbsp; isn't declared; read as HTML's",
        ),
        (item("caf&eacute;"), "café", "read as HTML's"),
        (
            item("x&madeup;y"),
            "x&madeup;y",
            "&madeup; isn't declared; kept as text",
        ),
        (item("Tom & Jerry"), "Tom & Jerry", "kept as text"),
        (
            item("vt\u{b}here"),
            "vthere",
            "U+000B isn't allowed in XML; dropped",
        ),
        (item("bad&#0;ref"), "badref", "dropped"),
        (
            format!("{}\n<b>Warning</b>: PHP", item("A")),
            "A",
            "after the root element; the rest is ignored",
        ),
        (
            format!("junk {}", item("A")),
            "A",
            "outside the root element; dropped",
        ),
    ] {
        assert!(Document::parse_xml(&xml).is_err(), "{xml:?} is broken");
        let (doc, repairs) = recovered(&xml);
        assert_eq!(xpath(&doc, "//title/text()"), [want], "{xml:?}");
        assert!(repairs.count >= 1);
        assert!(
            repairs.first[0].reason.contains(says),
            "{xml:?}: {repairs:?}"
        );
        assert!(repairs.summary().contains(says));
    }
}

#[test]
fn recover_mode_repairs_names_and_attributes() {
    let (doc, repairs) = recovered(r#"<r a="1" a="2" b="x&y"><media:content url="u"/></r>"#);
    assert_eq!(xpath(&doc, "/r/@a"), ["1"]);
    assert_eq!(xpath(&doc, "/r/@b"), ["x&y"]);
    // An undeclared prefix leaves the element in no namespace, named as
    // written, as lxml's recover mode does.
    assert_eq!(xpath(&doc, "name(/r/*)"), ["media:content"]);
    assert_eq!(xpath(&doc, "namespace-uri(/r/*)"), [""]);
    assert_eq!(repairs.count, 3);
    let reasons: Vec<&str> = repairs.first.iter().map(|e| e.reason.as_str()).collect();
    assert!(
        reasons[0].contains("the attribute a twice; the first is kept"),
        "{reasons:?}"
    );
    assert!(reasons[1].contains("starts no reference"), "{reasons:?}");
    assert!(
        reasons[2].contains("prefix media of <media:content> isn't declared"),
        "{reasons:?}"
    );
    assert_eq!((repairs.first[0].line, repairs.first[0].column), (1, 1));
}

#[test]
fn recover_mode_closes_what_a_cut_off_document_left_open() {
    let (doc, repairs) = recovered("<rss><item><title>A</title></item><item><title>B");
    assert_eq!(xpath(&doc, "//title/text()"), ["A", "B"]);
    assert!(
        repairs.first[0]
            .reason
            .contains("ends inside <rss><item><title>; closed")
    );
}

#[test]
fn recover_mode_closes_up_to_an_end_tag_for_an_element_further_out() {
    let (doc, repairs) = recovered("<r><a><b><c>x</a>y</r>");
    assert_eq!(doc.root().html(), "<r><a><b><c>x</c></b></a>y</r>");
    assert_eq!(repairs.count, 1);
    assert!(
        repairs.first[0]
            .reason
            .contains("</a> doesn't close <c>; <b><c> closed with it"),
        "{repairs:?}"
    );
    // A cut-off feed's last item, as lxml reads it.
    let (doc, _) =
        recovered("<rss><channel><item><title>A</title></item><item><title>B</channel></rss>");
    assert_eq!(
        doc.root().html(),
        "<rss><channel><item><title>A</title></item><item><title>B</title></item></channel></rss>"
    );
}

#[test]
fn recover_mode_ignores_an_end_tag_that_closes_nothing() {
    // lxml's recover mode ends the document there, losing <i>b</i>.
    let (doc, repairs) = recovered("<r><i>a</i></x><i>b</i></r>");
    assert_eq!(doc.root().html(), "<r><i>a</i><i>b</i></r>");
    assert!(
        repairs.first[0]
            .reason
            .contains("</x> doesn't close <r>; ignored")
    );
    let (doc, repairs) = recovered("</x><r/>");
    assert_eq!(doc.root().html(), "<r/>");
    assert!(repairs.first[0].reason.contains("never started; ignored"));
}

#[test]
fn recover_mode_keeps_a_lone_less_than_as_text() {
    for (xml, text) in [
        ("<r>1 < 2</r>", "1 < 2"),
        ("<r>a<=b and c<3</r>", "a<=b and c<3"),
        ("<r>x <</r>", "x <"),
        ("<r>< b>c</r>", "< b>c"),
    ] {
        let (doc, repairs) = recovered(xml);
        assert_eq!(xpath(&doc, "/r/text()"), [text], "{xml}");
        assert!(
            repairs
                .first
                .iter()
                .all(|r| r.reason.contains("`<` that starts no markup; kept as text")),
            "{repairs:?}"
        );
        assert!(
            bad(xml).reason.contains("isn't a valid element name"),
            "{xml}"
        );
    }
    let (_, repairs) = recovered("<r>a<=b and c<3</r>");
    assert_eq!(repairs.count, 2);
}

#[test]
fn recover_mode_still_refuses_what_it_cant_repair() {
    for xml in ["<a><b c</a>", "<a><!-- never ends</a>"] {
        assert!(Document::parse_xml_with(xml, recover()).is_err(), "{xml}");
    }
    // And the limits on hostile XML hold.
    let xxe = r#"<!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]><r>&x;</r>"#;
    assert!(Document::parse_xml_with(xxe, recover()).is_err());
}

#[test]
fn repairs_keep_the_first_few_and_count_them_all() {
    let xml = format!("<r>{}</r>", "&nbsp;".repeat(50));
    let (_, repairs) = recovered(&xml);
    assert_eq!(repairs.count, 50);
    assert_eq!(repairs.first.len(), netweir_dom::xml::REPAIRS_KEPT);
    assert!(repairs.summary().contains("and 40 more"));
}

#[test]
fn recover_mode_reads_nodes_one_at_a_time_too() {
    let xml = "<r><i>a&nbsp;</i><i>b</i></r> trailing".to_string();
    let mut nodes = XmlNodes::new(xml, "i", &[], recover()).unwrap();
    let texts: Vec<String> = nodes.by_ref().map(|d| d.unwrap().root().text()).collect();
    assert_eq!(texts, ["a\u{a0}", "b"]);
    assert_eq!(nodes.repairs().count, 2);
}

#[test]
fn many_declarations_and_prefixed_attributes_stay_linear() {
    let n = 20_000;
    let decls: Vec<String> = (0..n).map(|i| format!("xmlns:p{i}=\"u{i}\"")).collect();
    let attrs: Vec<String> = (0..n)
        .map(|i| format!("p{i}:a{i}=\"1\" p{i}:same=\"{i}\""))
        .collect();
    let xml = format!("<a {} {}><b/></a>", decls.join(" "), attrs.join(" "));
    let start = std::time::Instant::now();
    let doc = Document::parse_xml_within(&xml, Duration::from_secs(30)).unwrap();
    // <b> declares everything it inherits when written alone.
    let b = &xpath(&doc, "/a/b")[0];
    assert_eq!(b.matches("xmlns:").count(), n);
    unsafe { doc.remove_namespaces() }.unwrap();
    // Every p{i}:same became one `same`, the last one's.
    assert_eq!(xpath(&doc, "/a/@same"), [format!("{}", n - 1)]);
    assert_eq!(xpath(&doc, "count(/a/@*)"), [format!("{}.0", n + 1)]);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn recover_mode_keeps_a_processing_instruction_its_repair_changed() {
    // Found by fuzzing: dropping the NUL leaves `?>` inside the data,
    // which lexbor's own constructor refused, and that was a panic.
    let (doc, repairs) = recovered("<?p a?\u{0}>?><r/>");
    assert_eq!(repairs.count, 1);
    assert_eq!(xpath(&doc, "count(/processing-instruction())"), ["1.0"]);
    let crash: &[u8] = &[
        0x3c, 0x3f, 0xdf, 0x09, 0x3e, 0x3f, 0x3e, 0x3c, 0x3f, 0xdf, 0x09, 0x3e, 0x3f, 0x3c, 0x3f,
        0x00, 0x3e, 0x0d, 0x3f, 0x3e,
    ];
    let _ = Document::parse_xml_with(&String::from_utf8_lossy(crash), recover());
}

#[test]
fn recovering_from_runs_of_bad_markup_takes_linear_time() {
    // Each was quadratic: quick-xml read on to the next `>` for every
    // `<`; every `&` in an attribute was read on to the next `;`; and
    // every stray end tag searched every open element.
    let mb = 1 << 20;
    for (xml, check) in [
        (format!("<r>{}>x</r>", "<".repeat(mb)), "<"),
        (format!("<r>{}x</r>", "< ".repeat(mb / 2)), "<"),
        (format!("<r a=\"{}\"/>", "&".repeat(mb)), "&"),
        (
            format!("<r>{}{}", "<a>".repeat(2000), "</zz>".repeat(200_000)),
            "</zz>",
        ),
    ] {
        let start = std::time::Instant::now();
        let options = XmlOptions {
            recover: true,
            budget: Duration::from_secs(30),
        };
        let (_, repairs) =
            Document::parse_xml_with(&xml, options).unwrap_or_else(|e| panic!("{check}: {e}"));
        assert!(repairs.count > 100_000, "{check}: {}", repairs.count);
        // Linear, each takes well under a second; quadratic, minutes. The
        // margin is for a slow runner's debug build.
        let took = start.elapsed();
        assert!(took < Duration::from_secs(20), "{check}: {took:?}");
    }
    let (doc, _) = recovered(&format!("<r>{} x</r>", "<".repeat(10)));
    assert_eq!(xpath(&doc, "/r/text()"), [format!("{} x", "<".repeat(10))]);
}
