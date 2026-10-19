//! Any bytes as XML: parse within a budget, query, write out, strip the
//! namespaces, and read node by node. Hostile input may be refused, never
//! crash or run away.
#![no_main]

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use netweir_dom::{Document, Query, XPath, XmlNodes};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let namespaces = [("a".to_string(), "urn:a".to_string())];
    for doc in XmlNodes::new(text.to_string(), "a:x", &namespaces)
        .unwrap()
        .flatten()
    {
        let _ = doc.root().html();
    }
    let Ok(doc) = Document::parse_xml_within(&text, Duration::from_secs(1)) else {
        return;
    };
    let root = doc.root();
    let _ = root.html();
    let _ = root.pretty();
    let _ = XPath::new("//a:*/@*|//*[name()]/text()")
        .unwrap()
        .run_in(root, &[], &namespaces);
    let _ = Query::new("* ::text").unwrap().strings(root);
    unsafe { doc.remove_namespaces() }.unwrap();
    let _ = root.html();
});
