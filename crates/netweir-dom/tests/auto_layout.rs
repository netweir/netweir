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

#[test]
fn prose_with_a_date_or_a_byline_is_an_article_and_without_isnt() {
    let words = "<p>We opened the shop in a railway arch with four lamps and a borrowed till, \
        and for the first year most of our customers were people sheltering from the rain.</p>"
        .repeat(3);
    let typed = |extra: &str| {
        let html = format!(
            "<html><head>{extra}</head><body><main><h1>Our story</h1>{words}</main></body></html>"
        );
        let doc = Document::parse(&html);
        let url = url::Url::parse(SHOP).unwrap();
        netweir_dom::auto::page_type(doc.root(), Some(&url), None, Civil::date(2026, 10, 25)).0
    };
    assert_eq!(typed(""), None);
    assert_eq!(
        typed(r#"<meta property="og:type" content="article">"#),
        Some(Schema::Article)
    );
    assert_eq!(
        typed(r#"<meta property="article:published_time" content="2026-10-01">"#),
        Some(Schema::Article)
    );
}

const STORY: &str = r#"<main><article><h1>The night market returns</h1>
    <p class="byline">By <span class="author">Jo Park</span> · <time datetime="2026-10-20">20 October</time></p>
    <p>After two years away the night market is back on the quay, with forty stalls and a band on the old ferry steps every Friday until Christmas.</p>
    <p>Traders say the first night drew more people than any before the break, and the council has agreed to keep the lights up until midnight.</p>
    <p>Parking is limited, so the organisers are asking people to come on foot or by the late bus, which will run every twenty minutes.</p>
    </article>"#;

fn typed(html: &str) -> Option<Schema> {
    let doc = Document::parse(html);
    let url = url::Url::parse(SHOP).unwrap();
    netweir_dom::auto::page_type(doc.root(), Some(&url), None, Civil::date(2026, 10, 25)).0
}

#[test]
fn a_story_with_comments_isnt_a_thread() {
    let comment = |who: &str, when: &str| {
        format!(
            r#"<div class="comment"><span class="author">{who}</span> <time datetime="{when}">{when}</time>
            <div class="comment-body"><p>Went on Friday and it was wonderful, the band played until late.</p></div></div>"#
        )
    };
    let html = format!(
        r#"{STORY}<section class="responses"><h2>3 responses</h2>{}{}{}</section></main>"#,
        comment("a", "2026-10-21"),
        comment("b", "2026-10-22"),
        comment("c", "2026-10-23")
    );
    let thread = read(Schema::ForumThread, &html, SHOP);
    assert!(thread.probability < 0.4, "{}", thread.probability);
    assert_eq!(typed(&html), Some(Schema::Article));
}

#[test]
fn a_story_with_related_stories_isnt_a_list() {
    let related = |n: u32| {
        format!(
            r#"<li class="teaser"><h3><a href="/s/{n}">Another story about the quay, part {n}</a></h3></li>"#
        )
    };
    let html = format!(
        r#"{STORY}<aside class="more"><h2>More from the quay</h2><ul>{}{}{}</ul></aside></main>"#,
        related(1),
        related(2),
        related(3)
    );
    let list = read(Schema::ArticleList, &html, SHOP);
    assert!(list.probability < 0.4, "{}", list.probability);
    assert_eq!(typed(&html), Some(Schema::Article));
}

#[test]
fn a_shop_grid_isnt_an_article_list() {
    let html = format!(r#"<main><h1>Lamps</h1>{GRID}</main>"#);
    let list = read(Schema::ArticleList, &html, SHOP);
    assert!(list.probability < 0.4, "{}", list.probability);
    assert_eq!(typed(&html), Some(Schema::ProductList));
}

fn subcategories(html: &str, url: &str) -> Vec<(String, String)> {
    let item = read(Schema::ProductNavigation, html, url);
    match get(&item, "sub_categories") {
        Value::Array(list) => list
            .iter()
            .map(|c| {
                (
                    c["url"].as_str().unwrap().into(),
                    c["name"].as_str().unwrap().into(),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

const CATEGORY: &str = r#"<html><body>
<header><a href="/account">My account</a><a href="/basket">Basket</a></header>
<nav class="crumbs"><a href="/">Home</a> / <a href="/c/lamps">Lamps</a></nav>
<h1>Lamps</h1>
<ul class="subcategories"><li><a href="/c/lamps/desk">Desk lamps</a></li>
<li><a href="/c/lamps/floor">Floor lamps</a></li></ul>
<ul class="grid">
<li class="card"><a href="/p/arc"><h3>Arc lamp</h3></a><span class="price">£49.99</span></li>
<li class="card"><a href="/p/dome"><h3>Dome lamp</h3></a><span class="price">£29.00</span></li>
<li class="card"><a href="/p/clip"><h3>Clip lamp</h3></a><span class="price">£12.50</span></li></ul>
<a href="/c/lamps/wall">Wall lamps</a>
<nav class="pagination"><a rel="next" href="/c/lamps?page=2">Next</a></nav>
<footer><a href="/c/lamps/sale">Lamps on sale</a><a href="/help">Help</a></footer>
</body></html>"#;

#[test]
fn sub_categories_come_from_category_lists_and_child_paths() {
    let found = subcategories(CATEGORY, "https://shop.example/c/lamps");
    let urls: Vec<&str> = found.iter().map(|(u, _)| u.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://shop.example/c/lamps/desk",
            "https://shop.example/c/lamps/floor",
            "https://shop.example/c/lamps/wall",
        ]
    );
    assert_eq!(found[0].1, "Desk lamps");
}

#[test]
fn a_home_page_menu_gives_the_top_categories() {
    let html = r#"<html><body><header><nav class="main">
      <a href="/">Home</a><a href="/c/lamps">Lamps</a><a href="/c/rugs">Rugs</a>
      <a href="/c/chairs">Chairs</a><a href="/login">Sign in</a><a href="/basket">Basket</a>
      <a href="https://other.example/x">Partner</a></nav></header>
      <main><h1>Welcome</h1><p>Light, warmth, comfort.</p></main></body></html>"#;
    let found = subcategories(html, "https://shop.example/");
    let names: Vec<&str> = found.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(names, ["Lamps", "Rugs", "Chairs"]);
}

#[test]
fn item_links_and_pages_are_not_sub_categories() {
    let html = r##"<html><body><h1>Search: lamp</h1>
      <ul class="categories"><li><a href="/p/arc">Arc lamp</a></li><li><a href="/c/lamps?page=2">2</a></li>
      <li><a href="/c/lamps">Lamps</a></li><li><a href="javascript:void(0)">More</a></li><li><a href="#top">Top</a></li></ul>
      <ul class="grid">
      <li class="card"><a href="/p/arc"><h3>Arc lamp</h3></a><span class="price">£49.99</span></li>
      <li class="card"><a href="/p/dome"><h3>Dome lamp</h3></a><span class="price">£29.00</span></li>
      <li class="card"><a href="/p/clip"><h3>Clip lamp</h3></a><span class="price">£12.50</span></li></ul>
      <a rel="next" href="/c/lamps?page=2">Next</a></body></html>"##;
    let found = subcategories(html, "https://shop.example/c/lamps");
    let urls: Vec<&str> = found.iter().map(|(u, _)| u.as_str()).collect();
    assert_eq!(
        urls,
        Vec::<&str>::new(),
        "the page itself, an item, a page of it"
    );
}
