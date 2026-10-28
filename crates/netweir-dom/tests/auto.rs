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
        for schema in Schema::ALL {
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
        for schema in Schema::ALL {
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
fn item_main_xpath_holds_in_one_huge_div() {
    let html = format!(
        "<div>{}</div>",
        "<p>One paragraph of a long page that has no landmarks at all, only text in a single div.</p>"
            .repeat(2000)
    );
    let doc = Document::parse(&html);
    let url = url::Url::parse("https://big.example/").unwrap();
    let got = extract(
        Schema::PageContent,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 27),
    );
    let path = got
        .fields
        .iter()
        .find(|(n, _)| *n == "item_main_xpath")
        .unwrap()
        .1
        .clone();
    let found = netweir_dom::extract::Selector::xpath(path.as_str().unwrap())
        .unwrap()
        .hits(doc.root())
        .unwrap();
    assert_eq!(found.len(), 1, "{path}");
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

fn typed(html: &str, url: &str) -> &'static str {
    let doc = Document::parse(html);
    let url = url::Url::parse(url).unwrap();
    netweir_dom::auto::page_type(doc.root(), Some(&url), None, Civil::date(2026, 10, 27))
        .0
        .map_or("other", Schema::name)
}

fn read_as(
    schema: Schema,
    html: &str,
    url: &str,
) -> std::collections::HashMap<&'static str, serde_json::Value> {
    let doc = Document::parse(html);
    let url = url::Url::parse(url).unwrap();
    extract(
        schema,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 27),
    )
    .fields
    .into_iter()
    .collect()
}

fn timed(schema: Schema, html: &str, url: &str) -> f64 {
    let doc = Document::parse(html);
    let url = url::Url::parse(url).unwrap();
    let start = std::time::Instant::now();
    extract(
        schema,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 27),
    );
    start.elapsed().as_secs_f64()
}

#[test]
fn nested_hours_lists_are_bounded() {
    let html = format!(
        r#"<a href="tel:0123">0123</a><address>1 Mill Lane</address>{}{}"#,
        "<ul><li>Mon 9:00 - 17:00".repeat(10_000),
        "</li></ul>".repeat(10_000)
    );
    let took = timed(Schema::BusinessPlace, &html, "https://a.example/visit");
    assert!(took < 5.0, "{took}");
}

#[test]
fn deep_serp_cards_are_bounded() {
    let card = |i: usize, inner: &str| {
        format!(
            r#"<div class="r"><h3><a href="https://s{i}.example/">R {i}</a></h3><div>{inner}</div></div>"#
        )
    };
    let deep = format!("{}text{}", "<div>".repeat(20_000), "</div>".repeat(20_000));
    let html = format!("{}{}{}", card(0, &deep), card(1, "one"), card(2, "two"));
    let took = timed(Schema::Serp, &html, "https://search.example/s?q=x");
    assert!(took < 5.0, "{took}");
}

#[test]
fn a_big_property_page_is_bounded() {
    let html = format!(
        "<h1>Flat</h1><main>{}</main>",
        "£12 <b>a</b> 3 bed ".repeat(300_000)
    );
    let took = timed(Schema::RealEstate, &html, "https://homes.example/p/1");
    assert!(took < 5.0, "{took}");
}

#[test]
fn a_place_without_a_url_types_its_own_page() {
    let html = r#"<script type="application/ld+json">{"@context":"https://schema.org","@type":"Restaurant",
        "name":"Lumo Manchester","address":{"@type":"PostalAddress","streetAddress":"1 Iron St"},
        "openingHours":"Mo-Su 12:00-22:00"}</script><h1>Lumo Manchester</h1>"#;
    assert_eq!(
        typed(html, "https://lumo.example/locations/manchester"),
        "business-place"
    );
}

#[test]
fn a_shops_home_page_is_its_listing_not_its_store() {
    let cards: String = (0..6)
        .map(|i| format!(r#"<div class="p"><a href="/p/{i}">Lamp {i}</a><span class="price">£{i}9.00</span></div>"#))
        .collect();
    let html = format!(
        r#"<script type="application/ld+json">{{"@context":"https://schema.org","@type":"Store","name":"Lumo",
        "url":"https://shop.example/","address":{{"@type":"PostalAddress","streetAddress":"1 Iron St"}}}}</script>
        <h1>Lumo lamps</h1><main>{cards}</main>"#
    );
    assert_eq!(typed(&html, "https://shop.example/"), "product-list");
}

