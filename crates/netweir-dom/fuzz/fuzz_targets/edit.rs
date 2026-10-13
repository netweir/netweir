//! Any sequence of tree edits on a small page, each byte pair picking an
//! edit and a node (removed ones included). After every edit the index
//! must agree with a walk of the tree, and every node ever seen must
//! still read.
#![no_main]

use libfuzzer_sys::fuzz_target;
use netweir_dom::{Document, Node, NodeId, XPath};

const PAGE: &str = "<html><body><div id=a class='x y'><p lang=en>one <b>two</b></p><!-- c -->\
                    <ul><li>1</li><li>2</li></ul></div><template><i>t</i></template></body></html>";

fuzz_target!(|data: &[u8]| {
    let doc = Document::parse(PAGE);
    let count = XPath::new("count(//node())").unwrap();
    let mut seen: Vec<NodeId> = doc.root().descendants().map(|n| n.id()).collect();
    seen.push(doc.root().id());
    for pair in data.chunks(2).take(64) {
        let (op, at) = (pair[0], *pair.get(1).unwrap_or(&0) as usize);
        let node: Node<'_> = unsafe { doc.node(seen[at % seen.len()]) };
        let other = seen[(at / 3 + op as usize) % seen.len()];
        // SAFETY: every id came from `doc`, and only this thread uses it.
        let made = unsafe {
            match op % 8 {
                0 => doc.remove(node.id()).map(|_| None),
                1 => doc.unwrap(node.id()).map(|_| None),
                2 => doc.insert(node.id(), None, &[other]).map(|_| None),
                3 => doc
                    .insert(node.id(), Some(at as isize % 5 - 2), &[other])
                    .map(|_| None),
                4 => doc.replace(node.id(), &[other]).map(|_| None),
                5 => doc.set_text(node.id(), "s").map(|_| None),
                6 => doc.create_element("em", &[("class", "z")]).map(Some),
                _ => doc.create_text("t").map(Some),
            }
        };
        if let Ok(Some(id)) = made {
            seen.push(id);
        }
        // `//node()` is every node but the document itself.
        let walked = doc.root().descendants().count();
        let indexed = count.run(doc.root()).unwrap()[0].to_string_value();
        assert_eq!(indexed, format!("{walked}.0"));
        for &id in &seen {
            let n = unsafe { doc.node(id) };
            let _ = (n.html(), n.order(), n.text(), n.parent());
        }
    }
});
