//! A CSS query (the first line) run on HTML (the rest).
#![no_main]

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use netweir_dom::{Document, Query};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (query, html) = text.split_once('\n').unwrap_or((&text, "<p class=a>x</p>"));
    // Cached, as Python runs queries, so a compiled query and the thread's
    // matcher are reused across documents.
    let Ok(query) = Query::cached(query) else {
        return;
    };
    let Ok(doc) = Document::parse_within(html, Duration::from_secs(1)) else {
        return;
    };
    let _ = query.strings(doc.root());
});
