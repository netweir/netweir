//! `css_path` gives a selector that finds its element again, and only it.

use std::path::Path;

use netweir_dom::css_path::css_path;
use netweir_dom::{Document, Node, NodeKind, Query};

fn elements(doc: &Document) -> Vec<Node<'_>> {
    doc.root()
        .descendants()
        .filter(|n| n.kind() == NodeKind::Element)
        .collect()
}

fn path_of(html: &str, css: &str) -> String {
    let doc = Document::parse(html);
    let node = Query::new(css).unwrap().select(doc.root())[0];
    css_path(node).unwrap()
}

#[test]
fn every_element_of_a_page_is_found_again_by_its_path() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/content");
    let mut pages = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "html") {
            continue;
        }
        pages += 1;
        let doc = Document::parse(&std::fs::read_to_string(&path).unwrap());
        for node in elements(&doc) {
            let selector = css_path(node).unwrap();
            let found = Query::new(&selector)
                .unwrap_or_else(|e| panic!("{}: {selector:?} doesn't parse: {e}", path.display()))
                .select(doc.root());
            assert_eq!(
                found,
                [node],
                "{}: {selector:?} doesn't find just its element",
                path.display()
            );
        }
    }
    assert!(pages >= 8);
}

#[test]
fn a_unique_id_wins() {
    assert_eq!(
        path_of(
            r#"<div id="buy"><span>1</span></div><span>2</span>"#,
            "#buy span"
        ),
        "#buy > span"
    );
}

#[test]
fn generated_looking_ids_and_classes_are_skipped() {
    let html = r#"<div id="ember1234"><p class="css-1x2y3z">a</p><p class="jsx-8f9a lead">b</p></div>
                  <div id="ember1235"><p>c</p></div>"#;
    let path = path_of(html, "#ember1234 > p");
    assert!(
        !path.contains("ember") && !path.contains("css-1x2y3z"),
        "{path}"
    );
    let path = path_of(html, ".lead");
    assert_eq!(path, "p.lead");
}

#[test]
fn short_paths_beat_long_ones() {
    assert_eq!(
        path_of("<body><div><div><h1>Hi</h1></div></div></body>", "h1"),
        "h1"
    );
}

#[test]
fn awkward_names_are_escaped() {
    let html = r#"<p id="a:b.c">x</p><p id="9lives">y</p><p class="w-1/2">z</p>"#;
    for css in [r"#a\:b\.c", r"#\39 lives", r".w-1\/2"] {
        let doc = Document::parse(html);
        let node = Query::new(css).unwrap().select(doc.root())[0];
        let path = css_path(node).unwrap();
        assert_eq!(
            Query::new(&path).unwrap().select(doc.root()),
            [node],
            "{path}"
        );
    }
}

#[test]
fn only_elements_have_a_path() {
    let doc = Document::parse("<p>text</p>");
    let text = doc
        .root()
        .descendants()
        .find(|n| n.kind() == NodeKind::Text)
        .unwrap();
    assert_eq!(css_path(text), None);
}
