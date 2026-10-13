//! Tree edits: what they do to the tree, the index and handles to nodes
//! they move or remove.

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

use std::sync::{Barrier, mpsc};
use std::time::Duration;

use netweir_dom::{Content, Document, EditError, Node, NodeId, NodeKind, Query, XPath};

fn css<'a>(scope: Node<'a>, q: &str) -> Vec<Node<'a>> {
    Query::new(q).unwrap().select(scope)
}

fn one<'a>(doc: &'a Document, q: &str) -> Node<'a> {
    css(doc.root(), q)[0]
}

fn xpath(scope: Node<'_>, q: &str) -> Vec<String> {
    XPath::new(q)
        .unwrap()
        .run(scope)
        .unwrap()
        .iter()
        .map(|h| h.to_string_value())
        .collect()
}

fn body(doc: &Document) -> String {
    one(doc, "body").html()
}

const PAGE: &str = "<body><p id=a>one <b>bold</b></p><script>var x</script>\
                    <style>p{}</style><p id=b>two</p></body>";

#[test]
fn removed_nodes_leave_the_tree_and_their_handles_keep_working() {
    let doc = Document::parse(PAGE);
    // Build the index first, so the edit has one to drop.
    assert_eq!(xpath(doc.root(), "//p/@id"), ["a", "b"]);
    let a = one(&doc, "#a");
    let b_tag = one(&doc, "b");
    for s in css(doc.root(), "script, style") {
        unsafe { doc.remove(s.id()).unwrap() };
    }
    unsafe { doc.remove(a.id()).unwrap() };
    assert_eq!(body(&doc), "<body><p id=\"b\">two</p></body>");
    assert_eq!(xpath(doc.root(), "//p/@id"), ["b"]);
    assert_eq!(xpath(doc.root(), "count(//b)"), ["0.0"]);
    // The removed <p> is a tree of its own.
    assert_eq!(a.html(), "<p id=\"a\">one <b>bold</b></p>");
    assert_eq!(a.parent(), None);
    assert_eq!(b_tag.parent(), Some(a));
    assert_eq!(css(a, "b").len(), 1);
    assert_eq!(xpath(a, "//b/text()"), ["bold"]);
    assert_eq!(xpath(a, "/@id"), ["a"]);
    assert_eq!(xpath(b_tag, "ancestor::*/@id"), ["a"]);
    assert!(b_tag.order() > a.order());
    // Removing it again, or anything without a parent, does nothing.
    unsafe { doc.remove(a.id()).unwrap() };
    assert!(matches!(
        unsafe { doc.remove(doc.root().id()) },
        Err(EditError::Invalid(_))
    ));
}

#[test]
fn replace_unwrap_and_set_text() {
    let doc = Document::parse("<body><div><p>x <i>y</i> z</p></div></body>");
    let p = one(&doc, "p");
    let i = one(&doc, "i");
    unsafe { doc.unwrap(i.id()).unwrap() };
    assert_eq!(body(&doc), "<body><div><p>x y z</p></div></body>");
    assert_eq!(i.html(), "<i></i>");

    let h2 = unsafe { doc.create_element("H2", &[("Class", "t"), ("data-x", "1")]) }.unwrap();
    let text = unsafe { doc.create_text("tail & <more>") }.unwrap();
    unsafe { doc.replace(p.id(), &[h2, text]).unwrap() };
    assert_eq!(
        body(&doc),
        "<body><div><h2 class=\"t\" data-x=\"1\"></h2>tail &amp; &lt;more&gt;</div></body>"
    );
    assert_eq!(p.parent(), None);
    assert_eq!(xpath(doc.root(), "//h2/@class"), ["t"]);
    assert_eq!(css(doc.root(), "h2.t").len(), 1);

    let h2 = one(&doc, "h2");
    unsafe { doc.set_text(h2.id(), "Title").unwrap() };
    assert_eq!(xpath(doc.root(), "string(//h2)"), ["Title"]);
    unsafe { doc.set_text(h2.id(), "Again").unwrap() };
    assert_eq!(h2.html(), "<h2 class=\"t\" data-x=\"1\">Again</h2>");
}

