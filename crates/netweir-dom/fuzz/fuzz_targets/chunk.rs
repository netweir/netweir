//! Any HTML's Markdown, and any text read as Markdown, in chunks: nothing
//! panics, no chunk is empty or over the limit, no text is lost, and no
//! two words are glued together.
#![no_main]

use libfuzzer_sys::fuzz_target;
use netweir_dom::chunk::{Options, chunks};
use netweir_dom::{Document, markdown};

fn check(md: &str, options: Options) {
    let got = chunks(md, "https://example.com/", options);
    let solid = |s: &str| s.chars().filter(|c| !c.is_whitespace()).count();
    for c in &got {
        assert!(!c.text.is_empty());
        assert!(c.text.chars().count() <= options.max_chars, "{options:?}: {:?}", c.text);
    }
    // Pieces repeat fences, table headers and overlaps, but never drop
    // text. A line of nothing but `>`s is a quotation's blank line, which
    // separates blocks as a blank line does.
    let kept: usize = got.iter().map(|c| solid(&c.text)).sum();
    let text: usize = md
        .lines()
        .filter(|l| !l.trim_start_matches([' ', '>']).trim().is_empty())
        .map(solid)
        .sum();
    assert!(kept >= text, "{options:?}: {md:?}");
    // Nor glue two words into one: every word in a chunk is part of one
    // in the Markdown.
    let words: Vec<&str> = md.split_whitespace().collect();
    for c in &got {
        for w in c.text.split_whitespace() {
            assert!(words.iter().any(|m| m.contains(w)), "{options:?}: {w:?} in {md:?}");
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let [max, overlap, rest @ ..] = data else {
        return;
    };
    let max_chars = usize::from(*max) + 1;
    let options = Options {
        max_chars,
        overlap: usize::from(*overlap) % max_chars,
    };
    let text = String::from_utf8_lossy(rest);
    check(&text, options);
    let doc = Document::parse(&text);
    check(&markdown::markdown(doc.root(), None), options);
});
