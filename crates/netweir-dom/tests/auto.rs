//! Auto-extraction over a corpus of pages written for it: each
//! `fixtures/auto/<name>.html` has a `<name>.json` with the item type to
//! ask for, the page's URL, every field that must have a value (the rest
//! must be null), the sources of some fields, the probability's range and
//! the fields with validation messages, and the page's type as
//! `page_type()` should tell it. "@body" and "@html" stand for
//! the article text and HTML that `content::article` reads.

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
        let got = extract(
            schema,
            doc.root(),
            Some(&url),
            None,
            Civil::date(2026, 10, 25),
        );
        // "@body" and "@html" stand for the article text and HTML
        // content::article reads, which its own corpus checks.
        let article = netweir_dom::content::article(doc.root(), Some(&url));
        for (field, value) in &got.fields {
            let expected = match want["item"].get(*field).unwrap_or(&Value::Null) {
                Value::String(s) if s == "@body" => &Value::from(article.text.clone()),
                Value::String(s) if s == "@html" => &Value::from(article.html.clone()),
                other => other,
            };
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
        let (typed, _) =
            netweir_dom::auto::page_type(doc.root(), Some(&url), None, Civil::date(2026, 10, 25));
        let typed = typed.map_or("other", Schema::name);
        if Some(typed) != want["page_type"].as_str() {
            failures.push(format!(
                "{name}: page type {typed}, not {}",
                want["page_type"]
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
            Schema::ArticleList,
            Schema::ArticleNavigation,
            Schema::JobPostingNavigation,
            Schema::ForumThread,
        ] {
            extract(schema, doc.root(), None, None, Civil::date(2026, 10, 25));
        }
        assert!(start.elapsed().as_secs_f64() < 5.0, "{:?}", start.elapsed());
    }
}

#[test]
fn a_microdata_product_on_a_big_page_is_quick() {
    // A ~1 MB page with the product's microdata in the middle, 14 plain
    // divs deep among 9,000 others: the sources' CSS paths must not cost a
    // document-wide search per ancestor per field.
    let row =
        |i: usize| format!(r#"<div class="row"><p>Paragraph {i} of boilerplate text.</p></div>"#);
    let mut html = String::from("<html><body>");
    (0..4500).for_each(|i| html.push_str(&row(i)));
    html.push_str(&"<div>".repeat(14));
    html.push_str(
        r#"<div itemscope itemtype="https://schema.org/Product"><h2 itemprop="name">Linen apron</h2>
           <img itemprop="image" src="apron.jpg"><span itemprop="brand">Loom</span>
           <meta itemprop="sku" content="AP-7">
           <div itemprop="offers" itemscope itemtype="https://schema.org/Offer">
           <span itemprop="price" content="28.00">28</span><meta itemprop="priceCurrency" content="USD">
           <link itemprop="availability" href="https://schema.org/InStock"></div>
           <div itemprop="description"><p>Washed linen.</p></div></div>"#,
    );
    html.push_str(&"</div>".repeat(14));
    (4500..9000).for_each(|i| html.push_str(&row(i)));
    html.push_str("</body></html>");
    let doc = Document::parse(&html);
    let url = url::Url::parse("https://shop.example/p/apron").unwrap();
    let start = std::time::Instant::now();
    let item = extract(
        Schema::Product,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 25),
    );
    let took = start.elapsed();
    assert_eq!(
        item.fields.iter().find(|(f, _)| *f == "price").unwrap().1,
        "28.00"
    );
    let source = &item.sources.iter().find(|(f, _)| *f == "sku").unwrap().1;
    let path = source.strip_prefix("microdata:").unwrap();
    let found = netweir_dom::Query::new(path).unwrap().select(doc.root());
    assert_eq!(found.len(), 1, "{path}");
    assert!(took.as_secs_f64() < 1.0, "{took:?}");
}

#[test]
fn a_list_price_equal_to_the_price_is_no_regular_price() {
    let html = r#"<script type="application/ld+json">{"@type": "Product", "name": "x",
        "offers": {"price": "59.990", "priceCurrency": "GBP",
        "priceSpecification": {"price": 59.99, "priceType": "https://schema.org/ListPrice"}}}</script>"#;
    let doc = Document::parse(html);
    let item = extract(
        Schema::Product,
        doc.root(),
        None,
        None,
        Civil::date(2026, 10, 25),
    );
    let get = |f: &str| item.fields.iter().find(|(n, _)| *n == f).unwrap().1.clone();
    assert_eq!(get("price"), "59.990");
    assert_eq!(get("regular_price"), Value::Null);
}

