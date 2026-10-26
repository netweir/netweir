//! App state read for items on pages built to mislead it, and on state
//! built to cost too much.

use netweir_dom::Document;
use netweir_dom::auto::{Extracted, Schema, extract};
use netweir_dom::date::Civil;
use serde_json::Value;

fn read(schema: Schema, html: &str) -> Extracted {
    let doc = Document::parse(html);
    let url = url::Url::parse("https://shop.example/p/thing").unwrap();
    extract(
        schema,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 25),
    )
}

fn get<'e>(item: &'e Extracted, field: &str) -> &'e Value {
    &item.fields.iter().find(|(f, _)| *f == field).unwrap().1
}

fn next(data: &str) -> String {
    format!(r#"<script id="__NEXT_DATA__" type="application/json">{data}</script>"#)
}

#[test]
fn a_post_in_state_isnt_a_product() {
    let item = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"post":{"title":"Winter lamps","author":"Jo","body":"..."}}}}"#,
        ),
    );
    assert_eq!(get(&item, "name"), &Value::Null);
    assert!(item.probability < 0.2);
}

#[test]
fn a_basket_or_related_products_arent_the_product() {
    let item = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"basket":{"items":[{"name":"Mug","price":8}]},
            "related":[{"name":"A","price":1},{"name":"B","price":2},{"name":"C","price":3}]}}}"#,
        ),
    );
    assert_eq!(get(&item, "name"), &Value::Null);
}

#[test]
fn the_product_whose_url_is_the_pages_wins_among_listed_ones() {
    let item = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"items":[{"name":"Other","price":1,"url":"/p/other"},
            {"name":"Thing","price":2,"url":"/p/thing"}]}}}"#,
        ),
    );
    assert_eq!(get(&item, "name"), "Thing");
}

#[test]
fn structured_data_comes_before_state() {
    let item = read(
        Schema::Product,
        &format!(
            r#"<script type="application/ld+json">{{"@type": "Product", "name": "From JSON-LD",
            "offers": {{"price": "10.00", "priceCurrency": "GBP"}}}}</script>{}"#,
            next(
                r#"{"props":{"pageProps":{"product":{"name":"From state","price":12,"sku":"S-1"}}}}"#
            )
        ),
    );
    assert_eq!(get(&item, "name"), "From JSON-LD");
    assert_eq!(get(&item, "price"), "10.00");
    assert_eq!(get(&item, "sku"), "S-1");
}

#[test]
fn a_story_with_a_place_isnt_a_job() {
    let item = read(
        Schema::JobPosting,
        &next(
            r#"{"props":{"pageProps":{"article":{"title":"Floods in town","location":"Leeds","author":"Jo"}}}}"#,
        ),
    );
    assert_eq!(get(&item, "job_title"), &Value::Null);
}

fn quick(html: &str) {
    let doc = Document::parse(html);
    let start = std::time::Instant::now();
    for schema in [
        Schema::Product,
        Schema::ProductList,
        Schema::ProductNavigation,
        Schema::JobPosting,
    ] {
        extract(schema, doc.root(), None, None, Civil::date(2026, 10, 25));
    }
    let took = start.elapsed();
    assert!(took.as_secs_f64() < 3.0, "{took:?}");
}

#[test]
fn wide_state_is_quick() {
    let items: Vec<String> = (0..100_000)
        .map(|i| format!(r#"{{"name":"Item {i}","price":{i},"url":"/p/{i}"}}"#))
        .collect();
    quick(&next(&format!(
        r#"{{"props":{{"items":[{}]}}}}"#,
        items.join(",")
    )));
}

#[test]
fn shared_references_are_bounded() {
    // Each Apollo entry points at the next one twice, and each Nuxt 3
    // entry at the next one twice: written out, 2^200 objects.
    let mut apollo = String::from("window.__APOLLO_STATE__={");
    for i in 0..200 {
        apollo.push_str(&format!(
            r#""E:{i}":{{"name":"x","a":{{"__ref":"E:{n}"}},"b":{{"__ref":"E:{n}"}}}},"#,
            n = i + 1
        ));
    }
    apollo.push_str(r#""E:200":{"name":"end","price":1}};"#);
    quick(&format!("<script>{apollo}</script>"));
    let mut flat: Vec<String> = Vec::new();
    for i in 0..200 {
        flat.push(format!(r#"{{"a":{n},"b":{n}}}"#, n = i + 1));
    }
    flat.push(r#""end""#.into());
    quick(&format!(
        r#"<script type="application/json" id="__NUXT_DATA__">[{}]</script>"#,
        flat.join(",")
    ));
}