#[test]
fn nested_places_and_homes_are_not_the_page() {
    let event = r#"<script type="application/ld+json">{"@context":"https://schema.org","@type":"Event",
        "name":"Folk night","location":{"@type":"Place","name":"The Mill","address":"1 Mill Lane"}}</script>"#;
    assert_ne!(typed(event, "https://mill.example/"), "business-place");
    let hotel = r#"<script type="application/ld+json">{"@context":"https://schema.org","@type":"Hotel",
        "name":"The Grand","url":"https://grand.example/","address":{"@type":"PostalAddress","streetAddress":"2 Sea Rd"},
        "containsPlace":{"@type":"Suite","name":"Sea view suite","numberOfRooms":2}}</script>"#;
    assert_eq!(typed(hotel, "https://grand.example/"), "business-place");
    assert!(read_as(Schema::RealEstate, hotel, "https://grand.example/")["name"].is_null());
}

#[test]
fn links_pages_and_search_boxes_are_not_serps() {
    let links: String = (0..5)
        .map(|i| format!(r#"<div class="l"><h3><a href="https://s{i}.example/">Link {i}</a></h3><p>Why it's good.</p></div>"#))
        .collect();
    let roundup = format!("<title>Five links - Ada's blog</title><main>{links}</main>");
    assert_ne!(typed(&roundup, "https://ada.example/?p=123"), "serp");
    let aggregator = format!(r#"<form><input name="q" value="Search"></form><main>{links}</main>"#);
    assert_ne!(typed(&aggregator, "https://news.example/"), "serp");
}

#[test]
fn a_blog_post_at_a_posts_slug_is_not_a_social_post() {
    let html = r#"<meta property="og:type" content="article"><meta property="og:title" content="Rust tips">
        <meta property="og:description" content="Ten small things. #rust">
        <article><h1>Rust tips</h1><p class="byline">By Ada, 2 May 2026</p>
        <p>Ten small things that made my Rust faster this year, with numbers for each and a note on when not to bother.</p>
        <p>Most of them are about allocation, and two are about the borrow checker telling you something useful.</p></article>"#;
    assert_eq!(
        typed(html, "https://ada.example/posts/2026-rust-tips"),
        "article"
    );
}

#[test]
fn a_sale_isnt_read_as_a_rent() {
    let html = r#"<main><h1>Cottage</h1><p class="price">£325,000</p><p>Part exchange considered</p>
        <ul><li>3 bedrooms</li><li>1 bathroom</li><li>1,050 sq ft</li></ul><p>For sale.</p>
        <aside>Looking for homes to rent? See our lettings.</aside></main>"#;
    let got = read_as(Schema::RealEstate, html, "https://homes.example/p/9");
    assert_eq!(got["trade_type"], "sale");
    assert!(got["rental_period"].is_null());
}

#[test]
fn european_areas_read_their_separators() {
    use netweir_dom::auto::fields::area;
    use serde_json::json;
    assert_eq!(area(&json!("85,5 m²")).unwrap()["value"], json!(85.5));
    assert_eq!(area(&json!("1.250 m²")).unwrap()["value"], json!(1250.0));
    assert_eq!(
        area(&json!("1,250.5 sq ft")).unwrap()["value"],
        json!(1250.5)
    );
}

#[test]
fn small_misreads_stay_fixed() {
    use netweir_dom::auto::fields::opening_hours;
    let weds = opening_hours(&serde_json::json!("Weds 09:00-17:00")).unwrap();
    assert_eq!(weds[0]["day_of_week"], "Wednesday");
    let html = r#"<main><h1>Flat in Bath</h1><p class="price">£900 pcm</p><p>Bath: 2 bedroom flat, 60 m²</p></main>"#;
    let got = read_as(Schema::RealEstate, html, "https://lets.example/f/1");
    assert!(
        got["number_of_bathrooms_total"].is_null(),
        "{:?}",
        got["number_of_bathrooms_total"]
    );
    assert_eq!(got["number_of_bedrooms"], 2);
}
