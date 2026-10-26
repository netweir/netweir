//! Lists and threads on the markup real sites use, and on pages that look
//! like lists or threads without being either.

use netweir_dom::Document;
use netweir_dom::auto::{Extracted, Schema, extract, page_type};
use netweir_dom::date::Civil;
use serde_json::Value;

const URL: &str = "https://site.example/x";

fn read(schema: Schema, html: &str) -> Extracted {
    let doc = Document::parse(html);
    let url = url::Url::parse(URL).unwrap();
    extract(
        schema,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 25),
    )
}

fn typed(html: &str) -> Option<Schema> {
    let doc = Document::parse(html);
    let url = url::Url::parse(URL).unwrap();
    page_type(doc.root(), Some(&url), None, Civil::date(2026, 10, 25)).0
}

fn get<'e>(item: &'e Extracted, field: &str) -> &'e Value {
    &item.fields.iter().find(|(f, _)| *f == field).unwrap().1
}

fn texts(item: &Extracted) -> Vec<String> {
    get(item, "posts")
        .as_array()
        .map(|posts| {
            posts
                .iter()
                .map(|p| p["text"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

const BODY: &str =
    "Swapped the dimmer at the weekend and the flicker is gone, even at the lowest setting.";

#[test]
fn a_xenforo_thread_is_a_thread() {
    let post = |n: u32| {
        format!(
            r#"<article class="message message--post"><div class="message-inner">
            <div class="message-cell message-cell--user"><h4 class="message-name"><a href="/members/u{n}">user{n}</a></h4></div>
            <div class="message-cell message-cell--main"><div class="message-main"><header class="message-attribution"><ul><li>
            <a href="/posts/{n}"><time datetime="2026-10-2{n}T10:00:00Z">Oct 2{n}</time></a></li></ul></header>
            <div class="message-content"><article class="message-body"><div class="bbWrapper">Reply {n}. {BODY}</div></article></div>
            </div></div></div></article>"#
        )
    };
    let html = format!(
        r#"<main><h1>Flicker</h1><div class="block-body">{}{}{}{}</div></main>"#,
        post(1),
        post(2),
        post(3),
        post(4)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
    let thread = read(Schema::ForumThread, &html);
    assert_eq!(texts(&thread).len(), 4);
    assert!(texts(&thread)[0].starts_with("Reply 1."));
    assert!(read(Schema::ArticleList, &html).probability < 0.4);
}

#[test]
fn a_phpbb_thread_is_a_thread() {
    let post = |n: u32| {
        format!(
            r#"<div class="post bg{}"><div class="postbody"><p class="author">by <a class="username" href="/memberlist.php?u={n}">kit{n}</a> » <time datetime="2026-10-2{n}T10:00:00Z">Mon Oct 2{n}</time></p>
            <div class="content">Post {n}. {BODY}</div></div></div>"#,
            n % 2 + 1
        )
    };
    let html = format!(
        "<main><h1>Flicker</h1><div class=\"topic\">{}{}{}{}</div></main>",
        post(1),
        post(2),
        post(3),
        post(4)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 4);
}

#[test]
fn a_reply_quoting_a_dated_post_is_still_read() {
    let post = |n: u32, quote: bool| {
        let quoted = if quote {
            r#"<blockquote><div class="quote-head">kit said on <time datetime="2026-10-20">Oct 20</time></div>Is it the bulb?</blockquote>"#
        } else {
            ""
        };
        format!(
            r#"<div class="post"><div class="meta"><span class="author">u{n}</span> <time datetime="2026-10-2{n}">Oct 2{n}</time></div>
            <div class="body">{quoted}<p>Post {n}. {BODY}</p></div></div>"#
        )
    };
    let html = format!(
        "<main><h1>Flicker</h1><div>{}{}{}{}</div></main>",
        post(1, false),
        post(2, true),
        post(3, false),
        post(4, true)
    );
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 4);
}

#[test]
fn dated_lists_without_authors_arent_threads() {
    let teaser = |n: u32| {
        format!(
            r#"<div class="item"><a class="title" href="/b/{n}">A post about lamps, number {n}</a>
            <div class="date"><time datetime="2026-10-0{n}">Oct {n}</time></div><p class="excerpt">{BODY}</p></div>"#
        )
    };
    let blog = format!(
        "<main><h1>Blog</h1>{}{}{}{}</main>",
        teaser(1),
        teaser(2),
        teaser(3),
        teaser(4)
    );
    let release = |v: u32| {
        format!(
            r#"<section class="release"><h2>v1.{v}.0</h2><time datetime="2026-0{v}-01">1 {v} 2026</time><ul><li>{BODY}</li></ul></section>"#
        )
    };
    let changelog = format!(
        "<main><h1>Changes</h1>{}{}{}</main>",
        release(1),
        release(2),
        release(3)
    );
    for html in [blog, changelog] {
        assert!(read(Schema::ForumThread, &html).probability < 0.4, "{html}");
        assert_ne!(typed(&html), Some(Schema::ForumThread), "{html}");
    }
}

fn story(extra: &str) -> String {
    format!(
        r#"<html><head>{extra}</head><body><main><article><h1>The night market returns</h1>
        <p>After two years away the night market is back on the quay, with forty stalls and a band every Friday.</p>
        <p>Traders say the first night drew more people than any before the break, and the lights stay up until midnight.</p>
        </article></main></body></html>"#
    )
}

#[test]
fn one_post_in_a_blog_wrapper_is_an_article() {
    let html = story(
        r#"<script type="application/ld+json">{"@type": "Blog", "blogPost": {"@type": "BlogPosting",
        "headline": "The night market returns", "url": "https://site.example/x", "datePublished": "2026-10-20"}}</script>"#,
    );
    assert_eq!(typed(&html), Some(Schema::Article));
    assert!(read(Schema::ArticleList, &html).probability < 0.4);
}

#[test]
fn an_articles_microdata_teasers_arent_a_list() {
    let teasers: String = (1..=3)
        .map(|n| {
            format!(
                r#"<div itemscope itemtype="https://schema.org/NewsArticle"><a itemprop="url" href="/s/{n}"><span itemprop="headline">Related {n}</span></a></div>"#
            )
        })
        .collect();
    let html = story(&format!(
        r#"<script type="application/ld+json">{{"@type": "NewsArticle", "headline": "The night market returns",
        "url": "https://site.example/x", "datePublished": "2026-10-20"}}</script></head><body>{teasers}"#
    ));
    assert_eq!(typed(&html), Some(Schema::Article));
}

#[test]
fn a_wordpress_index_is_a_list() {
    let card = |n: u32| {
        format!(
            r#"<article id="post-{n}" class="post-{n} post type-post status-publish format-standard hentry category-news {}">
            <h2 class="entry-title"><a href="/{n}/">A post about lamps, number {n}</a></h2>
            <div class="entry-meta"><time datetime="2026-10-0{n}">Oct {n}</time> <span class="author">Ana</span></div>
            <div class="entry-summary"><p>{BODY}</p></div></article>"#,
            if n.is_multiple_of(2) { "even" } else { "odd" }
        )
    };
    let html = format!(
        "<main><h1>News</h1>{}</main>",
        (1..=6).map(card).collect::<String>()
    );
    let list = read(Schema::ArticleList, &html);
    assert_eq!(get(&list, "articles").as_array().map(Vec::len), Some(6));
    assert_eq!(typed(&html), Some(Schema::ArticleList));
}

#[test]
fn link_grids_without_dates_or_bylines_arent_article_lists() {
    let product = |n: u32| {
        format!(
            r#"<li class="product"><a href="/p/{n}"><img src="/i/{n}.jpg" alt=""></a><h3><a href="/p/{n}">Lamp {n}</a></h3><button>Request a quote</button></li>"#
        )
    };
    let job = |n: u32| {
        format!(
            r#"<li class="job"><h3><a href="/jobs/{n}">Gardener {n}</a></h3><span class="company">Green Ltd</span><p>Full time, £30,000 a year</p></li>"#
        )
    };
    for html in [
        format!(
            "<main><h1>Lamps</h1><ul>{}</ul></main>",
            (1..=4).map(product).collect::<String>()
        ),
        format!(
            "<main><h1>Jobs</h1><ul>{}</ul></main>",
            (1..=4).map(job).collect::<String>()
        ),
    ] {
        assert!(read(Schema::ArticleList, &html).probability < 0.4, "{html}");
        assert_ne!(typed(&html), Some(Schema::ArticleList), "{html}");
    }
}

#[test]
fn listing_pages_of_threads_arent_a_thread() {
    let html = r#"<script type="application/ld+json">[
        {"@type": "DiscussionForumPosting", "headline": "T1", "url": "https://site.example/t/1", "text": "First"},
        {"@type": "DiscussionForumPosting", "headline": "T2", "url": "https://site.example/t/2", "text": "Second"}]</script>"#;
    assert!(read(Schema::ForumThread, html).probability < 0.4);
}

