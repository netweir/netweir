//! The main content, Markdown and article fields of a corpus of realistic
//! pages: a news story, a blog post, a docs page, a product page, a forum
//! thread, a listing, a page of bare divs, and pages in French and
//! Japanese. Each fixture's JSON lists phrases that must be kept (in the
//! content), phrases that must be dropped (boilerplate), Markdown that
//! must appear, and the article's fields.
//!
//! Run with `--nocapture` to see each page's score.

use std::path::Path;

use netweir_dom::Document;
use netweir_dom::content::{article, main_content};
use serde_json::Value;

#[test]
fn the_corpus_reads_as_expected() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/content");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            (path.extension()? == "json").then(|| path.file_stem().unwrap().to_owned())
        })
        .collect();
    names.sort();
    assert!(names.len() >= 8, "the corpus is missing pages");
    let mut failures = Vec::new();
    println!(
        "{:<10} {:>8} {:>8} {:>9} {:>8}",
        "page", "kept", "dropped", "markdown", "fields"
    );
    for name in names {
        let name = name.to_string_lossy().to_string();
        let html = std::fs::read_to_string(dir.join(format!("{name}.html"))).unwrap();
        let want: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        let base = url::Url::parse(want["url"].as_str().unwrap()).unwrap();
        let doc = Document::parse(&html);
        let main = main_content(doc.root());
        let mut fail = |what: String| failures.push(format!("{name}: {what}"));

        let (text, md) = match &main {
            Some(m) => (m.clean_text(), m.markdown(Some(&base))),
            None => (String::new(), String::new()),
        };
        if want.get("main").is_some_and(Value::is_null) != main.is_none() {
            fail(format!("main content found: {}", main.is_some()));
        }
        let phrases = |key: &str| -> Vec<&str> {
            want[key]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default()
        };
        let score = |key: &str, hit: &dyn Fn(&str) -> bool, fail: &mut dyn FnMut(String)| {
            let all = phrases(key);
            let good = all.iter().filter(|p| hit(p)).count();
            for p in all.iter().filter(|p| !hit(p)) {
                fail(format!("{key}: {p:?}"));
            }
            format!("{good}/{}", all.len())
        };
        let kept = score("kept", &|p| text.contains(p), &mut fail);
        let dropped = score("dropped", &|p| !text.contains(p), &mut fail);
        let markdown = score("markdown", &|p| md.contains(p), &mut fail);

        let a = article(doc.root(), Some(&base));
        let got = [
            ("title", a.title),
            ("byline", a.byline),
            ("published", a.published),
            ("image", a.image),
            ("language", a.language),
        ];
        let mut right = 0;
        for (field, value) in &got {
            let expected = want["article"][field].as_str();
            if value.as_deref() == expected {
                right += 1;
            } else {
                fail(format!("article.{field}: {value:?}, wanted {expected:?}"));
            }
        }
        println!(
            "{name:<10} {kept:>8} {dropped:>8} {markdown:>9} {:>8}",
            format!("{right}/{}", got.len())
        );
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