#[test]
fn insert_moves_and_places_nodes() {
    let doc = Document::parse("<ul><li>1</li><li>2</li><li>3</li></ul>");
    let ul = one(&doc, "ul");
    let lis = css(doc.root(), "li");
    // Move the last to the front, then the first to the end.
    unsafe { doc.insert(ul.id(), Some(0), &[lis[2].id()]).unwrap() };
    assert_eq!(xpath(doc.root(), "//li/text()"), ["3", "1", "2"]);
    unsafe { doc.insert(ul.id(), None, &[lis[2].id()]).unwrap() };
    assert_eq!(xpath(doc.root(), "//li/text()"), ["1", "2", "3"]);
    // Before itself is where it already is.
    unsafe { doc.insert(ul.id(), Some(1), &[lis[1].id()]).unwrap() };
    assert_eq!(xpath(doc.root(), "//li/text()"), ["1", "2", "3"]);
    let new = unsafe { doc.create_element("li", &[]) }.unwrap();
    unsafe { doc.insert(ul.id(), Some(-2), &[new]).unwrap() };
    assert_eq!(xpath(doc.root(), "count(//li)"), ["4.0"]);
    assert_eq!(xpath(doc.root(), "//li[2]/text()"), Vec::<String>::new());
    assert_eq!(xpath(doc.root(), "//li[3]/text()"), ["2"]);
}

#[test]
fn edits_that_make_no_sense_change_nothing() {
    let doc = Document::parse("<div><p><b>x</b></p></div><span>s</span>");
    let before = doc.root().html();
    let (div, p, b) = (one(&doc, "div"), one(&doc, "p"), one(&doc, "b"));
    let text = b.first_child().unwrap();
    let invalid = |r: Result<(), EditError>| assert!(matches!(r, Err(EditError::Invalid(_))));
    // Into itself or its own descendant.
    invalid(unsafe { doc.insert(b.id(), None, &[div.id()]) });
    invalid(unsafe { doc.insert(p.id(), None, &[p.id()]) });
    // Under text; before a node that isn't a child; the document itself.
    invalid(unsafe { doc.insert(text.id(), None, &[div.id()]) });
    invalid(unsafe { doc.insert(div.id(), None, &[doc.root().id()]) });
    invalid(unsafe { doc.replace(p.id(), &[p.id()]) });
    invalid(unsafe { doc.replace(p.id(), &[div.id()]) });
    invalid(unsafe { doc.set_text(text.id(), "x") });
    // Text in a void element would be written nowhere.
    let br = unsafe { doc.create_element("br", &[]) }.unwrap();
    invalid(unsafe { doc.set_text(br, "x") });
    // Strings and nodes go in as one edit, so a failure leaves no new text
    // behind on the page or off it.
    let nodes = |d: &Document| d.root().descendants().count();
    let before_count = nodes(&doc);
    invalid(unsafe { doc.insert(b.id(), None, &[Content::Text("x"), Content::Node(div.id())]) });
    assert_eq!(nodes(&doc), before_count);
    for bad in ["", "1a", "a b", "a>"] {
        assert!(unsafe { doc.create_element(bad, &[]) }.is_err(), "{bad}");
    }
    assert!(unsafe { doc.create_element("a", &[("on click", "")]) }.is_err());
    assert_eq!(doc.root().html(), before);
}