#[test]
fn nested_comments_are_posts_too() {
    let html = r#"<script type="application/ld+json">{"@type": "DiscussionForumPosting", "headline": "T",
        "text": "Opening post", "comment": [{"@type": "Comment", "text": "A reply",
        "comment": [{"@type": "Comment", "text": "A reply to the reply"}]}]}</script>"#;
    assert_eq!(texts(&read(Schema::ForumThread, html)).len(), 3);
}

#[test]
fn a_card_reads_its_teaser_byline_and_date_past_a_kicker() {
    let card = |n: u32| {
        format!(
            r#"<div class="card"><p class="category">Opinion</p><h4 class="kicker"><a href="/opinion">Opinion</a></h4>
            <h2><a href="/s/{n}">A long look at lamps, part {n}</a></h2><time datetime="2026-10-0{n}">Oct {n}</time>
            <div class="byline">Ana Ruiz · 5 min read</div><p>{BODY}</p></div>"#
        )
    };
    let html = format!(
        "<main><h1>Opinion</h1>{}</main>",
        (1..=3).map(card).collect::<String>()
    );
    let list = read(Schema::ArticleList, &html);
    let first = &get(&list, "articles")[0];
    assert_eq!(first["article_body"], BODY);
    assert_eq!(first["authors"][0]["name"], "Ana Ruiz");
    assert_eq!(first["date_published"], "2026-10-01");
}

