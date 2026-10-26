//! The layout readers on pages built to mislead them: stray prices,
//! badges, old prices written in words, buttons that aren't what they
//! seem, and shapes that cost too much to read.

use netweir_dom::Document;
use netweir_dom::auto::{Extracted, Schema, extract};
use netweir_dom::date::Civil;
use serde_json::Value;

fn read(schema: Schema, html: &str, url: &str) -> Extracted {
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

fn get<'e>(item: &'e Extracted, field: &str) -> &'e Value {
    &item.fields.iter().find(|(f, _)| *f == field).unwrap().1
}

const SHOP: &str = "https://shop.example/x";

fn product(html: &str) -> Extracted {
    read(Schema::Product, html, SHOP)
}

/// A page that isn't a product's: no name, no price, probability low.
fn not_a_product(html: &str) {
    let item = product(html);
    assert_eq!(get(&item, "price"), &Value::Null, "{html}");
    assert_eq!(get(&item, "name"), &Value::Null, "{html}");
    assert!(item.probability < 0.2, "{html}: {}", item.probability);
}

const GRID: &str = r#"<ul>
    <li class="t"><a href="/p/a"><h2>A</h2></a><span class="price">£10.00</span></li>
    <li class="t"><a href="/p/b"><h2>B</h2></a><span class="price">£11.00</span></li>
    <li class="t"><a href="/p/c"><h2>C</h2></a><span class="price">£12.00</span></li>
    <li class="t"><a href="/p/d"><h2>D</h2></a><span class="price">£13.00</span></li></ul>"#;

#[test]
fn a_stray_price_doesnt_make_a_product() {
    not_a_product(&format!(
        r#"<main><h1>Lamps</h1><p class="sub">Products from £19</p>{GRID}</main>"#
    ));
    not_a_product(
        r#"<main><article><h1>A night at the opera</h1><p>Tickets £15</p>
        <p>The new season opens with a production that runs for three hours and
        sells out most nights, so book early if you can.</p></article></main>"#,
    );
    not_a_product(
        r#"<main><h1>Our blog</h1><p><a href="/donate">Donate £5</a></p>
        <p>Words about the things we do every week, at some length.</p></main>"#,
    );
    // Cards without links, each with its own buy button.
    let card = |n: u32| {
        format!(
            r#"<div class="card" data-href="/p/{n}"><h2>Lamp {n}</h2><span class="price">£{n}.99</span>
            <button>Add to basket</button></div>"#
        )
    };
    not_a_product(&format!(
        "<main><h1>Lamps</h1>{}{}{}{}</main>",
        card(1),
        card(2),
        card(3),
        card(4)
    ));
}

#[test]
fn a_category_with_a_from_price_is_still_a_category() {
    let item = read(
        Schema::ProductList,
        &format!(r#"<main><h1>Lamps</h1><p class="sub">Products from £19</p>{GRID}</main>"#),
        SHOP,
    );
    assert_eq!(get(&item, "category_name"), "Lamps");
    assert!(item.probability > 0.7, "{}", item.probability);
}

#[test]
fn savings_badges_are_not_prices() {
    let item = product(
        r#"<main><h1>Oak stool</h1><span class="badge">Save £20</span><del>£120</del>
        <span class="price">£100</span><button>Add to basket</button></main>"#,
    );
    assert_eq!(get(&item, "price"), "100");
    assert_eq!(get(&item, "regular_price"), "120");
    let list = read(
        Schema::ProductList,
        r#"<ul>
        <li class="t"><a href="/p/a">Arc desk lamp</a><span class="badge">Save £10</span><span class="price">£49.99</span></li>
        <li class="t"><a href="/p/b">Dome pendant</a><span class="badge">£5 off</span><span class="price">£65.00</span></li>
        <li class="t"><a href="/p/c">Clip lamp</a><span class="price">£19.00</span></li></ul>"#,
        SHOP,
    );
    let prices: Vec<&Value> = get(&list, "products")
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["price"])
        .collect();
    assert_eq!(prices, ["49.99", "65.00", "19.00"]);
}