#[test]
fn hostile_layouts_are_bounded() {
    // 20,000 priced cards, a price 3,000 elements deep and 150,000
    // prices in one paragraph.
    let mut cards = String::from("<h1>Lamps</h1>");
    for i in 0..20_000 {
        cards.push_str(&format!(
            r#"<div class="c"><a href="/p/{i}">Lamp {i}</a><span>£{i}.00</span></div>"#
        ));
    }
    let deep = format!(
        "<h1>Deep</h1>{}<span>£1.00</span>{}",
        "<div>".repeat(3000),
        "</div>".repeat(3000)
    );
    let spans = format!(
        "<h1>Spans</h1><p>{}</p>",
        "<span>£9.99</span> ".repeat(150_000)
    );
    for (case, html) in [cards, deep, spans].into_iter().enumerate() {
        let doc = Document::parse(&html);
        for schema in [
            Schema::Product,
            Schema::ProductList,
            Schema::ProductNavigation,
            Schema::JobPosting,
            Schema::ArticleList,
            Schema::ArticleNavigation,
            Schema::JobPostingNavigation,
            Schema::ForumThread,
        ] {
            let start = std::time::Instant::now();
            extract(schema, doc.root(), None, None, Civil::date(2026, 10, 25));
            let took = start.elapsed();
            // About a second at most in a debug build on a laptop, and
            // twice that on a busy CI runner. Quadratic work on 150,000
            // spans would take minutes.
            assert!(
                took.as_secs_f64() < 5.0,
                "case {case}, {schema:?}: {took:?}"
            );
        }
    }
}

#[test]
fn restaurant_subtype_is_a_business_place() {
    let html = r#"<script type="application/ld+json">{"@context":"https://schema.org",
        "@type":"Restaurant","name":"Lumo","url":"https://lumo.example/",
        "address":{"@type":"PostalAddress","streetAddress":"1 Iron St"}}</script>"#;
    let doc = Document::parse(html);
    let url = url::Url::parse("https://lumo.example/").unwrap();
    let got = extract(
        Schema::BusinessPlace,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 27),
    );
    let name = got.fields.iter().find(|(f, _)| *f == "name").unwrap();
    assert_eq!(name.1, "Lumo");
    assert!(got.probability >= 0.9, "{}", got.probability);
}

#[test]
fn opening_hours_string_expands_per_day() {
    use netweir_dom::auto::fields::opening_hours;
    let got = opening_hours(&serde_json::json!(
        "Mo-Fr 09:00-17:30, Sa 10:00-14:00, Su closed"
    ))
    .unwrap();
    let days: Vec<&str> = got
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["day_of_week"].as_str().unwrap())
        .collect();
    assert_eq!(
        days,
        [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday"
        ]
    );
    assert_eq!(
        got[0],
        serde_json::json!({"day_of_week": "Monday", "opens": "09:00:00", "closes": "17:30:00",
            "raw_day_of_week": "Mo-Fr", "raw_opens": "09:00", "raw_closes": "17:30"})
    );
    assert_eq!(got[5]["closes"], "14:00:00");
}

