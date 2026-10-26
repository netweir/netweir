//! Any HTML read as every auto-extracted item type, and each of its
//! elements read as every kind of field, and its page type asked for:
//! nothing panics, and the same page gives the same answer every time.
#![no_main]

use libfuzzer_sys::fuzz_target;
use netweir_dom::auto::fields::{FieldKind, from_element};
use netweir_dom::auto::{Schema, extract};
use netweir_dom::date::Civil;
use netweir_dom::{Document, NodeKind, css_path};

const KINDS: [FieldKind; 24] = [
    FieldKind::Text,
    FieldKind::Html,
    FieldKind::Url,
    FieldKind::Price,
    FieldKind::Currency,
    FieldKind::CurrencyRaw,
    FieldKind::Availability,
    FieldKind::Gtin,
    FieldKind::Brand,
    FieldKind::Rating,
    FieldKind::Breadcrumbs,
    FieldKind::Date,
    FieldKind::Salary,
    FieldKind::EmploymentType,
    FieldKind::Image,
    FieldKind::Images,
    FieldKind::Texts,
    FieldKind::Organization,
    FieldKind::Location,
    FieldKind::Authors,
    FieldKind::Link,
    FieldKind::Request,
    FieldKind::Int,
    FieldKind::Media,
];

fuzz_target!(|data: &[u8]| {
    let html = String::from_utf8_lossy(data);
    let doc = Document::parse(&html);
    let url = url::Url::parse("https://example.com/a/b").unwrap();
    let now = Civil::date(2026, 10, 25);
    for schema in [
        Schema::Product,
        Schema::ProductList,
        Schema::ProductNavigation,
        Schema::JobPosting,
        Schema::Article,
    ] {
        let item = extract(schema, doc.root(), Some(&url), None, now);
        assert_eq!(item, extract(schema, doc.root(), Some(&url), None, now));
        assert!((0.0..=1.0).contains(&item.probability));
    }
    let (kind, all) = netweir_dom::auto::page_type(doc.root(), Some(&url), None, now);
    assert_eq!(
        (kind, all.clone()),
        netweir_dom::auto::page_type(doc.root(), Some(&url), None, now)
    );
    assert!(all.iter().all(|(_, p)| (0.0..=1.0).contains(p)));
    for node in doc.root().descendants().filter(|n| n.kind() == NodeKind::Element).take(64) {
        css_path::css_path(node);
        for kind in KINDS {
            from_element(kind, node, Some(&url), now);
        }
    }
});
