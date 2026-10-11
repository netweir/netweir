//! XPath 1.0, checked against the examples in the W3C recommendation
//! (section 2.5, abbreviated syntax) and against how lxml answers the same
//! queries on HTML.

use netweir_dom::{Document, Hit, XPath, XPathError};

const PAGE: &str = r#"<html><head><title>Doc</title></head><body>
<div id="intro" class="section lead" lang="en">
  <p>One <b>bold</b> start.</p>
  <p class="note">Two</p>
  <!-- remark -->
  <p>Three</p>
</div>
<div id="chapters" class="section">
  <h2>Chapter 1</h2><para type="warning">careful</para><para>fine</para>
  <h2>Chapter 2</h2><para type="warning">again</para>
  <ul><li>a</li><li>b</li><li>c</li><li>d</li></ul>
</div>
<a href="/x" title="X">first</a><a href="/y">second</a>
<span lang="en-GB">colour</span>
</body></html>"#;

fn eval(query: &str) -> Vec<String> {
    let doc = Document::parse(PAGE);
    XPath::new(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
        .run(doc.root())
        .unwrap_or_else(|e| panic!("{query}: {e}"))
        .iter()
        .map(Hit::to_string_value)
        .collect()
}

fn one(query: &str) -> String {
    let all = eval(query);
    assert_eq!(all.len(), 1, "{query} gave {all:?}");
    all.into_iter().next().unwrap()
}

// --- location paths (spec 2.5) --------------------------------------------

#[test]
fn child_and_descendant_steps() {
    assert_eq!(
        eval("/html/body/div/p/text()"),
        ["Two", "Three"].map(|s| s.to_string()).iter().fold(
            vec!["One ".to_string(), " start.".to_string()],
            |mut v, s| {
                v.push(s.clone());
                v
            }
        )
    );
    assert_eq!(eval("//li/text()"), ["a", "b", "c", "d"]);
    assert_eq!(
        eval("//div[@id='chapters']//para/text()"),
        ["careful", "fine", "again"]
    );
    assert_eq!(eval("count(//p)"), ["3.0"]);
}

#[test]
fn positions_and_last() {
    assert_eq!(one("//li[1]/text()"), "a");
    assert_eq!(one("//li[last()]/text()"), "d");
    assert_eq!(one("//li[last()-1]/text()"), "c");
    assert_eq!(eval("//li[position()>2]/text()"), ["c", "d"]);
    // (//li)[1] is the first li in the document; //li[1] each list's first.
    assert_eq!(one("(//li)[2]/text()"), "b");
    assert_eq!(one("//ul/li[3]/text()"), "c");
}

#[test]
fn reverse_axes_count_backwards() {
    assert_eq!(one("//li[4]/preceding-sibling::li[1]/text()"), "c");
    assert_eq!(one("//li[4]/preceding-sibling::li[last()]/text()"), "a");
    assert_eq!(
        one("//b/ancestor::*[1]/@id | //b/ancestor::div[1]/@id"),
        "intro"
    );
    assert_eq!(eval("//li[2]/following-sibling::li/text()"), ["c", "d"]);
    assert_eq!(one("//b/ancestor::div/@id"), "intro");
}

#[test]
fn the_spec_examples() {
    assert_eq!(eval("//para[@type='warning']/text()"), ["careful", "again"]);
    assert_eq!(one("//para[@type='warning'][2]/text()"), "again");
    assert_eq!(
        eval("//para[2][@type='warning']/text()"),
        Vec::<String>::new()
    );
    assert_eq!(
        one("//div[@id='chapters']/h2[2]/following-sibling::para[1]/text()"),
        "again"
    );
    assert_eq!(eval("//div[h2]/@id"), ["chapters"]);
    assert_eq!(eval("//*[self::h2 or self::para][1]/text()"), ["Chapter 1"]);
    assert_eq!(
        one("//para[preceding-sibling::h2[1][. = 'Chapter 2']]/text()"),
        "again"
    );
}

#[test]
fn abbreviations() {
    assert_eq!(
        eval("//b/../@class"),
        Vec::<String>::new(),
        "no such attribute"
    );
    assert_eq!(eval("//b/.."), ["<p>One <b>bold</b> start.</p>"]);
    assert_eq!(eval("//p[@class]/text()"), ["Two"]);
    assert_eq!(eval("//a/@*"), ["/x", "X", "/y"]);
    assert_eq!(eval("//div/@id"), ["intro", "chapters"]);
}

#[test]
fn node_tests() {
    assert_eq!(eval("//div[1]/comment()"), ["<!-- remark -->"]);
    assert_eq!(one("count(//div[1]/node())"), "9.0");
    assert_eq!(one("count(//div[1]/*)"), "3.0");
    assert_eq!(one("name(//*[@title])"), "a");
    assert_eq!(one("local-name(//a/@title)"), "title");
}

#[test]
fn following_and_preceding() {
    assert_eq!(eval("//ul/following::a/text()"), ["first", "second"]);
    assert_eq!(one("count(//a[1]/preceding::li)"), "4.0");
    assert_eq!(one("//li[1]/preceding::h2[1]/text()"), "Chapter 2");
}

#[test]
fn results_are_in_document_order_without_duplicates() {
    assert_eq!(
        eval("//li[3] | //li[1] | //li[1]"),
        ["<li>a</li>", "<li>c</li>"]
    );
    assert_eq!(one("count(//li/.. | //ul)"), "1.0");
}

// --- functions ------------------------------------------------------------

#[test]
fn string_functions() {
    assert_eq!(one("concat('a', 'b', 'c')"), "abc");
    assert_eq!(one("string(//a[1]/@href)"), "/x");
    assert_eq!(one("normalize-space('  a \n  b  ')"), "a b");
    assert_eq!(one("normalize-space(//div[1]/p[1])"), "One bold start.");
    assert_eq!(one("substring('12345', 2, 3)"), "234");
    assert_eq!(one("substring('12345', 1.5, 2.6)"), "234");
    assert_eq!(one("substring('12345', 0, 3)"), "12");
    assert_eq!(one("substring('12345', 0 div 0, 3)"), "");
    assert_eq!(one("substring('12345', -42, 1 div 0)"), "12345");
    assert_eq!(one("substring-before('1999/04/01', '/')"), "1999");
    assert_eq!(one("substring-after('1999/04/01', '/')"), "04/01");
    assert_eq!(one("translate('bar', 'abc', 'ABC')"), "BAr");
    assert_eq!(one("translate('--aaa--', 'abc-', 'ABC')"), "AAA");
    assert_eq!(one("string-length('héllo')"), "5.0");
    assert_eq!(eval("//a[starts-with(@href, '/y')]/text()"), ["second"]);
    assert_eq!(eval("//p[contains(., 'bold')]/b/text()"), ["bold"]);
}

#[test]
fn number_and_boolean_functions() {
    assert_eq!(one("1 + 2 * 3"), "7.0");
    assert_eq!(one("7 mod 3"), "1.0");
    assert_eq!(one("-7 mod 3"), "-1.0");
    assert_eq!(one("1 div 0"), "inf");
    assert_eq!(one("0 div 0"), "nan");
    assert_eq!(one("round(2.5)"), "3.0");
    assert_eq!(one("round(-2.5)"), "-2.0");
    assert_eq!(one("floor(-1.5)"), "-2.0");
    assert_eq!(one("ceiling(1.2)"), "2.0");
    assert_eq!(one("number('  12.5 ')"), "12.5");
    assert_eq!(one("number('1e3')"), "nan");
    assert_eq!(one("sum(//nothing)"), "0.0");
    assert_eq!(one("boolean(//li)"), "1");
    assert_eq!(one("not(//li)"), "0");
    assert_eq!(one("true() and not(false())"), "1");
    assert_eq!(one("string(1 div 0)"), "Infinity");
    assert_eq!(one("string(3)"), "3");
    assert_eq!(one("string(0.5)"), "0.5");
    assert_eq!(one("string(-0)"), "0");
}

#[test]
fn comparisons_with_node_sets_are_existential() {
    assert_eq!(one("//li = 'c'"), "1");
    assert_eq!(one("//li != 'c'"), "1");
    assert_eq!(one("//li = 'z'"), "0");
    assert_eq!(
        one("count(//li[. > 'b'])"),
        "0.0",
        "strings compare as numbers: NaN"
    );
    assert_eq!(one("//nothing = //nothing"), "0");
    assert_eq!(one("'2' > 1"), "1");
}

#[test]
fn lang_and_id() {
    assert_eq!(
        eval("//p[lang('en')]/text()"),
        ["One ", " start.", "Two", "Three"]
    );
    assert_eq!(eval("//span[lang('en')]/text()"), ["colour"]);
    assert_eq!(eval("//*[lang('fr')]"), Vec::<String>::new());
    assert_eq!(eval("id('chapters intro')/@id"), ["intro", "chapters"]);
}

#[test]
fn extensions_parsel_users_expect() {
    assert_eq!(
        eval("//div[has-class('section')]/@id"),
        ["intro", "chapters"]
    );
    assert_eq!(eval("//div[has-class('section', 'lead')]/@id"), ["intro"]);
    assert_eq!(eval("//div[has-class('sect')]/@id"), Vec::<String>::new());
    assert_eq!(
        eval("//a[re:test(@href, '^/[xy]$')]/text()"),
        ["first", "second"]
    );
    assert_eq!(eval("//li[re:test(., 'B', 'i')]/text()"), ["b"]);
    assert_eq!(one("re:replace('a-b-c', '-', 'g', '+')"), "a+b+c");
    assert_eq!(one("re:replace('a-b-c', '-', '', '+')"), "a+b-c");
}

#[test]
fn variables() {
    let doc = Document::parse(PAGE);
    let q = XPath::new("//a[@href = $where]/text()").unwrap();
    let vars = [("where".to_string(), netweir_dom::XValue::Str("/y".into()))];
    let hits = q.run_with(doc.root(), &vars).unwrap();
    assert_eq!(
        hits.iter().map(Hit::to_string_value).collect::<Vec<_>>(),
        ["second"]
    );
    let err = q.run(doc.root()).unwrap_err();
    assert!(err.to_string().contains("$where"), "{err}");
}

// --- context and errors ---------------------------------------------------

#[test]
fn relative_queries_start_at_the_scope() {
    let doc = Document::parse(PAGE);
    let chapters = XPath::new("//div[@id='chapters']")
        .unwrap()
        .run(doc.root())
        .unwrap();
    let Hit::Node(chapters) = chapters[0].clone() else {
        panic!()
    };
    let q = |s: &str| -> Vec<String> {
        XPath::new(s)
            .unwrap()
            .run(chapters)
            .unwrap()
            .iter()
            .map(Hit::to_string_value)
            .collect()
    };
    assert_eq!(q("h2/text()"), ["Chapter 1", "Chapter 2"]);
    assert_eq!(q(".//li[1]/text()"), ["a"]);
    // A leading slash still means the document root, as in lxml.
    assert_eq!(q("count(//p)"), ["3.0"]);
    assert_eq!(q("@id"), ["chapters"]);
}

#[test]
fn bad_queries_are_errors() {
    for (bad, why) in [
        ("//", "ends where a step"),
        ("//li[", "ends where a step"),
        ("//li[1", "\"]\""),
        ("count(", "ends where a step"),
        ("1 +", "ends where a step"),
        ("'unclosed", "literal"),
        ("//li ==", "found \"=\""),
        ("//a[@x = $y]", "$y"),
        ("a b", "operator"),
        ("unknown-function()", "unknown-function"),
        ("count(1)", "node-set"),
        ("//svg:path", "prefix"),
        ("re:match('a', 'a')", "re:match"),
        ("re:test('a', '(')", "regular expression"),
        ("@", "ends where a step"),
        ("child::", "ends where a step"),
        ("//li[)]", "found \")\""),
        ("nonsense::a", "axis"),
    ] {
        let doc = Document::parse("<p>x</p>");
        let err: XPathError = match XPath::new(bad) {
            Err(e) => e,
            Ok(q) => q.run(doc.root()).expect_err(bad),
        };
        assert!(err.to_string().contains(why), "{bad:?}: {err}");
    }
}

#[test]
fn deep_documents_do_not_overflow_or_crawl() {
    let html = "<div>".repeat(20_000) + "x";
    let doc = Document::parse(&html);
    let start = std::time::Instant::now();
    assert_eq!(
        XPath::new("count(//div)").unwrap().run(doc.root()).unwrap()[0].to_string_value(),
        "20000.0"
    );
    assert_eq!(
        XPath::new("//div[not(div)]/text()")
            .unwrap()
            .run(doc.root())
            .unwrap()[0]
            .to_string_value(),
        "x"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

// --- the fast paths must give the spec's answers ------------------------

#[test]
fn positions_count_siblings_even_when_lists_nest() {
    let doc = Document::parse(
        "<ul><li>a<ul><li>x</li><li>y</li><li>z</li></ul></li><li>b</li></ul><li>q</li>",
    );
    let q = |s: &str| -> Vec<String> {
        XPath::new(s)
            .unwrap()
            .run(doc.root())
            .unwrap()
            .iter()
            .map(Hit::to_string_value)
            .collect()
    };
    // Each list's second li, in document order: y (inner), then b (outer).
    assert_eq!(q("//li[2]/text()"), ["y", "b"]);
    assert_eq!(q("//li[last()]/text()"), ["z", "b", "q"]);
    assert_eq!(q("//li[position() = 1]/text()"), ["a", "x", "q"]);
    assert_eq!(q("//li[1][2]"), Vec::<String>::new());
    assert_eq!(q("//ul/li[1]/text()"), ["a", "x"]);
    assert_eq!(q("count(//li[3])"), ["1.0"]);
    // Equivalent long form, which takes the general path.
    assert_eq!(
        q("/descendant-or-self::node()/child::li[2]/text()"),
        q("//li[2]/text()")
    );
}

#[test]
fn attribute_predicates_take_the_short_way_correctly() {
    let doc = Document::parse(r#"<a x="1">a</a><a x="2" y="">b</a><a>c</a><a X="1">d</a>"#);
    let q = |s: &str| -> Vec<String> {
        XPath::new(s)
            .unwrap()
            .run(doc.root())
            .unwrap()
            .iter()
            .map(Hit::to_string_value)
            .collect()
    };
    assert_eq!(
        q("//a[@x='1']/text()"),
        ["a", "d"],
        "HTML attribute names are lowercased"
    );
    assert_eq!(q("//a['2'=@x]/text()"), ["b"]);
    assert_eq!(q("//a[@y]/text()"), ["b"], "present but empty still counts");
    assert_eq!(q("//a[@x]/text()"), ["a", "b", "d"]);
    assert_eq!(q("//a[@missing='']/text()"), Vec::<String>::new());
    assert_eq!(q("//a[@x != '1']/text()"), ["b"]);
    assert_eq!(q("//a[not(@x)]/text()"), ["c"]);
}

#[test]
fn xpath_2_steps_are_rejected_with_a_readable_message() {
    let err = XPath::new("//p/name()").unwrap_err();
    assert!(err.to_string().contains("unexpected \"(\""), "{err}");
}

fn on(html: &str, query: &str) -> Vec<String> {
    let doc = Document::parse(html);
    XPath::new(query)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
        .run(doc.root())
        .unwrap_or_else(|e| panic!("{query}: {e}"))
        .iter()
        .map(Hit::to_string_value)
        .collect()
}

#[test]
fn nested_context_nodes_count_each_child_once() {
    let divs = "<div id=o><div id=i><p>a</p><p>b</p><p>c</p></div></div>";
    assert_eq!(on(divs, "//div//p[position() > 1]/text()"), ["b", "c"]);
    assert_eq!(on(divs, "//div//p[4]/text()"), Vec::<String>::new());
    assert_eq!(on(divs, "//div//p[last() = 3]/text()"), ["a", "b", "c"]);
    assert_eq!(
        on(
            divs,
            "//div/descendant-or-self::node()/child::p[position() > 1]/text()"
        ),
        ["b", "c"]
    );
    let lists = "<ul><li>1<ul><li>x</li></ul></li><li>2</li><li>3</li><li>4</li></ul>";
    assert_eq!(on(lists, "//ul//li[position() > 2]/text()"), ["3", "4"]);
    let mixed = "<div><ul><li>a</li><li>b</li><li>c</li></ul></div><div><ul><li>x</li></ul></div>";
    assert_eq!(on(mixed, "(//div | //div/ul)//li[3]/text()"), ["c"]);
}

#[test]
fn deeply_nested_expressions_are_errors_not_crashes() {
    let deep = [
        "(".repeat(3000) + "1" + &")".repeat(3000),
        "not(".repeat(10_000) + "1" + &")".repeat(10_000),
        "//p[".repeat(5000) + "1" + &"]".repeat(5000),
        "-".repeat(50_000) + "1",
        vec!["//p"; 30_000].join(" | "),
        vec!["1"; 100_000].join(" + "),
    ];
    for q in deep {
        let err = XPath::new(&q).unwrap_err();
        assert!(err.to_string().contains("too"), "{err}");
    }
    // Reasonable depth still works.
    assert_eq!(
        on("<p>", &("(".repeat(100) + "1" + &")".repeat(100))),
        ["1.0"]
    );
    assert_eq!(on("<p>", &vec!["1"; 200].join(" + ")), ["200.0"]);
}

#[test]
fn attribute_names_with_capitals_match_nothing_on_both_paths() {
    let html = r#"<p id="a">x</p>"#;
    assert_eq!(on(html, "//p[@ID]/@id"), Vec::<String>::new());
    assert_eq!(on(html, "//p[@ID='a']/@id"), Vec::<String>::new());
    assert_eq!(on(html, "//p/@ID"), Vec::<String>::new());
}

#[test]
fn first_on_an_axis_from_many_nodes_is_linear() {
    let html = "<ul>".to_string() + &"<li>x</li>".repeat(10_000) + "</ul>";
    let doc = Document::parse(&html);
    let start = std::time::Instant::now();
    for (q, n) in [
        ("//li/following-sibling::li[1]", 9_999),
        ("//li/preceding-sibling::li[1]", 9_999),
        ("//li/following-sibling::li[2]", 9_998),
    ] {
        assert_eq!(
            XPath::new(q).unwrap().run(doc.root()).unwrap().len(),
            n,
            "{q}"
        );
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    let dl = "<dl><dt>A</dt><dd>1</dd><dd>2</dd><dt>B</dt><dd>3</dd></dl>";
    assert_eq!(on(dl, "//dt[.='A']/following-sibling::dd[1]/text()"), ["1"]);
    assert_eq!(on(dl, "//dd[.='3']/preceding-sibling::*[1]/text()"), ["B"]);
    assert_eq!(on(dl, "//dd[.='3']/preceding-sibling::dd[2]/text()"), ["1"]);
    assert_eq!(
        on(dl, "//dt/following-sibling::dd[1][.='2']"),
        Vec::<String>::new()
    );
}

#[test]
fn round_is_exact_at_the_edges() {
    assert_eq!(on("<p>", "round(0.49999999999999994)"), ["0.0"]);
    assert_eq!(on("<p>", "round(4503599627370497)"), ["4503599627370497.0"]);
    assert_eq!(on("<p>", "round(2.5)"), ["3.0"]);
    assert_eq!(on("<p>", "round(-2.5)"), ["-2.0"]);
}

#[test]
fn re_replace_takes_backslash_groups_like_lxml() {
    assert_eq!(
        on("<p>", r"re:replace('a-b', '(\w)-(\w)', 'g', '\2-\1')"),
        ["b-a"]
    );
    assert_eq!(
        on("<p>", r"re:replace('a-b', '(\w)-(\w)', 'g', '$1')"),
        ["$1"]
    );
}

#[test]
fn id_finds_every_token_in_document_order() {
    let html = r#"<p id="a">1</p><p id="b">2</p><p id="c">3</p>"#;
    assert_eq!(on(html, "id('c a')/text()"), ["1", "3"]);
    assert_eq!(on(html, "id('a a')/text()"), ["1"]);
    assert_eq!(on(html, "id(//p[2]/@id)/text()"), ["2"]);
    assert_eq!(on(html, "id('zz')"), Vec::<String>::new());
}

/// Runs `query` on another thread and fails if it takes over ten seconds,
/// so a regression shows up as a failure instead of a hung test run.
fn finishes_quickly(query: &str) {
    let query = query.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let doc = Document::parse(PAGE);
        let result = XPath::new(&query).and_then(|x| x.run(doc.root()));
        let _ = tx.send(result.map(|hits| hits.len()));
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the query was still running after ten seconds")
        .expect("the query should parse and run");
}

#[test]
fn nested_predicates_test_each_node_once() {
    // A predicate nested in another tests every node again for each node the
    // outer one tests, so the work grew as nodes^depth: the fuzzer found an
    // input that ran for half an hour.
    let nest = |step: &str| (0..12).fold(".".to_string(), |inner, _| format!("{step}[{inner}]"));
    finishes_quickly(&nest("//*"));
    finishes_quickly(&format!("//*[{}]", nest("..//*")));
    // The parts of a predicate that don't read the position are kept even
    // when another part does.
    let mixed = |shape: &str| {
        let nested = (0..12).fold("1".to_string(), |inner, _| {
            shape.replace("INNER", &format!("..//*[{inner}]"))
        });
        format!("//*[{nested}]")
    };
    finishes_quickly(&mixed("position() > 0 and INNER"));
    finishes_quickly(&mixed("count(INNER) >= position()"));
    finishes_quickly(&mixed("not(INNER) or position() = last()"));
    finishes_quickly(include_str!(
        "fixtures/xpath/nested-absolute-predicates.txt"
    ));
}

#[test]
fn a_kept_verdict_still_counts_positions() {
    // The number a predicate gives is kept per node, but which nodes it
    // keeps depends on their position in each set it filters.
    assert_eq!(eval("//li[count(..//li) - 2]/text()"), ["b"]);
    assert_eq!(eval("//li[count(//li) - 1][. = //li]/text()"), ["c"]);
    assert_eq!(eval("//div[.//p[2]][//li]/@id"), ["intro"]);
    assert_eq!(
        eval("//p[../@id = //div[2]/@id or . = //p[last()]]/text()"),
        ["Three"]
    );
}