#[test]
fn an_edit_inside_a_read_on_the_same_thread_fails_instead_of_hanging() {
    let doc = Document::parse("<p>x</p>");
    let p = one(&doc, "p");
    {
        let _reading = doc.read();
        let _nested = doc.read();
        assert_eq!(unsafe { doc.remove(p.id()) }, Err(EditError::Reading));
    }
    unsafe { doc.remove(p.id()).unwrap() };
    // Nor while reading another document: two threads doing that the
    // other way round would wait for each other.
    let other = Document::parse("<p>y</p>");
    let reading = other.read();
    let body = one(&doc, "body").id();
    assert_eq!(unsafe { doc.remove(body) }, Err(EditError::Reading));
    drop(reading);
    unsafe { doc.remove(body).unwrap() };
}

#[test]
fn pretty_lays_out_one_node_a_line() {
    let doc = Document::parse(
        "<!DOCTYPE html><html><head><title>T &amp; t</title></head><body>\
         <div class=\"a\">  text  <br><!-- c --><pre>  keep\n  this</pre>\
         <script>if (a < b) {}</script></div></body></html>",
    );
    assert_eq!(
        doc.root().pretty(),
        "<!DOCTYPE html>\n\
         <html>\n <head>\n  <title>\n   T &amp; t\n  </title>\n </head>\n <body>\n  \
         <div class=\"a\">\n   text\n   <br>\n   <!-- c -->\n   <pre>  keep\n  this</pre>\n   \
         <script>if (a < b) {}</script>\n  </div>\n </body>\n</html>\n"
    );
    assert_eq!(one(&doc, "br").pretty(), "<br>\n");
    let doc = Document::parse("<p>a<wbr>b</p>");
    assert_eq!(one(&doc, "p").pretty(), "<p>\n a\n <wbr>\n b\n</p>\n");
}

/// Many edits and queries, mixed, keeping handles to everything: no
/// handle may read freed memory, every query must see the current tree,
/// and the index must agree with a walk of it.
#[test]
fn stress_edits_against_queries_and_old_handles() {
    let html = "<div>".to_string() + &"<p class=x><b>t</b> u</p>".repeat(200) + "</div>";
    let doc = Document::parse(&html);
    let div = one(&doc, "div");
    let mut kept: Vec<NodeId> = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut seed = 7u64;
    let mut rand = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for round in 0..2000 {
        let all: Vec<Node<'_>> = div.descendants().collect();
        kept.extend(all.iter().map(|n| n.id()).filter(|&id| seen.insert(id)));
        let pick = all.get(rand(all.len().max(1))).copied();
        let any = unsafe { doc.node(kept[rand(kept.len())]) };
        let _ = unsafe {
            match (rand(6), pick) {
                (0, Some(n)) => doc.remove(n.id()),
                (1, Some(n)) if n.kind() == NodeKind::Element => doc.unwrap(n.id()),
                (2, Some(n)) => doc
                    .insert(div.id(), None, &[any.id()])
                    .or(doc.remove(n.id())),
                (3, Some(n)) if n.kind() == NodeKind::Element => doc.set_text(n.id(), "s"),
                (4, Some(n)) => {
                    let new = doc.create_element("p", &[("class", "x")]).unwrap();
                    doc.replace(n.id(), &[new, any.id()])
                }
                _ => {
                    let new = doc.create_element("p", &[("class", "x")]).unwrap();
                    doc.insert(div.id(), None, &[new])
                }
            }
        };
        if round % 50 == 0 {
            let walked = div.descendants().filter(|n| n.tag() == Some("p")).count();
            assert_eq!(xpath(div, "count(.//p)"), [format!("{walked}.0")]);
            assert_eq!(css(div, "p.x").len(), walked);
            // Every handle ever taken still reads.
            for &id in &kept {
                let n = unsafe { doc.node(id) };
                let _ = (n.html(), n.order(), n.parent(), n.text());
            }
        }
    }
}

