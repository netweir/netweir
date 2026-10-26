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

fn read_at(schema: Schema, html: &str, url: &str) -> Extracted {
    let doc = Document::parse(html);
    let url = url::Url::parse(url).unwrap();
    extract(
        schema,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 25),
    )
}

#[test]
fn a_menu_in_state_isnt_a_listing() {
    let html = next(
        r#"{"props":{"pageProps":{"post":{"title":"Winter lamps"},"nav":[{"name":"Home","url":"/"},
        {"name":"About","url":"/about"},{"name":"Blog","url":"/blog"},{"name":"Contact","url":"/contact"}]}}}"#,
    );
    for schema in [Schema::ProductList, Schema::ProductNavigation] {
        let item = read(schema, &html);
        let field = if schema == Schema::ProductList {
            "products"
        } else {
            "items"
        };
        assert_eq!(get(&item, field), &Value::Null, "{schema:?}");
        assert!(item.probability < 0.2, "{schema:?}");
    }
}

#[test]
fn a_products_related_items_and_sizes_arent_a_listing() {
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/auto/product-next.html"
    ))
    .unwrap();
    let item = read(Schema::ProductList, &page);
    assert_eq!(get(&item, "products"), &Value::Null);
    assert!(item.probability < 0.4, "{}", item.probability);
    let sizes = next(
        r#"{"props":{"pageProps":{"product":{"name":"Tee","price":20,"sku":"T1","variants":[
        {"title":"S","price":20},{"title":"M","price":20},{"title":"L","price":20}]}}}}"#,
    );
    assert_eq!(
        get(&read(Schema::ProductList, &sizes), "products"),
        &Value::Null
    );
}

#[test]
fn a_basket_line_isnt_the_product() {
    let item = read_at(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"cart":{"lines":[{"quantity":2,"merchandise":{"product":
            {"title":"Mug","price":8,"sku":"M1","image":"/i/m.jpg"}}}]}}}}"#,
        ),
        "https://shop.example/cart",
    );
    assert_eq!(get(&item, "name"), &Value::Null);
}

#[test]
fn cents_written_as_text_are_cents() {
    let item = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"product":{"name":"Stool","priceInCents":"5900","currency":"USD","sku":"S1"}}}}"#,
        ),
    );
    assert_eq!(get(&item, "price"), "59.00");
    let negative = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"product":{"name":"Credit","priceInCents":-50,"currency":"USD","sku":"C1"}}}}"#,
        ),
    );
    assert_eq!(get(&negative, "price"), "-0.50");
    let odd = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"product":{"name":"Odd","price":{"centAmount":5900,"fractionDigits":20,"currencyCode":"EUR"},"sku":"O1"}}}}"#,
        ),
    );
    assert_eq!(get(&odd, "price"), &Value::Null);
}

#[test]
fn a_sale_price_is_the_price() {
    for (data, price, regular) in [
        (
            r#"{"name":"Stool","price":100,"salePrice":80,"currency":"USD","sku":"S"}"#,
            "80",
            "100",
        ),
        (
            r#"{"name":"Stool","price":120,"finalPrice":100,"currency":"USD","sku":"S"}"#,
            "100",
            "120",
        ),
        (
            r#"{"name":"Stool","price":{"regular":100,"sale":80},"currency":"USD","sku":"S"}"#,
            "80",
            "100",
        ),
    ] {
        let item = read(
            Schema::Product,
            &next(&format!(
                r#"{{"props":{{"pageProps":{{"product":{data}}}}}}}"#
            )),
        );
        assert_eq!(get(&item, "price"), price, "{data}");
        assert_eq!(get(&item, "regular_price"), regular, "{data}");
    }
}

