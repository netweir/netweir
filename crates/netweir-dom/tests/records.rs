//! Generic records: the page's main repeated region as rows with named
//! columns, each column with a selector that finds it in a row.

use netweir_dom::records::{Records, records};
use netweir_dom::{Document, Hit, Query};
use serde_json::{Value, json};

const BASE: &str = "https://records.example/search?q=x";

fn read(html: &str) -> Option<Records> {
    let doc = Document::parse(html);
    let base = url::Url::parse(BASE).unwrap();
    records(doc.root(), Some(&base))
}

fn names(r: &Records) -> Vec<&str> {
    r.columns.iter().map(|c| c.name.as_str()).collect()
}

fn column<'r>(r: &'r Records, name: &str) -> Vec<&'r Value> {
    r.rows.iter().map(|row| &row[name]).collect()
}

/// Every selector finds what the rows say: the row selector the rows, in
/// order, and a column's selector its value in every row that has one.
fn selectors_hold(html: &str, r: &Records) {
    let doc = Document::parse(html);
    let base = url::Url::parse(BASE).unwrap();
    let rows = Query::new(&r.selector).unwrap().select(doc.root());
    assert_eq!(rows.len(), r.rows.len(), "{}", r.selector);
    for (row, values) in rows.iter().zip(&r.rows) {
        for c in &r.columns {
            let Value::String(want) = &values[&c.name] else {
                continue;
            };
            let hits = Query::new(&c.selector).unwrap().run(*row);
            let got = match hits.first() {
                // A lazy image's placeholder, where most rows' images
                // load straight away.
                Some(Hit::Value(v)) if v.starts_with("data:") => continue,
                Some(Hit::Value(v)) => base.join(v).unwrap().to_string(),
                Some(Hit::Node(n)) => n.text().split_whitespace().collect::<Vec<_>>().join(" "),
                None => panic!("{} finds nothing where the row has {want:?}", c.selector),
            };
            assert!(
                got.ends_with(want.as_str()),
                "{}: {got:?} for {want:?}",
                c.selector
            );
        }
    }
}

const TABLE: &str = r#"<html><body>
<nav><ul><li><a href="/">Home</a></li><li><a href="/a">About</a></li>
<li><a href="/b">Search</a></li><li><a href="/c">Contact</a></li></ul></nav>
<table class="results">
  <thead><tr><th>Name</th><th>Licence no.</th><th>Issued</th></tr></thead>
  <tbody>
    <tr><td><a href="/l/1">Ada Byron</a></td><td>L-001</td><td>2024-01-03</td></tr>
    <tr><td><a href="/l/2">Alan Turing</a></td><td>L-002</td><td>2024-02-11</td></tr>
    <tr><td><a href="/l/3">Grace Hopper</a></td><td>L-003</td><td>2024-03-09</td></tr>
  </tbody>
</table></body></html>"#;

#[test]
fn a_table_reads_by_its_header() {
    let r = read(TABLE).unwrap();
    assert_eq!(names(&r), ["name", "name_url", "licence_no", "issued"]);
    assert_eq!(
        column(&r, "name"),
        ["Ada Byron", "Alan Turing", "Grace Hopper"]
    );
    assert_eq!(column(&r, "name_url")[0], "https://records.example/l/1");
    assert_eq!(column(&r, "licence_no")[2], "L-003");
    selectors_hold(TABLE, &r);
}

#[test]
fn a_table_without_a_head_takes_a_first_row_of_th() {
    let html = r#"<table>
      <tr><th>Town</th><th>Population</th></tr>
      <tr><td>Ayr</td><td>46,000</td></tr>
      <tr><td>Troon</td><td>15,000</td></tr>
      <tr><td>Irvine</td><td>34,000</td></tr></table>"#;
    let r = read(html).unwrap();
    assert_eq!(names(&r), ["town", "population"]);
    assert_eq!(r.rows.len(), 3);
    selectors_hold(html, &r);
}