#[test]
fn edits_wait_for_readers_on_other_threads() {
    let html = "<ul>".to_string() + &"<li>x</li>".repeat(300) + "</ul>";
    let doc = Arc::new(Document::parse(&html));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let doc = doc.clone();
            thread::spawn(move || {
                let q = XPath::new("count(//li)").unwrap();
                for _ in 0..300 {
                    let _reading = doc.read();
                    let n = q.run(doc.root()).unwrap()[0].to_string_value();
                    let n = n.parse::<f64>().unwrap() as usize;
                    // Each read sees a whole tree, never one mid-edit.
                    assert_eq!(
                        n,
                        doc.root()
                            .descendants()
                            .filter(|n| n.tag() == Some("li"))
                            .count()
                    );
                }
            })
        })
        .collect();
    let ul = one(&doc, "ul").id();
    for i in 0..300 {
        unsafe {
            let li = doc.create_element("li", &[]).unwrap();
            doc.insert(ul, None, &[li]).unwrap();
            if i % 2 == 0 {
                doc.remove(li).unwrap();
            }
        }
    }
    for r in readers {
        r.join().unwrap();
    }
    assert_eq!(css(doc.root(), "li").len(), 450);
}

#[test]
fn strings_and_nodes_insert_together_at_a_position() {
    let doc = Document::parse("<ul><li>1</li><li>2</li></ul>");
    let ul = one(&doc, "ul");
    let li = unsafe { doc.create_element("li", &[]) }.unwrap();
    unsafe {
        doc.insert(ul.id(), Some(1), &[Content::Text("a"), Content::Node(li)])
            .unwrap()
    };
    assert_eq!(ul.html(), "<ul><li>1</li>a<li></li><li>2</li></ul>");
    unsafe {
        doc.insert(ul.id(), Some(-1), &[Content::Text("b")])
            .unwrap()
    };
    assert_eq!(ul.html(), "<ul><li>1</li>a<li></li>b<li>2</li></ul>");
    unsafe {
        doc.insert(ul.id(), Some(99), &[Content::Text("c")])
            .unwrap()
    };
    unsafe {
        doc.insert(ul.id(), Some(-99), &[Content::Text("d")])
            .unwrap()
    };
    assert_eq!(ul.html(), "<ul>d<li>1</li>a<li></li>b<li>2</li>c</ul>");
}

#[test]
fn pretty_keeps_foreign_names_and_raw_text() {
    let doc = Document::parse(
        "<svg><foreignObject><p>x</p></foreignObject></svg><xmp>  a\n  b</xmp>\
         <iframe>  c\n d</iframe><noembed> e\n</noembed><noframes> f\n</noframes>",
    );
    let pretty = one(&doc, "body").pretty();
    assert!(
        pretty.contains("  <foreignObject>\n   <p>\n    x\n   </p>\n  </foreignObject>\n"),
        "{pretty}"
    );
    for raw in [
        "<xmp>  a\n  b</xmp>",
        "<iframe>  c\n d</iframe>",
        "<noembed> e\n</noembed>",
        "<noframes> f\n</noframes>",
    ] {
        assert!(pretty.contains(raw), "{raw} in {pretty}");
    }
    let doc = Document::parse("<plaintext>  a\n  b");
    assert!(
        one(&doc, "body")
            .pretty()
            .contains("<plaintext>  a\n  b</plaintext>")
    );
}

/// Two threads each read one page and edit the other's, as two find_all()
/// functions could. Waiting would deadlock, so both must fail at once.
#[test]
fn an_edit_while_reading_any_document_fails_instead_of_deadlocking() {
    let x = Arc::new(Document::parse("<p>x</p>"));
    let y = Arc::new(Document::parse("<p>y</p>"));
    let both = Arc::new(Barrier::new(2));
    let (tx, rx) = mpsc::channel();
    for (read, edit) in [(x.clone(), y.clone()), (y, x)] {
        let (both, tx) = (both.clone(), tx.clone());
        thread::spawn(move || {
            let _reading = read.read();
            both.wait();
            let p = one(&edit, "p").id();
            tx.send(unsafe { edit.remove(p) }).unwrap();
        });
    }
    for _ in 0..2 {
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the edits deadlocked");
        assert_eq!(got, Err(EditError::Reading));
    }
}
