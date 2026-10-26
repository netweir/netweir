//! Generic records: the page's main repeated region as rows with named
//! columns, each column with a selector that finds it in a row.

use netweir_dom::records::{Records, records};
use netweir_dom::{Document, Query};
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

/// Every selector finds what the rows say: the row selector the rows, a
/// column's selector its value in each row.
fn selectors_hold(html: &str, r: &Records) {
    let doc = Document::parse(html);
    let rows = Query::new(&r.selector).unwrap().select(doc.root());
    assert_eq!(rows.len(), r.rows.len(), "{}", r.selector);
    for c in &r.columns {
        let found = Query::new(&c.selector).unwrap().select(rows[0]);
        assert!(
            !found.is_empty(),
            "{} finds nothing in the first row",
            c.selector
        );
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
