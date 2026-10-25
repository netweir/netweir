//! Auto-extraction over a corpus of pages written for it: each
//! `fixtures/auto/<name>.html` has a `<name>.json` with the item type to
//! ask for, the page's URL, every field that must have a value (the rest
//! must be null), the sources of some fields, the probability's range and
//! the fields with validation messages.

use std::path::Path;

use netweir_dom::Document;
use netweir_dom::auto::{Schema, extract};
use netweir_dom::date::Civil;
use serde_json::Value;

#[test]
fn every_auto_fixture_matches_its_answer() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/auto");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            (path.extension()? == "json")
                .then(|| path.file_stem().unwrap().to_string_lossy().into())
        })
        .collect();
    names.sort();
    assert!(names.len() >= 9, "the corpus is missing pages");
    let mut failures = Vec::new();
    for name in &names {
        let html = std::fs::read_to_string(dir.join(format!("{name}.html"))).unwrap();
        let want: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        let schema = Schema::from_name(want["type"].as_str().unwrap()).unwrap();
        let url = url::Url::parse(want["url"].as_str().unwrap()).unwrap();
        let doc = Document::parse(&html);
        let got = extract(schema, doc.root(), Some(&url), Civil::date(2026, 10, 25));
        for (field, value) in &got.fields {
            let expected = want["item"].get(*field).unwrap_or(&Value::Null);
            if value != expected {
                failures.push(format!("{name}: {field} is {value}, not {expected}"));
            }
        }
        for key in want["item"].as_object().unwrap().keys() {
            if !got.fields.iter().any(|(f, _)| f == key) {
                failures.push(format!("{name}: {key} isn't a field of {}", want["type"]));
            }
        }
        for (field, source) in want["sources"].as_object().unwrap() {
            let found = got
                .sources
                .iter()
                .find(|(f, _)| f == field)
                .map(|(_, s)| s.as_str());
            if found != source.as_str() {
                failures.push(format!(
                    "{name}: {field}'s source is {found:?}, not {source}"
                ));
            }
        }
        let (low, high) = (
            want["probability"][0].as_f64().unwrap(),
            want["probability"][1].as_f64().unwrap(),
        );
        if !(low..=high).contains(&got.probability) {
            failures.push(format!(
                "{name}: probability {} outside {low}..={high}",
                got.probability
            ));
        }
        let mut flagged: Vec<&str> = got.validation.iter().map(|(f, _)| *f).collect();
        flagged.sort();
        flagged.dedup();
        let wanted: Vec<&str> = want["validation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        if flagged != wanted {
            failures.push(format!(
                "{name}: validation on {flagged:?} ({:?}), not {wanted:?}",
                got.validation
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn hostile_structured_data_is_bounded() {
    let deep = format!(
        r#"<script type="application/ld+json">{}{}</script>"#,
        r#"{"@type": "Product", "name": "x", "a": "#.repeat(5000),
        "}".repeat(5001)
    );
    let big = format!(
        r#"<script type="application/ld+json">{{"@type": "Product", "name": "big", "description": "{}"}}</script>"#,
        "lamp ".repeat(1_000_000)
    );
    for html in [deep, big] {
        let doc = Document::parse(&html);
        let start = std::time::Instant::now();
        for schema in [
            Schema::Product,
            Schema::ProductList,
            Schema::ProductNavigation,
            Schema::JobPosting,
            Schema::Article,
        ] {
            extract(schema, doc.root(), None, Civil::date(2026, 10, 25));
        }
        assert!(start.elapsed().as_secs_f64() < 5.0, "{:?}", start.elapsed());
    }
}
