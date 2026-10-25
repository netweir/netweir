//! Declarative extraction: fields, conversions and link rules, all without
//! leaving Rust.

use netweir_dom::Document;
use netweir_dom::extract::{Convert, Field, ItemSpec, Selector, Value, links};

const PAGE: &str = r#"<html><body>
<h1>  A Light in the Attic  </h1>
<p class="price_color">£51.77</p>
<p class="stock">In stock (22 available)</p>
<table><tr><th>UPC</th><td>a897fe39b1053632</td></tr><tr><th>Reviews</th><td>0</td></tr></table>
<ul><li class="tag">poetry</li><li class="tag">classics</li><li class="tag">verse</li></ul>
<a href="/a" class="link">A</a><a class="link">no href</a><a href="b.html" class="link">B</a>
<span data-n="1,234">n</span>
</body></html>"#;

fn css(name: &str, q: &str) -> Field {
    Field::new(name, Selector::css(q).unwrap())
}

fn one(field: Field) -> Value {
    let doc = Document::parse(PAGE);
    let spec = ItemSpec::new(vec![field]);
    spec.extract(doc.root(), None).remove(0)
}

#[test]
fn a_field_is_the_first_result() {
    assert_eq!(
        one(css("t", "h1::text")),
        Value::Text("  A Light in the Attic  ".into())
    );
    assert_eq!(
        one(css("t", "h1::text").strip(true)),
        Value::Text("A Light in the Attic".into())
    );
    assert_eq!(one(css("t", "h2::text")), Value::Missing);
    // An element gives its HTML, as get() does.
    assert_eq!(
        one(css("t", "p.price_color")),
        Value::Text("<p class=\"price_color\">£51.77</p>".into())
    );
}

#[test]
fn all_gives_every_result() {
    assert_eq!(
        one(css("tags", ".tag::text").all(true)),
        Value::List(vec![
            Value::Text("poetry".into()),
            Value::Text("classics".into()),
            Value::Text("verse".into())
        ])
    );
    assert_eq!(one(css("x", ".none::text").all(true)), Value::List(vec![]));
}

#[test]
fn a_pattern_keeps_its_first_group_or_the_match() {
    let f = css("p", ".price_color::text").re(r"[\d.]+").unwrap();
    assert_eq!(one(f), Value::Text("51.77".into()));
    let f = css("n", ".stock::text").re(r"\((\d+) available").unwrap();
    assert_eq!(one(f), Value::Text("22".into()));
    let f = css("n", ".stock::text").re(r"sold out").unwrap();
    assert_eq!(one(f), Value::Missing);
    let f = css("t", ".tag::text").re(r"^(\w)").unwrap().all(true);
    assert_eq!(
        one(f),
        Value::List(vec![
            Value::Text("p".into()),
            Value::Text("c".into()),
            Value::Text("v".into())
        ])
    );
    assert!(
        css("bad", "p").re("(").is_err(),
        "a bad pattern is an error up front"
    );
}

#[test]
fn conversions_run_in_rust_and_report_what_failed() {
    let f = css("p", ".price_color::text")
        .re(r"[\d.]+")
        .unwrap()
        .convert(Convert::Float);
    assert_eq!(one(f), Value::Float(51.77));
    let f = css("n", ".stock::text")
        .re(r"(\d+)")
        .unwrap()
        .convert(Convert::Int);
    assert_eq!(one(f), Value::Int(22));
    let f = css("p", ".price_color::text").convert(Convert::Float);
    assert_eq!(one(f), Value::Invalid("£51.77".into()));
    let f = css("n", "span::attr(data-n)").convert(Convert::Int);
    assert_eq!(one(f), Value::Invalid("1,234".into()));
    let f = Field::new(
        "r",
        Selector::xpath("//th[.='Reviews']/following-sibling::td/text()").unwrap(),
    )
    .convert(Convert::Bool);
    assert_eq!(one(f), Value::Bool(false));
    // Missing stays missing whatever the conversion.
    assert_eq!(
        one(css("x", ".none::text").convert(Convert::Int)),
        Value::Missing
    );
}

#[test]
fn xpath_fields() {
    let f = Field::new(
        "upc",
        Selector::xpath("//th[.='UPC']/following-sibling::td/text()").unwrap(),
    );
    assert_eq!(one(f), Value::Text("a897fe39b1053632".into()));
    let f = Field::new("n", Selector::xpath("count(//li)").unwrap()).convert(Convert::Float);
    assert_eq!(one(f), Value::Float(3.0));
    assert!(Selector::xpath("//p[").is_err());
}

#[test]
fn an_item_has_one_value_per_field_in_order() {
    let doc = Document::parse(PAGE);
    let spec = ItemSpec::new(vec![
        css("title", "h1::text").strip(true),
        css("tags", ".tag::text").all(true),
    ]);
    assert_eq!(spec.names(), ["title", "tags"]);
    let values = spec.extract(doc.root(), None);
    assert_eq!(values.len(), 2);
    assert_eq!(values[0], Value::Text("A Light in the Attic".into()));
}

#[test]
fn links_come_from_href_or_from_the_strings_asked_for() {
    let doc = Document::parse(PAGE);
    let sel = Selector::css("a.link").unwrap();
    assert_eq!(links(&sel, doc.root()).unwrap(), ["/a", "b.html"]);
    let sel = Selector::css("a.link::attr(href)").unwrap();
    assert_eq!(links(&sel, doc.root()).unwrap(), ["/a", "b.html"]);
    let sel = Selector::xpath("//a[@class='link']").unwrap();
    assert_eq!(links(&sel, doc.root()).unwrap(), ["/a", "b.html"]);
    let sel = Selector::xpath("//a/@href").unwrap();
    assert_eq!(links(&sel, doc.root()).unwrap(), ["/a", "b.html"]);
}

#[test]
fn compiled_specs_are_shared_across_threads() {
    let spec = std::sync::Arc::new(ItemSpec::new(vec![css("t", "h1::text").strip(true)]));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let spec = spec.clone();
            std::thread::spawn(move || {
                let doc = Document::parse(PAGE);
                spec.extract(doc.root(), None).remove(0)
            })
        })
        .collect();
    for t in threads {
        assert_eq!(
            t.join().unwrap(),
            Value::Text("A Light in the Attic".into())
        );
    }
}

#[test]
fn a_query_that_fails_is_an_error_not_a_bad_value() {
    let f = Field::new("n", Selector::xpath("count(1)").unwrap());
    assert!(matches!(one(f), Value::Error(e) if e.contains("count")));
}

#[test]
fn markdown_convert_resolves_links_against_base() {
    let doc = Document::parse(PAGE);
    let base = url::Url::parse("https://books.example/catalogue/").unwrap();
    let spec = ItemSpec::new(vec![
        css("md", "a.link").convert(Convert::Markdown { main_only: false }),
        css("text", "table").convert(Convert::CleanText { main_only: false }),
        css("attr", "a::attr(href)").convert(Convert::Markdown { main_only: false }),
    ]);
    assert_eq!(
        spec.extract(doc.root(), Some(&base)),
        vec![
            Value::Text("[A](https://books.example/a)".into()),
            Value::Text("UPC\ta897fe39b1053632\nReviews\t0".into()),
            Value::Invalid("/a".into()),
        ]
    );
}
