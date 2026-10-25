//! Tracked selectors against hand-made redesigns: the element found before
//! a redesign is found again after it, or, when it's gone, nothing is.

use netweir_dom::track::{Fingerprint, THRESHOLD, relocate};
use netweir_dom::{Document, Query};

#[derive(serde::Deserialize)]
struct Case {
    case: String,
    query: String,
    after: Option<String>,
}

fn page(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/redesigns/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn every_redesign_case() {
    let cases: Vec<Case> = serde_json::from_str(&page("cases.json")).unwrap();
    let mut failures = Vec::new();
    for case in cases {
        let before = Document::parse(&page(&format!("{}.before.html", case.case)));
        let after = Document::parse(&page(&format!("{}.after.html", case.case)));
        let query = Query::new(&case.query).unwrap();
        let found = query.select(before.root());
        let element = found
            .first()
            .unwrap_or_else(|| panic!("{}: no match before", case.case));
        let fp = Fingerprint::of(*element);
        assert!(
            query.select(after.root()).is_empty(),
            "{}: the selector still works",
            case.case
        );
        // The fingerprint survives being saved and read back.
        let fp = Fingerprint::from_json(&fp.to_json()).unwrap();
        let got = relocate(after.root(), &fp, THRESHOLD);
        let got_text = got.as_ref().map(|(n, _)| n.text().trim().to_string());
        let score = got.as_ref().map(|(_, s)| *s);
        let best = netweir_dom::track::best(after.root(), &fp).map(|(n, s)| (n.text(), s));
        println!("{:16} best {:?}", case.case, best);
        if got_text != case.after {
            failures.push(format!(
                "{}: got {got_text:?} ({score:?}), want {:?}",
                case.case, case.after
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn an_unchanged_page_scores_one() {
    let html = page("class-renamed.before.html");
    let doc = Document::parse(&html);
    let el = Query::new("p.price_color").unwrap().select(doc.root())[0];
    let (found, score) = relocate(doc.root(), &Fingerprint::of(el), THRESHOLD).unwrap();
    assert_eq!(found, el);
    assert!((score - 1.0).abs() < 1e-9, "{score}");
}

use std::collections::HashMap;
use std::sync::Mutex;

use netweir_dom::Hit;
use netweir_dom::extract::{
    Field, ItemSpec, Selector, TrackContext, Tracker, Tracking, Tracks, Value,
};

#[derive(Default)]
struct Memory(Mutex<HashMap<(String, String), String>>);

impl Tracks for Memory {
    fn get(&self, site: &str, name: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .get(&(site.into(), name.into()))
            .cloned()
    }
    fn put(&self, site: &str, name: &str, fingerprint: &str) {
        self.0
            .lock()
            .unwrap()
            .insert((site.into(), name.into()), fingerprint.into());
    }
}

fn values(hits: &[Hit<'_>]) -> Vec<String> {
    hits.iter().map(Hit::to_string_value).collect()
}

#[test]
fn a_tracked_query_learns_then_relocates() {
    let memory = Memory::default();
    let ctx = TrackContext {
        tracks: &memory,
        site: "shop.example",
        threshold: THRESHOLD,
    };
    let before = Document::parse(&page("class-renamed.before.html"));
    let after = Document::parse(&page("class-renamed.after.html"));
    for (kind, query) in [
        ("css", "p.price_color::text"),
        ("xpath", "//p[@class='price_color']/text()"),
    ] {
        let tracker = Tracker::new("price", kind, query).unwrap();
        let (hits, tracking) = tracker.run(before.root(), &ctx).unwrap();
        assert_eq!(
            (values(&hits), tracking),
            (vec!["£24.99".to_string()], Tracking::Matched)
        );
        let (hits, tracking) = tracker.run(after.root(), &ctx).unwrap();
        assert_eq!(values(&hits), ["£24.99"], "{kind}");
        assert!(
            matches!(tracking, Tracking::Relocated(s) if s >= THRESHOLD),
            "{tracking:?}"
        );
    }
    // Another site's fingerprint is its own.
    let other = TrackContext {
        site: "other.example",
        ..ctx
    };
    let tracker = Tracker::new("price", "css", "p.price_color::text").unwrap();
    assert_eq!(
        tracker.run(after.root(), &other).unwrap().1,
        Tracking::Unknown
    );
}

#[test]
fn a_tracked_field_in_an_item() {
    let memory = Memory::default();
    let ctx = TrackContext {
        tracks: &memory,
        site: "s",
        threshold: THRESHOLD,
    };
    let field = |q: &str| {
        Field::new("price", Selector::css(q).unwrap())
            .track(Tracker::new("price", "css", q).unwrap())
    };
    let spec = ItemSpec::new(vec![field("p.price_color::text")]);
    let before = Document::parse(&page("class-renamed.before.html"));
    let after = Document::parse(&page("class-renamed.after.html"));
    let (v, notes) = spec.extract_with(before.root(), None, &ctx);
    assert_eq!((v, notes), (vec![Value::Text("£24.99".into())], vec![]));
    let (v, notes) = spec.extract_with(after.root(), None, &ctx);
    assert_eq!(v, vec![Value::Text("£24.99".into())]);
    assert!(matches!(notes[..], [(0, Tracking::Relocated(_))]));
    // Without a context, a tracked field is an ordinary one.
    assert_eq!(spec.extract(after.root(), None), vec![Value::Missing]);
}

#[test]
fn what_can_be_tracked() {
    let err = |kind: &str, q: &str| Tracker::new("x", kind, q).err().unwrap();
    assert!(err("css", "a, b").contains("list"));
    assert!(err("css", "::text").contains("element"));
    assert!(Tracker::new("x", "css", "a[title='a, b']::text").is_ok());
    assert!(Tracker::new("x", "xpath", "//a/@href").is_ok());
}

fn product(title: &str, price: &str, class: &str) -> String {
    format!(
        "<html><body><main><h1>{title}</h1><p class=\"{class}\">{price}</p>\
         <p class=\"stock\">In stock</p></main></body></html>"
    )
}

#[test]
fn across_a_crawl_varying_text_does_not_stop_relocation() {
    let memory = Memory::default();
    let ctx = TrackContext {
        tracks: &memory,
        site: "shop",
        threshold: THRESHOLD,
    };
    let tracker = Tracker::new("price", "css", "p.price_color::text").unwrap();
    // Two ordinary product pages teach it that the text and the title before
    // the price change from page to page.
    for (title, price) in [("Blue Kettle", "£24.99"), ("Red Toaster", "£31.50")] {
        let doc = Document::parse(&product(title, price, "price_color"));
        assert_eq!(tracker.run(doc.root(), &ctx).unwrap().1, Tracking::Matched);
    }
    // After a redesign, a third product, with another price.
    let doc = Document::parse(&product("Green Mug", "£8.75", "ProductPrice"));
    let (hits, tracking) = tracker.run(doc.root(), &ctx).unwrap();
    assert_eq!(values(&hits), ["£8.75"]);
    assert!(matches!(tracking, Tracking::Relocated(_)), "{tracking:?}");
}

#[test]
fn relocation_is_quick_on_a_big_page() {
    let mut html = String::from("<html><body>");
    for i in 0..18_000 {
        html.push_str(&format!("<div class=\"c{}\">cell {i}</div>", i % 7));
    }
    html.push_str("<div id=\"target\" class=\"special\">Target text</div></body></html>");
    let doc = Document::parse(&html);
    let el = Query::new("#target").unwrap().select(doc.root())[0];
    let fp = Fingerprint::of(el);
    let page = Document::parse(&html.replace("id=\"target\" class=\"special\"", ""));
    let start = std::time::Instant::now();
    let _ = netweir_dom::track::best(page.root(), &fp);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn a_tracked_query_must_find_elements() {
    let memory = Memory::default();
    let ctx = TrackContext {
        tracks: &memory,
        site: "s",
        threshold: THRESHOLD,
    };
    let doc = Document::parse("<p>a</p><p>b</p>");
    let tracker = Tracker::new("x", "xpath", "//p/text()[1]").unwrap();
    let err = tracker.run(doc.root(), &ctx).err().unwrap();
    assert!(err.contains("element"), "{err}");
}