#[test]
fn a_headless_table_numbers_its_columns() {
    let html = r#"<table>
      <tr><td>Ayr</td><td>46,000</td></tr>
      <tr><td>Troon</td><td>15,000</td></tr>
      <tr><td>Irvine</td><td>34,000</td></tr></table>"#;
    let r = read(html).unwrap();
    assert_eq!(names(&r), ["column_1", "column_2"]);
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn colspans_keep_cells_under_their_header() {
    let html = r#"<table>
      <tr><th>A</th><th>B</th><th>C</th></tr>
      <tr><td colspan="2">wide</td><td>c1</td></tr>
      <tr><td>a2</td><td>b2</td><td>c2</td></tr>
      <tr><td>a3</td><td>b3</td><td>c3</td></tr></table>"#;
    let r = read(html).unwrap();
    assert_eq!(column(&r, "c"), ["c1", "c2", "c3"]);
    assert_eq!(column(&r, "b")[0], &Value::Null);
}

const CARDS: &str = r#"<html><body><main><ul class="hits">
  <li class="hit"><a class="title" href="/b/1">The Hobbit</a>
    <span class="author">J. R. R. Tolkien</span><span class="year">1937</span>
    <img src="/i/1.jpg" alt=""></li>
  <li class="hit"><a class="title" href="/b/2">Dune</a>
    <span class="author">Frank Herbert</span><span class="year">1965</span>
    <img data-src="/i/2.jpg" src="data:image/gif;base64,R0lGOD"></li>
  <li class="hit"><a class="title" href="/b/3">Emma</a>
    <span class="author">Jane Austen</span>
    <img src="/i/3.jpg"></li>
  <li class="hit"><a class="title" href="/b/4">Beloved</a>
    <span class="author">Toni Morrison</span><span class="year">1987</span>
    <img src="/i/4.jpg"></li>
</ul></main></body></html>"#;

#[test]
fn cards_take_their_names_from_classes() {
    let r = read(CARDS).unwrap();
    assert_eq!(names(&r), ["title", "title_url", "author", "year", "image"]);
    assert_eq!(
        column(&r, "title"),
        ["The Hobbit", "Dune", "Emma", "Beloved"]
    );
    assert_eq!(column(&r, "title_url")[1], "https://records.example/b/2");
    // A missing field is null; a lazy image reads its real source.
    assert_eq!(
        column(&r, "year"),
        [&json!("1937"), &json!("1965"), &Value::Null, &json!("1987")]
    );
    assert_eq!(column(&r, "image")[1], "https://records.example/i/2.jpg");
    selectors_hold(CARDS, &r);
}

#[test]
fn labels_name_the_value_beside_them() {
    let html = r#"<div class="list">
      <div class="entry"><h3>Ayr Library</h3><dl><dt>Phone</dt><dd>01292 286385</dd>
        <dt>Open</dt><dd>Mon-Sat</dd></dl></div>
      <div class="entry"><h3>Troon Library</h3><dl><dt>Phone</dt><dd>01292 315352</dd>
        <dt>Open</dt><dd>Tue-Sat</dd></dl></div>
      <div class="entry"><h3>Irvine Library</h3><dl><dt>Phone</dt><dd>01294 311992</dd>
        <dt>Open</dt><dd>Mon-Fri</dd></dl></div></div>"#;
    let r = read(html).unwrap();
    assert_eq!(names(&r), ["heading", "phone", "open"]);
    assert_eq!(column(&r, "phone")[1], "01292 315352");
    selectors_hold(html, &r);
}

#[test]
fn a_shared_prefix_names_the_column_and_leaves_the_value() {
    let html = r#"<section>
      <article><h2>Plot 4</h2><p>Area: 120 m²</p><p>Status: let</p></article>
      <article><h2>Plot 9</h2><p>Area: 95 m²</p><p>Status: vacant</p></article>
      <article><h2>Plot 12</h2><p>Area: 140 m²</p><p>Status: let</p></article></section>"#;
    let r = read(html).unwrap();
    assert_eq!(names(&r), ["heading", "area", "status"]);
    assert_eq!(column(&r, "area"), ["120 m²", "95 m²", "140 m²"]);
}

#[test]
fn a_menu_is_not_the_records() {
    // Thirty links in a nav, and a short list of real results.
    let links: String = (0..30)
        .map(|i| format!(r#"<li class="m"><a href="/s/{i}">Section {i}</a></li>"#))
        .collect();
    let html = format!(
        r#"<div class="menu"><ul>{links}</ul></div>
        <div class="r"><div class="row"><b class="who">Ann</b><i class="when">May</i></div>
        <div class="row"><b class="who">Bob</b><i class="when">June</i></div>
        <div class="row"><b class="who">Cy</b><i class="when">July</i></div></div>"#
    );
    let r = read(&html).unwrap();
    assert_eq!(names(&r), ["who", "when"]);
}

#[test]
fn navigation_and_footers_are_passed_over() {
    let html = r#"<footer><table>
      <tr><td><a href="/1">Privacy</a></td><td>Legal</td></tr>
      <tr><td><a href="/2">Terms</a></td><td>Legal</td></tr>
      <tr><td><a href="/3">Cookies</a></td><td>Legal</td></tr></table></footer>
      <article><p>One long story and nothing repeated.</p></article>"#;
    assert!(read(html).is_none());
}

#[test]
fn prose_has_no_records() {
    let html = "<article><h1>Title</h1><p>One.</p><p>Two.</p><p>Three.</p><p>Four.</p></article>";
    assert!(read(html).is_none());
}

#[test]
fn a_layout_table_isnt_the_records_but_what_it_holds_is() {
    let html = r#"<table><tr><td><table>
      <tr><td>Ayr</td><td>46,000</td></tr>
      <tr><td>Troon</td><td>15,000</td></tr>
      <tr><td>Irvine</td><td>34,000</td></tr></table></td></tr></table>"#;
    let r = read(html).unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(column(&r, "column_1"), ["Ayr", "Troon", "Irvine"]);
}

#[test]
fn the_biggest_region_wins() {
    let small: String = (0..3)
        .map(|i| format!(r#"<li class="x"><b class="k">k{i}</b><i class="v">v{i}</i></li>"#))
        .collect();
    let big: String = (0..20)
        .map(|i| format!("<tr><td>a{i}</td><td>b{i}</td><td>c{i}</td></tr>"))
        .collect();
    let html = format!("<div><ul>{small}</ul></div><table>{big}</table>");
    let r = read(&html).unwrap();
    assert_eq!(r.rows.len(), 20);
}

#[test]
fn many_rows_read_in_time() {
    let rows: String = (0..20_000)
        .map(|i| format!(r#"<div class="r"><a class="n" href="/{i}">n{i}</a><span class="p">{i}</span></div>"#))
        .collect();
    let html = format!("<main>{rows}</main>");
    let start = std::time::Instant::now();
    let r = read(&html).unwrap();
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    assert_eq!(r.rows.len(), 5000);
}

#[test]
fn deep_nesting_reads_in_time() {
    let html = format!(
        "{}x{}",
        "<div><div class=a>".repeat(3000),
        "</div></div>".repeat(3000)
    );
    let start = std::time::Instant::now();
    let _ = read(&html);
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
}

#[test]
fn rows_unlike_each_other_read_in_time() {
    // Every row has fields under its own classes: thousands of columns
    // that line up with nothing.
    let rows: String = (0..5000)
        .map(|i| {
            // Letters only: a class with digits in it reads as generated.
            let tag: String = format!("{i:04}").bytes().map(|d| (b'a' + d - b'0') as char).collect();
            format!(
                r#"<div class="r"><b class="k">k</b><i class="v">v</i><span class="u{tag}">a</span><span class="w{tag}">b</span></div>"#
            )
        })
        .collect();
    let html = format!("<main>{rows}</main>");
    let start = std::time::Instant::now();
    let r = read(&html).unwrap();
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    assert_eq!(names(&r), ["k", "v"]);
}

#[test]
fn nested_tables_read_in_time() {
    let mut html = String::from("<td>x</td><td>y</td>");
    for _ in 0..300 {
        html = format!(
            "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr><tr>{html}</tr></table>"
        );
        html = format!("<td>{html}</td><td>z</td>");
    }
    let html = format!("<table><tr>{html}</tr></table>");
    let start = std::time::Instant::now();
    let _ = read(&html);
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
}

#[test]
fn many_small_groups_read_in_time() {
    let groups: String = (0..5000)
        .map(|i| format!(r#"<ul class="g{i}"><li><b>a</b><i>b</i></li><li><b>c</b><i>d</i></li><li><b>e</b><i>f</i></li></ul>"#))
        .collect();
    let start = std::time::Instant::now();
    let r = read(&format!("<main>{groups}</main>"));
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    assert!(r.is_some());
}

#[test]
fn siblings_that_differ_by_a_class_are_one_run() {
    let cards: String = (0..9)
        .map(|i| {
            let sale = if i % 3 == 0 { " card--sale" } else { "" };
            format!(
                r#"<li class="card{sale}"><b class="name">P {i}</b><i class="price">£{i}</i></li>"#
            )
        })
        .collect();
    let html = format!("<ul>{cards}</ul>");
    let r = read(&html).unwrap();
    assert_eq!(r.rows.len(), 9);
    assert_eq!(column(&r, "name")[0], "P 0");
    selectors_hold(&html, &r);
    let rows: String = (0..8)
        .map(|i| {
            let parity = if i % 2 == 0 { "odd" } else { "even" };
            format!(r#"<div class="item {parity}"><b>N{i}</b><i>{i}</i></div>"#)
        })
        .collect();
    let html = format!("<main>{rows}</main>");
    let r = read(&html).unwrap();
    assert_eq!(r.rows.len(), 8);
    selectors_hold(&html, &r);
}

#[test]
fn equal_regions_go_to_the_first() {
    let list = |p: &str| -> String {
        (0..4)
            .map(|i| format!(r#"<li class="{p}"><b>{p}{i}</b><i>x{i}</i></li>"#))
            .collect()
    };
    let html = format!("<ul>{}</ul><ul>{}</ul>", list("a"), list("b"));
    for _ in 0..10 {
        assert_eq!(column(&read(&html).unwrap(), "text")[0], "a0");
    }
}

#[test]
fn rowspans_fill_the_rows_below() {
    let html = r#"<table>
      <tr><th>Year</th><th>Title</th><th>Role</th></tr>
      <tr><th rowspan="2">2001</th><td>Film A</td><td>Ann</td></tr>
      <tr><td>Film B</td><td>Bea</td></tr>
      <tr><th>2003</th><td>Film C</td><td>Cy</td></tr></table>"#;
    let r = read(html).unwrap();
    assert_eq!(column(&r, "year"), ["2001", "2001", "2003"]);
    assert_eq!(column(&r, "title"), ["Film A", "Film B", "Film C"]);
    assert_eq!(column(&r, "role"), ["Ann", "Bea", "Cy"]);
}

#[test]
fn table_selectors_agree_with_the_values() {
    let html = r#"<table>
      <tr><th>A</th><th>B</th><th>C</th></tr>
      <tr><td colspan="2">wide</td><td>c1</td></tr>
      <tr><td>a2</td><td>b2</td><td>c2</td></tr>
      <tr><td>a3</td><td>b3</td><td>c3</td></tr></table>"#;
    let r = read(html).unwrap();
    let c = r.columns.iter().find(|c| c.name == "c").unwrap();
    assert_eq!(c.selector, "td:nth-child(3)");
    let html = r##"<table>
      <tr><td><a href="#top">x</a> <a href="/p/1">One</a></td><td><img src="data:image/gif;base64,R0" data-src="/i/1.jpg"></td><td>1</td></tr>
      <tr><td><a href="#top">x</a> <a href="/p/2">Two</a></td><td><img src="data:image/gif;base64,R0" data-src="/i/2.jpg"></td><td>2</td></tr>
      <tr><td><a href="#top">x</a> <a href="/p/3">Three</a></td><td><img src="data:image/gif;base64,R0" data-src="/i/3.jpg"></td><td>3</td></tr></table>"##;
    let r = read(html).unwrap();
    assert_eq!(column(&r, "column_1_url")[0], "https://records.example/p/1");
    assert_eq!(
        column(&r, "column_2_image")[0],
        "https://records.example/i/1.jpg"
    );
    selectors_hold(html, &r);
}

#[test]
fn an_optional_element_doesnt_shift_the_rest() {
    let rows: String = (0..6)
        .map(|i| {
            let sub = if i > 0 { "<span>Sub</span>" } else { "" };
            format!(r#"<div class="p"><h3>Item {i}</h3>{sub}<span>£{i}.99</span></div>"#)
        })
        .collect();
    let html = format!("<main>{rows}</main>");
    let r = read(&html).unwrap();
    assert_eq!(names(&r), ["heading", "text", "text_2"]);
    assert_eq!(
        column(&r, "text_2"),
        ["£0.99", "£1.99", "£2.99", "£3.99", "£4.99", "£5.99"]
    );
    assert_eq!(column(&r, "text")[0], &Value::Null);
}

#[test]
fn a_sidebar_of_categories_loses_to_the_main_list() {
    let cats: String = (0..50)
        .map(|i| format!(r#"<li class="cat"><a href="/c/{i}">Category {i}</a> <span class="n">({i})</span></li>"#))
        .collect();
    let repos: String = (0..10)
        .map(|i| {
            format!(
                r#"<li class="repo"><h3><a href="/r/{i}">repo{i}</a></h3><p class="d">Tool number {i}</p>
                <span class="lang">Rust</span><span class="stars">{i}0</span><relative-time>May {i}</relative-time></li>"#
            )
        })
        .collect();
    let html = format!(r#"<div class="side"><ul>{cats}</ul></div><main><ul>{repos}</ul></main>"#);
    let r = read(&html).unwrap();
    assert_eq!(r.rows.len(), 10);
}

#[test]
fn rows_with_their_own_classes_read_in_time() {
    let rows: String = (0..5000)
        .map(|i| {
            let tag: String = format!("{i:04}")
                .bytes()
                .map(|d| (b'a' + d - b'0') as char)
                .collect();
            let own: String = (0..12)
                .map(|k| format!(r#"<span class="x{k}{tag}">s</span>"#))
                .collect();
            format!(r#"<div class="r"><b class="k">k</b><i class="v">v</i>{own}</div>"#)
        })
        .collect();
    let html = format!("<main>{rows}</main>");
    let start = std::time::Instant::now();
    let r = read(&html).unwrap();
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    assert_eq!(&names(&r)[..2], ["k", "v"]);
}

#[test]
fn deep_rows_read_in_time() {
    // Each level holds three look-alike blocks, the first holding the
    // next level.
    let mut html = String::from("x");
    for i in 0..1000 {
        html = format!(
            r#"<div class="a">{html}<b>t{i}</b></div><div class="a"><b>u</b><i>v</i></div><div class="a"><b>w</b><i>y</i></div>"#
        );
    }
    let start = std::time::Instant::now();
    let r = read(&html);
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    assert!(r.is_none_or(|r| r.columns.len() < 50));
}

#[test]
fn urls_times_and_badges_are_not_labels() {
    let rows: String = (0..4)
        .map(|i| {
            format!(
                r#"<div class="g"><h3>Site {i}</h3><cite>https://site{i}.com › path</cite>
                <span class="when">10:{i}0 AM</span><span class="label label-success">New</span></div>"#
            )
        })
        .collect();
    let r = read(&format!("<main>{rows}</main>")).unwrap();
    assert_eq!(names(&r), ["heading", "text", "when", "label"]);
    assert_eq!(column(&r, "text")[1], "https://site1.com › path");
    assert_eq!(column(&r, "when")[2], "10:20 AM");
}

#[test]
fn a_row_class_comes_off_only_before_a_separator() {
    let rows: String = (0..3)
        .map(|i| format!(r#"<div class="post"><span class="poster">A{i}</span><span class="post-date">D{i}</span></div>"#))
        .collect();
    let r = read(&format!("<main>{rows}</main>")).unwrap();
    assert_eq!(names(&r), ["poster", "date"]);
}

#[test]
fn odd_tags_are_escaped_in_the_row_selector() {
    let rows: String = (0..3)
        .map(|i| format!("<x:item><b>a{i}</b><i>b{i}</i></x:item>"))
        .collect();
    let html = format!("<div>{rows}</div>");
    let r = read(&html).unwrap();
    selectors_hold(&html, &r);
}
