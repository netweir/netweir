//! Any string as an XPath expression, evaluated on a small page.
#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use netweir_dom::{Document, XPath};

static DOC: LazyLock<Document> = LazyLock::new(|| {
    Document::parse(
        "<html><body><div id=a class='x y'><p lang=en>one <b>two</b></p><!-- c -->\
         <ul><li>1</li><li>2</li></ul></div><template><i>t</i></template></body></html>",
    )
});

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(xpath) = XPath::cached(src) {
        let _ = xpath.run(DOC.root());
    }
});