#[test]
fn a_category_or_a_shipping_option_isnt_a_product() {
    for data in [
        r#"{"category":{"name":"Lamps","priceRange":"£19 - £65","description":"All our lamps"}}"#,
        r#"{"shippingOption":{"name":"Next day","price":5}}"#,
        r#"{"giftWrap":{"name":"Gift wrap","price":3,"id":"gw"}}"#,
    ] {
        let item = read_at(
            Schema::Product,
            &next(&format!(r#"{{"props":{{"pageProps":{data}}}}}"#)),
            "https://shop.example/c/lamps",
        );
        assert_eq!(get(&item, "name"), &Value::Null, "{data}");
    }
}

#[test]
fn news_and_testimonials_arent_jobs() {
    for data in [
        r#"{"article":{"title":"Floods in town","location":"Leeds","publishedAt":"2026-10-20"}}"#,
        r#"{"review":{"title":"Great service","company":"Acme","location":"Leeds"}}"#,
    ] {
        let item = read(
            Schema::JobPosting,
            &next(&format!(r#"{{"props":{{"pageProps":{data}}}}}"#)),
        );
        assert_eq!(get(&item, "job_title"), &Value::Null, "{data}");
    }
}

#[test]
fn a_nuxt_error_doesnt_lose_the_state() {
    let item = read(
        Schema::Product,
        r#"<script type="application/json" id="__NUXT_DATA__">[["ShallowReactive",1],{"data":2,"error":6},
        {"product":3},{"name":4,"price":5,"sku":9},"Clay mug",18.5,["NuxtError",7],{"message":8},"boom","M2"]</script>"#,
    );
    assert_eq!(get(&item, "name"), "Clay mug");
}

#[test]
fn a_slug_matches_whole_path_segments() {
    let item = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"items":[{"name":"Other","price":1,"slug":"g"},
            {"name":"Thing","price":2,"slug":"thing"}]}}}"#,
        ),
    );
    assert_eq!(get(&item, "name"), "Thing");
}

#[test]
fn nuxt_state_after_a_mention_of_it_is_read() {
    let item = read(
        Schema::Product,
        r#"<script>if(window.__NUXT__&&window.__NUXT__.state){};window.__NUXT__=(function(a,b){return {data:[{product:{name:a,price:b,sku:"L1"}}]}}("Lamp",49.99));</script>"#,
    );
    assert_eq!(get(&item, "name"), "Lamp");
}

#[test]
fn apollo_state_inside_next_data_is_followed() {
    let item = read(
        Schema::Product,
        &next(
            r#"{"props":{"pageProps":{"initialApolloState":{"ROOT_QUERY":{"p":{"__ref":"Product:1"}},
            "Product:1":{"__typename":"Product","name":"Lamp","price":10,"image":{"__ref":"Image:1"}},
            "Image:1":{"url":"/i/lamp.jpg"}}}}}"#,
        ),
    );
    assert_eq!(
        get(&item, "main_image")["url"],
        "https://shop.example/i/lamp.jpg"
    );
}

#[test]
fn a_row_timestamp_isnt_a_posting_date() {
    let item = read(
        Schema::JobPosting,
        &next(
            r#"{"props":{"pageProps":{"job":{"title":"Gardener","salary":"£12 an hour","employmentType":"PART_TIME","createdAt":"2020-01-01"}}}}"#,
        ),
    );
    assert_eq!(get(&item, "job_title"), "Gardener");
    assert_eq!(get(&item, "date_published"), &Value::Null);
}

#[test]
fn keys_with_dots_are_quoted_in_sources() {
    let item = read(
        Schema::Product,
        &next(r#"{"props":{"pageProps":{"a.b":{"name":"Lamp","price":10,"sku":"L1"}}}}"#),
    );
    let source = &item.sources.iter().find(|(f, _)| *f == "name").unwrap().1;
    assert_eq!(source, r#"js:__NEXT_DATA__.props.pageProps["a.b"].name"#);
}

#[test]
fn a_long_key_over_many_children_is_cheap() {
    let children: Vec<String> = (0..20_000).map(|i| format!(r#""c{i}":{{}}"#)).collect();
    quick(&next(&format!(
        r#"{{"props":{{"pageProps":{{"{}":{{{}}}}}}}}}"#,
        "k".repeat(100_000),
        children.join(",")
    )));
}

fn quick(html: &str) {
    let doc = Document::parse(html);
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
        // Each reader on its own, so adding readers doesn't eat the
        // others' time.
        let start = std::time::Instant::now();
        extract(schema, doc.root(), None, None, Civil::date(2026, 10, 25));
        let took = start.elapsed();
        assert!(took.as_secs_f64() < 1.5, "{schema:?}: {took:?}");
    }
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