#[test]
fn a_structured_price_without_a_currency_takes_none_from_a_badge() {
    let item = product(
        r#"<script type="application/ld+json">{"@type": "Product", "name": "Lamp",
        "offers": {"price": "49.99"}}</script>
        <main><h1>Lamp</h1><span class="badge">Save $10</span>
        <span class="price">49,99 €</span></main>"#,
    );
    assert_eq!(get(&item, "price"), "49.99");
    assert_eq!(get(&item, "currency_raw"), "€");
}

#[test]
fn old_prices_said_in_words_or_classes_are_regular_prices() {
    for html in [
        r#"<main><h1>Oak stool</h1><p class="price">Was £120.00 Now £99.00</p><button>Add to basket</button></main>"#,
        r#"<main><h1>Oak stool</h1><p>RRP: £120.00</p><p class="price">£99.00</p><button>Add to basket</button></main>"#,
        r#"<main><h1>Oak stool</h1><span class="list-price">£120.00</span> <span class="sale-price">£99.00</span><button>Add to basket</button></main>"#,
        r#"<main><h1>Oak stool</h1><span class="price-item price-item--regular">£120.00</span> <span class="price-item price-item--sale">£99.00</span><button>Add to basket</button></main>"#,
    ] {
        let item = product(html);
        assert_eq!(get(&item, "price"), "99.00", "{html}");
        assert_eq!(get(&item, "regular_price"), "120.00", "{html}");
    }
}

#[test]
fn a_regular_price_class_alone_is_still_the_price() {
    // Shopify's themes mark the price "regular" when nothing is on sale.
    let item = product(
        r#"<main><h1>Oak stool</h1><div class="price__regular"><span class="price-item price-item--regular">£120.00</span></div>
        <button>Add to basket</button></main>"#,
    );
    assert_eq!(get(&item, "price"), "120.00");
    assert_eq!(get(&item, "regular_price"), &Value::Null);
}

#[test]
fn a_regular_price_is_kept_when_the_amounts_agree() {
    let item = product(
        r#"<script type="application/ld+json">{"@type": "Product", "name": "Lamp",
        "offers": {"price": "49.9", "priceCurrency": "GBP"}}</script>
        <main><h1>Lamp</h1><del>£60.00</del> <span class="price">£49.90</span></main>"#,
    );
    assert_eq!(get(&item, "regular_price"), "60.00");
}

fn not_a_job(html: &str) {
    let item = read(Schema::JobPosting, html, SHOP);
    assert_eq!(get(&item, "job_title"), &Value::Null, "{html}");
    assert!(item.probability < 0.2, "{html}: {}", item.probability);
}

#[test]
fn buttons_that_apply_something_else_dont_make_a_job() {
    not_a_job(&format!(
        r#"<main><h1>Lamps</h1><form><button>Apply filters</button></form>{GRID}</main>"#
    ));
    not_a_job(
        r#"<main><h1>Oak stool</h1><span class="price">£100</span>
        <form><input name="code"><button>Apply coupon</button></form></main>"#,
    );
}

#[test]
fn a_news_story_about_pay_isnt_a_job() {
    not_a_job(
        r#"<main><article><h1>MPs' pay to rise again</h1>
        <h2>Salaries climb to £91,346 a year from April</h2>
        <p>The rise follows two years of increases set by the standards body, which
        reviews pay every year and publishes its reasons.</p></article></main>"#,
    );
}

#[test]
fn a_job_with_an_apply_link_is_still_a_job() {
    let item = read(
        Schema::JobPosting,
        r#"<main><article><h1>Gardener</h1><p>£12.50 an hour</p>
        <a href="/apply/7">Apply for this job</a></article></main>"#,
        SHOP,
    );
    assert_eq!(get(&item, "job_title"), "Gardener");
    assert_eq!(get(&item, "base_salary")["rate_type"], "hourly");
}

