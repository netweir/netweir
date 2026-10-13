//! Any HTML as Markdown, as text, and read for its main content and
//! article: nothing panics, and Markdown comes out the same every time.
#![no_main]

use libfuzzer_sys::fuzz_target;
use netweir_dom::{Document, content, markdown};

fuzz_target!(|data: &[u8]| {
    let html = String::from_utf8_lossy(data);
    let doc = Document::parse(&html);
    let base = url::Url::parse("https://example.com/a/b").unwrap();
    let md = markdown::markdown(doc.root(), Some(&base));
    assert_eq!(md, markdown::markdown(doc.root(), Some(&base)));
    assert!(!md.starts_with('\n') && !md.ends_with('\n'), "{md:?}");
    markdown::clean_text(doc.root());
    if let Some(main) = content::main_content(doc.root()) {
        main.markdown(Some(&base));
        main.html();
    }
    content::article(doc.root(), Some(&base));
});