#[test]
fn likes_read_as_people_write_them() {
    let post = |score: &str| {
        format!(
            r#"<div class="post"><span class="author">u</span> <time datetime="2026-10-20">Oct 20</time>
            <div class="body"><p>{BODY}</p></div><span class="score">{score}</span></div>"#
        )
    };
    let html = format!(
        "<main><h1>T</h1><div>{}{}{}</div></main>",
        post("1.2k points"),
        post("-4 points"),
        post("7 points")
    );
    let posts = get(&read(Schema::ForumThread, &html), "posts").clone();
    let likes: Vec<&Value> = posts
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["reactions"]["likes"])
        .collect();
    assert_eq!(likes, [&Value::from(1200), &Value::Null, &Value::from(7)]);
}

#[test]
fn a_thread_with_a_box_of_recent_topics_is_a_thread() {
    let post = |n: u32| {
        format!(
            r#"<div class="post"><span class="author">u{n}</span> <time datetime="2026-10-2{n}">Oct 2{n}</time><div class="body"><p>{BODY}</p></div></div>"#
        )
    };
    let recent = (1..=3)
        .map(|n| format!(r#"<div class="topic"><h3><a href="/t/{n}">Another topic, number {n}</a></h3></div>"#))
        .collect::<String>();
    let html = format!(
        "<main><h1>Flicker</h1><div>{}{}{}</div><div class=\"recent\">{recent}</div></main>",
        post(1),
        post(2),
        post(3)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
}

#[test]
fn no_listing_no_category_name() {
    let item = read(
        Schema::ArticleNavigation,
        "<main><h1>Oak stool</h1><p>£59</p></main>",
    );
    assert_eq!(get(&item, "category_name"), &Value::Null);
}

#[test]
fn a_blog_index_isnt_its_first_post() {
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/auto/article-list-jsonld.html"
    ))
    .unwrap();
    let doc = Document::parse(&page);
    let url = url::Url::parse("https://farm.example/notes").unwrap();
    let article = extract(
        Schema::Article,
        doc.root(),
        Some(&url),
        None,
        Civil::date(2026, 10, 25),
    );
    assert!(article.probability < 0.4, "{}", article.probability);
}