#[test]
fn next_to_the_same_page_or_the_last_isnt_next() {
    let nav = |links: &str, url: &str| {
        read(
            Schema::ProductNavigation,
            &format!(r#"{GRID}<nav class="pagination">{links}</nav>"#),
            url,
        )
    };
    let same = nav(
        r#"<span class="current">4</span><a class="next" href="/c/lamps/?page=4">Next</a>"#,
        "https://shop.example/c/lamps?page=4",
    );
    assert_eq!(get(&same, "next_page"), &Value::Null);
    let last = nav(
        r#"<span class="current">2</span><a href="?page=3">3</a><a href="?page=10">»</a>"#,
        "https://shop.example/c/lamps?page=2",
    );
    assert_eq!(get(&last, "next_page"), &Value::Null);
}

#[test]
fn a_header_row_isnt_a_value() {
    let item = product(
        r#"<main><h1>Oak stool</h1><span class="price">£100</span><button>Add to basket</button>
        <table><tr><th>Model</th><th>Price</th></tr><tr><td>S1</td><td>£100</td></tr></table></main>"#,
    );
    assert_eq!(get(&item, "mpn"), &Value::Null);
}

#[test]
fn back_in_stock_soon_isnt_in_stock() {
    for note in ["Back in stock soon", "Email me when back in stock"] {
        let item = product(&format!(
            r#"<main><h1>Oak stool</h1><span class="price">£100</span><p>{note}</p></main>"#
        ));
        assert_eq!(get(&item, "availability"), &Value::Null, "{note}");
    }
}

#[test]
fn a_wrapper_called_description_isnt_the_description() {
    let item = product(
        r##"<script type="application/ld+json">{"@type": "Product", "name": "Lamp",
        "offers": {"price": "49.99", "priceCurrency": "GBP"}}</script>
        <main class="page-description-wrapper"><h1>Lamp</h1><span class="price">£49.99</span>
        <nav><a href="#r">Reviews (12)</a></nav>
        <div class="product-description"><p>A steel lamp with a linen shade that lights a desk.</p></div></main>"##,
    );
    assert_eq!(
        get(&item, "description"),
        "A steel lamp with a linen shade that lights a desk."
    );
}

#[test]
fn images_stay_near_the_product() {
    let item = product(
        r#"<main><h1>Lamp</h1><p>Some words about the lamp and why you'd want one.</p>
        <span class="price">£49.99</span><button>Add to basket</button>
        <section class="blog"><h2>From our blog</h2><img src="/blog/post.jpg" alt="Lamps in winter"></section></main>"#,
    );
    assert_eq!(get(&item, "main_image"), &Value::Null);
}

#[test]
fn many_small_numbers_before_the_buy_box_dont_hide_its_price() {
    let rows: String = (0..3000)
        .map(|i| format!("<li><span>{i}</span> reviews</li>"))
        .collect();
    let item = product(&format!(
        r#"<main><ul class="ratings">{rows}</ul><h1>Lamp</h1><span class="price">£49.99</span>
        <button>Add to basket</button></main>"#
    ));
    assert_eq!(get(&item, "price"), "49.99");
}

#[test]
fn a_thread_with_pages_isnt_a_listing() {
    let item = read(
        Schema::ProductList,
        r#"<main><h1>Help with my lamp</h1><p>It flickers. Any ideas what to check first?</p>
        <div class="pages"><span class="current">2</span><a href="?page=3">Next</a></div></main>"#,
        "https://forum.example/t/9?page=2",
    );
    assert_eq!(get(&item, "page_number"), &Value::Null);
    assert_eq!(get(&item, "pagination_next"), &Value::Null);
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
fn numbers_in_a_long_paragraph_are_quick() {
    let prose = "words of a long article ".repeat(40_000);
    quick(&format!(
        "<main><h1>Deep text</h1><p>{}{prose}</p></main>",
        "1<b></b>".repeat(2000)
    ));
}

#[test]
fn many_nested_linked_prices_are_quick() {
    let mut html = String::from("<main><h1>Nest</h1>");
    for i in 0..2000 {
        html.push_str(&format!(r#"<div><a href="/{i}">x</a><span>£{i}.00</span>"#));
    }
    html.push_str(&"</div>".repeat(2000));
    html.push_str(&"<p>junk</p>".repeat(100_000));
    html.push_str("</main>");
    quick(&html);
}

#[test]
fn nested_descriptions_are_quick() {
    let words = "word ".repeat(200);
    let mut html =
        String::from("<main><h1>Lamp</h1><span class=\"price\">£5</span></main><footer>");
    for _ in 0..3000 {
        html.push_str(&format!(r#"<div class="description company">{words}"#));
    }
    html.push_str(&"</div>".repeat(3000));
    html.push_str("</footer>");
    quick(&html);
}