#[test]
fn a_postal_address_is_split_and_a_string_is_raw() {
    use netweir_dom::auto::fields::address;
    let got = address(&serde_json::json!({"streetAddress": "1 Iron St",
        "addressLocality": "Manchester", "postalCode": "M1 1AA", "addressCountry": "GB"}))
    .unwrap();
    assert_eq!(got["street_address"], "1 Iron St");
    assert_eq!(got["address_locality"], "Manchester");
    assert_eq!(got["address_city"], serde_json::Value::Null);
    assert_eq!(got["postal_code"], "M1 1AA");
    assert_eq!(got["address_country"], "GB");
    assert_eq!(got["address_raw"], "1 Iron St, Manchester, M1 1AA, GB");
    let raw = address(&serde_json::json!("1 Iron St, Manchester")).unwrap();
    assert_eq!(raw["address_raw"], "1 Iron St, Manchester");
    assert_eq!(raw["street_address"], serde_json::Value::Null);
    // A country written as an object gives its name.
    let nested =
        address(&serde_json::json!({"addressCountry": {"@type": "Country", "name": "GB"}}))
            .unwrap();
    assert_eq!(nested["address_country"], "GB");
}

#[test]
fn areas_read_in_their_units() {
    use netweir_dom::auto::fields::area;
    use serde_json::json;
    let read =
        |v: serde_json::Value| area(&v).map(|a| (a["value"].clone(), a["unit_code"].clone()));
    assert_eq!(read(json!("120 m²")), Some((json!(120.0), json!("SQMT"))));
    assert_eq!(
        read(json!("1,250 sq ft")),
        Some((json!(1250.0), json!("SQFT")))
    );
    assert_eq!(read(json!("0.5 acres")), Some((json!(0.5), json!("ACRE"))));
    let q = area(&json!({"value": 120, "unitCode": "MTK"})).unwrap();
    assert_eq!(q["unit_code"], "SQMT");
    assert_eq!(q["raw"], "120 MTK");
    assert_eq!(read(json!("big")), None);
}

#[test]
fn a_rent_is_not_a_salary() {
    let html = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/auto/estate-rent-dom.html"),
    )
    .unwrap();
    let doc = Document::parse(&html);
    let url = url::Url::parse("https://citylets.example/flat/5521").unwrap();
    let job = extract(
        Schema::JobPosting,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 27),
    );
    assert!(job.probability < 0.1, "{}", job.probability);
}

#[test]
fn serp_metadata_names_the_query() {
    let html = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/auto/serp-dom.html"),
    )
    .unwrap();
    let doc = Document::parse(&html);
    let url = url::Url::parse("https://search.example/search?q=rust+html+parser").unwrap();
    let got = extract(
        Schema::Serp,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 27),
    );
    let meta: std::collections::HashMap<_, _> = got.extra_metadata.into_iter().collect();
    assert_eq!(meta["searched_query"], "rust html parser");
    assert_eq!(meta["displayed_query"], "rust html parser");
    assert_eq!(meta["total_organic_results"], 1230);
}

#[test]
fn item_main_xpath_finds_the_main_element() {
    for (name, url) in [
        ("content-page", "https://kitchen.example/notes/cast-iron"),
        ("content-page-no-landmarks", "https://hall.example/times"),
    ] {
        let html = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/auto/{name}.html")),
        )
        .unwrap();
        let doc = Document::parse(&html);
        let url = url::Url::parse(url).unwrap();
        let got = extract(
            Schema::PageContent,
            doc.root(),
            Some(&url),
            None,
            Civil::date(2026, 10, 27),
        );
        let field = |f: &str| got.fields.iter().find(|(n, _)| *n == f).unwrap().1.clone();
        let main = field("item_main");
        let main = main
            .as_str()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let path = field("item_main_xpath");
        let found = netweir_dom::extract::Selector::xpath(path.as_str().unwrap())
            .unwrap()
            .hits(doc.root())
            .unwrap();
        assert_eq!(found.len(), 1, "{name}: {path}");
        let netweir_dom::Hit::Node(n) = found[0] else {
            panic!("{name}: not an element")
        };
        let text = netweir_dom::markdown::clean_text(n);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(text.contains(&main[..40]), "{name}: {text}");
    }
}
