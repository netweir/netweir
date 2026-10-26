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

#[test]
fn xenforo_with_its_avatar_links_is_a_thread() {
    let post = |n: u32| {
        format!(
            r#"<article class="message message--post"><div class="message-inner">
            <div class="message-cell message-cell--user"><div class="message-avatar"><a href="/members/u{n}.{n}/" class="avatar"><img src="/a/{n}.jpg" alt="u{n}"></a></div>
            <h4 class="message-name"><a href="/members/u{n}.{n}/" class="username">u{n}</a></h4></div>
            <div class="message-cell message-cell--main"><header class="message-attribution"><a href="/posts/{n}"><time datetime="2026-10-2{n}T10:00:00Z">Oct 2{n}</time></a></header>
            <div class="message-content"><article class="message-body"><div class="bbWrapper">Reply {n}. {BODY}</div></article></div></div></div></article>"#
        )
    };
    let html = format!(
        r#"<main><h1>Flicker</h1><div class="block-body">{}{}{}</div></main>"#,
        post(1),
        post(2),
        post(3)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 3);
}

#[test]
fn phpbb_with_its_profile_column_is_a_thread_with_dates() {
    let post = |n: u32| {
        format!(
            r#"<div class="post bg{}"><div class="inner"><dl class="postprofile"><dt><a href="./memberlist.php?mode=viewprofile&amp;u={n}" class="username">kit{n}</a></dt><dd>Posts: 12</dd></dl>
            <div class="postbody"><p class="author">by <a href="./memberlist.php?mode=viewprofile&amp;u={n}" class="username">kit{n}</a> » <time datetime="2026-10-2{n}T10:00:00Z">Mon Oct 2{n}</time></p>
            <div class="content">Post {n}. {BODY}</div></div></div></div>"#,
            n % 2 + 1
        )
    };
    let html = format!(
        "<main><h1>Flicker</h1><div class=\"topic\">{}{}{}</div></main>",
        post(1),
        post(2),
        post(3)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
    let thread = read(Schema::ForumThread, &html);
    assert_eq!(texts(&thread).len(), 3);
    assert_eq!(
        get(&thread, "posts")[0]["date_published"],
        "2026-10-21T10:00:00Z"
    );
}

#[test]
fn cards_opening_with_a_label_are_a_list_not_a_thread() {
    let card = |n: u32| {
        format!(
            r#"<article class="card"><a href="/s/{n}"><img src="/i/{n}.jpg" alt=""></a><p class="eyebrow">News</p>
            <h2><a href="/s/{n}">A long look at lamps, part {n}</a></h2><time datetime="2026-10-0{n}">Oct {n}</time>
            <div class="byline">Ana Ruiz</div><p>{BODY}</p></article>"#
        )
    };
    let html = format!(
        "<main><h1>News</h1>{}</main>",
        (1..=5).map(card).collect::<String>()
    );
    assert_eq!(typed(&html), Some(Schema::ArticleList));
    assert!(read(Schema::ForumThread, &html).probability < 0.4);
}

#[test]
fn a_reply_button_doesnt_make_a_post_a_teaser() {
    let post = |n: u32| {
        format!(
            r#"<li class="postcontainer"><div class="userinfo"><a class="username" href="/member.php?u={n}">kit{n}</a></div>
            <span class="date"><time datetime="2026-10-2{n}">Oct 2{n}</time></span>
            <div class="postbody"><blockquote class="postcontent restore">Post {n}. {BODY}</blockquote></div>
            <div class="postfoot"><a class="newreply" href="newreply.php?p={n}"><span>Reply With Quote</span></a></div></li>"#
        )
    };
    let html = format!(
        "<main><h1>Flicker</h1><ol class=\"posts\">{}{}{}</ol></main>",
        post(1),
        post(2),
        post(3)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
}

#[test]
fn an_opening_post_with_extra_classes_keeps_its_replies() {
    let post = |classes: &str, n: u32| {
        format!(
            r#"<div class="{classes}"><span class="author">u{n}</span> <time datetime="2026-10-2{n}">Oct 2{n}</time><div class="body"><p>Post {n}. {BODY}</p></div></div>"#
        )
    };
    let html = format!(
        "<main><h1>Flicker</h1><div>{}{}{}{}</div></main>",
        post("post original-post pinned-post", 1),
        post("post", 2),
        post("post", 3),
        post("post", 4)
    );
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 4);
}

#[test]
fn replies_inside_main_under_an_article_arent_a_thread() {
    let reply = |n: u32| {
        format!(
            r#"<div class="reply"><span class="author">Kit</span> <time datetime="2026-10-2{n}">Oct 2{n}</time><p>{BODY}</p></div>"#
        )
    };
    let prose = format!("<p>{BODY} {BODY} {BODY}</p>").repeat(4);
    let html = format!(
        r#"<main><article><h1>The night market returns</h1>{prose}</article><section class="replies">{}{}{}</section></main>"#,
        reply(1),
        reply(2),
        reply(3)
    );
    assert!(read(Schema::ForumThread, &html).probability < 0.4);
    assert_eq!(typed(&html), Some(Schema::Article));
}

#[test]
fn a_post_keeps_every_paragraph() {
    let post = |n: u32| {
        format!(
            r#"<li class="post"><span class="author">u{n}</span> <time datetime="2026-10-2{n}">Oct 2{n}</time>
            <p>First paragraph of post {n}.</p><p>{BODY}</p><p>Third, short again.</p></li>"#
        )
    };
    let html = format!(
        "<main><h1>T</h1><ul>{}{}{}</ul></main>",
        post(1),
        post(2),
        post(3)
    );
    let first = &texts(&read(Schema::ForumThread, &html))[0];
    assert!(first.starts_with("First paragraph of post 1."), "{first}");
    assert!(first.ends_with("Third, short again."), "{first}");
}

#[test]
fn a_member_directory_isnt_a_thread() {
    let member = |n: u32| {
        format!(
            r#"<div class="member-card"><img src="/a/{n}.jpg" alt=""><a href="/members/u{n}">user{n}</a>
            <div class="stats">Joined October 2020 · 234 posts · Leeds, United Kingdom</div></div>"#
        )
    };
    let html = format!(
        "<main><h1>Members</h1>{}</main>",
        (1..=6).map(member).collect::<String>()
    );
    assert!(read(Schema::ForumThread, &html).probability < 0.4);
    assert_ne!(typed(&html), Some(Schema::ForumThread));
}

#[test]
fn a_people_section_is_still_a_list() {
    let card = |n: u32| {
        format!(
            r#"<div class="card"><h3><a href="/people/story-{n}">A profile of a lamp maker, part {n}</a></h3>
            <time datetime="2026-10-0{n}">Oct {n}</time><p>{BODY}</p></div>"#
        )
    };
    let html = format!(
        "<main><h1>People</h1>{}</main>",
        (1..=4).map(card).collect::<String>()
    );
    assert_eq!(typed(&html), Some(Schema::ArticleList));
}

#[test]
fn github_and_old_reddit_comments_are_threads() {
    let github = |n: u32| {
        format!(
            r#"<div class="timeline-comment"><div class="timeline-comment-header"><a class="author" href="/kit{n}">kit{n}</a> commented <relative-time datetime="2026-10-2{n}T10:00:00Z">Oct 2{n}</relative-time></div>
            <div class="comment-body"><p>Comment {n}. {BODY}</p></div></div>"#
        )
    };
    let reddit = |n: u32| {
        format!(
            r#"<div class="thing comment"><div class="entry"><p class="tagline"><a href="/user/kit{n}" class="author">kit{n}</a> <time datetime="2026-10-2{n}T10:00:00Z">2 hours ago</time></p>
            <div class="usertext-body"><div class="md"><p>Comment {n}. {BODY}</p></div></div></div></div>"#
        )
    };
    for card in [github as fn(u32) -> String, reddit] {
        let html = format!(
            "<main><h1>Lamp flickers when dimmed</h1><div class=\"discussion\">{}{}{}</div></main>",
            card(1),
            card(2),
            card(3)
        );
        assert_eq!(typed(&html), Some(Schema::ForumThread), "{html}");
        let thread = read(Schema::ForumThread, &html);
        assert_eq!(texts(&thread).len(), 3, "{html}");
        assert_eq!(
            get(&thread, "posts")[0]["date_published"],
            "2026-10-21T10:00:00Z"
        );
    }
}

#[test]
fn a_like_button_doesnt_hide_the_count() {
    let post = |n: u32| {
        format!(
            r#"<div class="post"><span class="author">u{n}</span> <time datetime="2026-10-2{n}">Oct 2{n}</time><div class="body"><p>{BODY}</p></div>
            <a class="like-button" href="/like">Like</a> <span class="like-count">{n}</span></div>"#
        )
    };
    let html = format!(
        "<main><h1>T</h1><div>{}{}{}</div></main>",
        post(1),
        post(2),
        post(3)
    );
    let posts = get(&read(Schema::ForumThread, &html), "posts").clone();
    assert_eq!(posts[1]["reactions"]["likes"], 2);
}

#[test]
fn a_thousand_profile_links_are_quick() {
    let who: String = (0..20_000)
        .map(|i| format!(r#"<li><a href="/members/u{i}">u{i}</a></li>"#))
        .collect();
    let html = format!("<main><h1>Online</h1><ul>{who}</ul></main>");
    let doc = Document::parse(&html);
    let start = std::time::Instant::now();
    extract(
        Schema::ForumThread,
        doc.root(),
        None,
        None,
        Civil::date(2026, 10, 25),
    );
    assert!(start.elapsed().as_secs_f64() < 1.5, "{:?}", start.elapsed());
}

fn xenforo_post(n: u32, mention: bool) -> String {
    let tail = if mention {
        r#" Thanks <a href="/members/bob.9/" class="username">@bob</a>, that was it."#
    } else {
        ""
    };
    format!(
        r#"<article class="message message--post"><div class="message-inner">
        <div class="message-cell message-cell--user"><section class="message-user">
        <div class="message-avatar"><a href="https://site.example/members/u{n}.{n}/" class="avatar"><img src="/a/{n}.jpg" alt="u{n}"></a></div>
        <div class="message-userDetails"><h4 class="message-name"><a href="/members/u{n}.{n}/" class="username">u{n}</a></h4><h5 class="userTitle">Member</h5></div>
        <div class="message-userExtras"><dl class="pairs"><dt>Joined</dt><dd>Mar 3, 2020</dd></dl><dl class="pairs"><dt>Messages</dt><dd>1,204</dd></dl></div>
        </section></div>
        <div class="message-cell message-cell--main"><div class="message-main"><header class="message-attribution"><ul class="listInline"><li><a href="/posts/{n}/"><time datetime="2026-10-2{n}T10:00:00Z">Oct 2{n}</time></a></li></ul></header>
        <div class="message-content"><article class="message-body"><div class="bbWrapper">Reply {n}. {BODY}{tail}</div></article></div>
        <footer class="message-footer"><a href="/posts/{n}/react" class="reaction">Like</a> <a href="/posts/{n}/reply">Reply</a></footer></div></div></div></article>"#
    )
}

#[test]
fn a_real_xenforo_thread_with_mentions_keeps_every_post_and_no_profile_text() {
    let html = format!(
        r#"<main><h1>Flicker</h1><div class="block-body">{}{}{}{}</div></main>"#,
        xenforo_post(1, false),
        xenforo_post(2, true),
        xenforo_post(3, true),
        xenforo_post(4, true)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
    let thread = read(Schema::ForumThread, &html);
    let posts = texts(&thread);
    assert_eq!(posts.len(), 4);
    assert!(posts[0].starts_with("Reply 1."), "{}", posts[0]);
    assert!(
        posts[1].contains("Thanks @bob, that was it."),
        "{}",
        posts[1]
    );
    assert!(
        posts
            .iter()
            .all(|p| !p.contains("Joined") && !p.contains("Reply\n")),
        "{posts:?}"
    );
    assert_eq!(
        get(&thread, "posts")[3]["date_published"],
        "2026-10-24T10:00:00Z"
    );
}

#[test]
fn inline_dates_and_tables_dont_split_posts() {
    let post = |n: u32, extra: &str| {
        format!(
            r#"<div class="post"><span class="author">u{n}</span> <time datetime="2026-10-2{n}">Oct 2{n}</time>
            <div class="body"><p>Post {n}. {BODY}</p>{extra}</div></div>"#
        )
    };
    let html = format!(
        "<main><h1>T</h1><div>{}{}{}{}{}</div></main>",
        post(
            1,
            r#"<p>As I said <time datetime="2026-10-01">last week</time> in the other thread, it's the dimmer.</p>"#
        ),
        post(
            2,
            r#"<table><tr><td>Fitted</td><td><time datetime="2026-10-02">Oct 2</time></td></tr></table>"#
        ),
        post(3, ""),
        post(4, ""),
        post(5, "")
    );
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 5);
}

#[test]
fn githubs_real_comment_header_is_a_thread() {
    let comment = |n: u32| {
        format!(
            r##"<div class="timeline-comment"><div class="timeline-comment-header"><h3 class="timeline-comment-header-text f5 text-normal">
            <strong><a class="author Link--primary" href="/kit{n}">kit{n}</a></strong> commented <a href="#issuecomment-{n}"><relative-time datetime="2026-10-2{n}T10:00:00Z">Oct 2{n}</relative-time></a></h3></div>
            <div class="edit-comment-hide"><table class="d-block"><tbody><tr><td class="comment-body markdown-body"><p>Comment {n}. {BODY}</p></td></tr></tbody></table></div></div>"##
        )
    };
    let html = format!(
        "<main><h1>Lamp flickers when dimmed</h1><div class=\"js-discussion\">{}{}{}</div></main>",
        comment(1),
        comment(2),
        comment(3)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 3);
}

#[test]
fn old_reddits_real_nested_comments_are_all_read() {
    fn thing(n: u32, children: &str) -> String {
        format!(
            r#"<div class="thing comment" id="t1_{n}"><div class="entry unvoted"><p class="tagline">
            <a href="https://old.reddit.com/user/kit{n}" class="author">kit{n}</a>
            <span class="score dislikes">{} points</span><span class="score unvoted">{} points</span><span class="score likes">{} points</span>
            <time datetime="2026-10-2{n}T10:00:00Z">2 hours ago</time></p>
            <form class="usertext warn-on-unload"><div class="usertext-body"><div class="md"><p>Comment {n}. {BODY}</p></div></div></form>
            <ul class="flat-list buttons"><li><a href="/r/x/c/{n}">permalink</a></li><li><a href="/reply">reply</a></li></ul></div>
            <div class="child"><div class="sitetable listing">{children}</div></div></div>"#,
            n + 1,
            n + 2,
            n + 3
        )
    }
    let tree = format!(
        "{}{}",
        thing(1, &format!("{}{}", thing(2, &thing(3, "")), thing(4, ""))),
        thing(5, &thing(6, ""))
    );
    let html = format!(
        r#"<main><h1>Lamp flickers when dimmed</h1><div class="commentarea"><div class="sitetable nestedlisting">{tree}</div></div></main>"#
    );
    let thread = read(Schema::ForumThread, &html);
    let posts = texts(&thread);
    assert_eq!(typed(&html), Some(Schema::ForumThread));
    assert_eq!(posts.len(), 6, "{posts:?}");
    assert!(
        posts
            .iter()
            .enumerate()
            .all(|(i, p)| p == &format!("Comment {}. {BODY}", i + 1)),
        "{posts:?}"
    );
}

#[test]
fn a_vbulletin_date_without_time_is_read() {
    let post = |n: u32| {
        format!(
            r#"<li class="postcontainer"><div class="posthead"><span class="postdate"><span class="date">Oct 2{n}, 2026,&nbsp;<span class="time">10:00 AM</span></span></span><span class="nodecontrols">#{n}</span></div>
            <div class="userinfo"><a class="username" href="member.php?u={n}">kit{n}</a></div>
            <div class="postbody"><h2 class="title">Re: Flicker</h2><blockquote class="postcontent restore">Post {n}. {BODY}</blockquote></div>
            <div class="postfoot"><a class="newreply" href="newreply.php?p={n}"><span>Reply With Quote</span></a></div></li>"#
        )
    };
    let html = format!(
        "<main><h1>Flicker</h1><ol class=\"posts\">{}{}{}</ol></main>",
        post(1),
        post(2),
        post(3)
    );
    let thread = read(Schema::ForumThread, &html);
    let posts = texts(&thread);
    assert_eq!(posts.len(), 3);
    assert_eq!(posts[0], format!("Post 1. {BODY}"));
    assert_eq!(
        get(&thread, "posts")[0]["date_published"],
        "2026-10-21T10:00:00"
    );
}

#[test]
fn a_byline_and_an_update_note_arent_posts() {
    let html = format!(
        r#"<main><article><h1>The night market returns</h1>
        <div class="byline">By <span class="author">Ana Ruiz</span>, <time datetime="2026-10-20">Oct 20</time>, staff writer in Leeds covering housing and transport</div>
        <div class="updated"><span class="author">Ana Ruiz</span> updated this story on <time datetime="2026-10-21">Oct 21</time> with a correction to the stall count</div>
        <p>{BODY}</p><p>{BODY}</p><p>{BODY}</p></article></main>"#
    );
    assert!(read(Schema::ForumThread, &html).probability < 0.4);
}

#[test]
fn teasers_with_a_byline_before_the_title_are_a_list_not_a_thread() {
    let card = |n: u32| {
        format!(
            r#"<div class="card"><div class="byline">By Ana Ruiz · <time datetime="2026-10-0{n}">October {n}, 2026</time> · 5 min read</div>
            <h5 class="title"><a href="/b/{n}">A post about lamps, number {n}</a></h5><p>{BODY}</p></div>"#
        )
    };
    let html = format!(
        "<main><h1>Blog</h1>{}</main>",
        (1..=5).map(card).collect::<String>()
    );
    assert!(read(Schema::ForumThread, &html).probability < 0.4);
}

#[test]
fn a_question_with_answers_is_a_thread() {
    let answer = |n: u32| {
        format!(
            r#"<div class="answer"><div class="votecell"><div class="js-vote-count" itemprop="upvoteCount">{n}</div></div>
            <div class="answercell"><div class="s-prose"><p>Answer {n}. {BODY}</p></div>
            <div class="user-details" itemprop="author"><a href="/users/{n}/kit">kit{n}</a> <span class="relativetime" title="2026-10-2{n} 10:00:00Z">Oct 2{n}</span></div></div></div>"#
        )
    };
    let question = format!(
        r#"<div class="question"><div class="s-prose"><p>{BODY} {BODY} {BODY} {BODY} {BODY}</p></div>
        <div class="user-details" itemprop="author"><a href="/users/9/lee">lee</a></div></div>"#
    );
    let html = format!(
        r#"<main><h1>Why does my lamp flicker?</h1>{question}<div id="answers">{}{}{}</div></main>"#,
        answer(1),
        answer(2),
        answer(3)
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
}

#[test]
fn a_product_with_reviews_is_a_product() {
    let review = |n: u32| {
        format!(
            r#"<div class="review"><span class="review-author">Kit {n}</span> <time datetime="2026-10-2{n}">Oct 2{n}</time><p>{BODY}</p>
            <span class="review-votes">{n} people found this helpful</span></div>"#
        )
    };
    let html = format!(
        r#"<main><h1>Oak stool</h1><span class="price">£59.00</span><button>Add to basket</button><p>A low stool.</p>
        <section class="reviews">{}</section></main>"#,
        (1..=5).map(review).collect::<String>()
    );
    assert_eq!(typed(&html), Some(Schema::Product));
    assert!(read(Schema::ForumThread, &html).probability < 0.4);
}

#[test]
fn deep_posts_are_quick() {
    let deep = |n: u32| {
        format!(
            r#"<div class="post"><span class="author">u{n}</span>{}<time datetime="2026-10-2{n}">Oct</time>{}</div>"#,
            "<div><p>Some words in a paragraph here.</p>".repeat(1500),
            "</div>".repeat(1500)
        )
    };
    let html = format!("<main><h1>T</h1><div>{}{}</div></main>", deep(1), deep(2));
    let doc = Document::parse(&html);
    let start = std::time::Instant::now();
    extract(
        Schema::ForumThread,
        doc.root(),
        None,
        None,
        Civil::date(2026, 10, 25),
    );
    assert!(start.elapsed().as_secs_f64() < 1.5, "{:?}", start.elapsed());
}

fn three(post: impl Fn(u32) -> String) -> String {
    (1..=3).map(post).collect()
}

#[test]
fn a_date_wrapping_a_permalink_isnt_a_title() {
    let html = format!(
        "<main><h1>T</h1><div>{}</div></main>",
        three(|n| format!(
            r#"<div class="post"><span class="author">u{n}</span> <span class="date"><a href="/posts/{n}">Oct 2{n}, 2026 10:00 AM</a></span><div class="body"><p>{BODY}</p></div></div>"#
        ))
    );
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 3);
}

#[test]
fn xenforo_ones_relative_profile_links_are_posters() {
    let html = format!(
        "<main><h1>Flicker</h1><ol class=\"messageList\">{}</ol></main>",
        three(|n| format!(
            r#"<li class="message"><div class="messageUserInfo"><h3 class="userText"><a href="members/kit{n}.{n}/" class="username">kit{n}</a></h3></div>
            <div class="messageContent"><blockquote class="messageText">Post {n}. {BODY}</blockquote></div>
            <div class="messageMeta"><a href="members/kit{n}.{n}/" class="username author">kit{n}</a> <a href="posts/{n}/" class="datePermalink"><span class="DateTime">Oct 2{n}, 2026</span></a></div></li>"#
        ))
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
}

#[test]
fn a_reply_under_a_long_quote_keeps_its_own_words() {
    let html = format!(
        "<main><h1>T</h1><div>{}</div></main>",
        three(|n| format!(
            r#"<div class="post"><span class="author">u{n}</span> <time datetime="2026-10-2{n}">Oct</time><div class="body"><blockquote><cite><a href="/members/kit.1">kit</a> said: <time datetime="2026-10-20">Oct 20</time></cite><p>{BODY} {BODY}</p></blockquote>Yes, same here.</div></div>"#
        ))
    );
    let posts = texts(&read(Schema::ForumThread, &html));
    assert!(
        posts.iter().all(|p| p.ends_with("Yes, same here.")),
        "{posts:?}"
    );
}

#[test]
fn nested_classless_and_custom_element_comments_are_all_read() {
    fn li(n: u32, children: &str) -> String {
        format!(
            r#"<li><div class="meta"><a href="/user/kit{n}">kit{n}</a> <time datetime="2026-10-2{n}">Oct</time></div><div class="comment-text"><p>Comment {n}. {BODY}</p></div>{children}</li>"#
        )
    }
    let tree = format!(
        r#"<ul class="comment-list">{}{}</ul>"#,
        li(
            1,
            &format!("<ul>{}</ul>", li(2, &format!("<ul>{}</ul>", li(3, ""))))
        ),
        li(4, &format!("<ul>{}{}</ul>", li(5, ""), li(6, "")))
    );
    let html = format!("<main><h1>Flicker</h1>{tree}</main>");
    let posts = texts(&read(Schema::ForumThread, &html));
    assert_eq!(posts.len(), 6, "{posts:?}");
    assert!(
        posts
            .iter()
            .enumerate()
            .all(|(i, p)| p.starts_with(&format!("Comment {}.", i + 1))),
        "{posts:?}"
    );
    fn shreddit(n: u32, children: &str) -> String {
        format!(
            r#"<shreddit-comment author="kit{n}"><div slot="commentMeta"><a href="/user/kit{n}/">kit{n}</a> <time datetime="2026-10-2{n}T10:00:00Z">2h</time></div><div slot="comment"><p>Comment {n}. {BODY}</p></div>{children}</shreddit-comment>"#
        )
    }
    let tree = format!(
        "{}{}",
        shreddit(1, &shreddit(2, &shreddit(3, ""))),
        shreddit(4, &shreddit(5, ""))
    );
    let html = format!(
        "<main><h1>Flicker</h1><shreddit-comment-tree>{tree}</shreddit-comment-tree></main>"
    );
    let posts = texts(&read(Schema::ForumThread, &html));
    assert_eq!(posts.len(), 5, "{posts:?}");
}

#[test]
fn og_type_article_alone_doesnt_demote_a_forum() {
    let html = format!(
        r#"<html><head><meta property="og:type" content="article"></head><body><main><h1>Flicker</h1><div class="bbp-replies">{}</div></main></body></html>"#,
        three(|n| format!(
            r#"<div class="bbp-reply"><div class="bbp-reply-author"><a href="/forums/users/kit{n}/">kit{n}</a></div><div class="bbp-reply-content"><p>Reply {n}. {BODY}</p></div></div>"#
        ))
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
}

#[test]
fn invision_author_panes_are_posters() {
    let html = format!(
        "<main><h1>Flicker</h1><div class=\"cTopic\">{}</div></main>",
        three(|n| format!(
            r#"<article class="cPost ipsBox ipsComment ipsComment_parent"><aside class="ipsComment_author cAuthorPane"><h3 class="cAuthorPane_author"><a href="https://site.example/profile/{n}-kit{n}/">kit{n}</a></h3></aside>
            <div class="ipsColumn"><time datetime="2026-10-2{n}T10:00:00Z">Oct 2{n}</time><div data-role="commentContent" class="ipsType_richText"><p>Post {n}. {BODY}</p></div></div></article>"#
        ))
    );
    assert_eq!(typed(&html), Some(Schema::ForumThread));
}

#[test]
fn testimonials_authors_and_unlinked_teasers_arent_threads() {
    let testimonials = format!(
        "<main><h1>What our customers say</h1>{}</main>",
        (1..=4)
            .map(|n| format!(r#"<div class="testimonial"><blockquote><p>{BODY} {n}</p></blockquote><cite class="author">Jane Doe {n}, Head of School</cite></div>"#))
            .collect::<String>()
    );
    let authors = format!(
        "<main><h1>Our writers</h1>{}</main>",
        (1..=5)
            .map(|n| format!(r#"<div class="author-card"><img src="/a/{n}.jpg" alt=""><h3>Writer {n}</h3><p>{BODY} {n}</p></div>"#))
            .collect::<String>()
    );
    let teasers = format!(
        "<main><h1>Blog</h1>{}</main>",
        (1..=6)
            .map(|n| format!(
                r#"<article class="card"><h2>A long look at lamps, part {n}</h2><div class="meta"><span class="author">Ana</span> <time datetime="2026-10-0{n}">Oct</time></div><p>{BODY}</p><a class="more" href="/s/{n}">Read more</a></article>"#
            ))
            .collect::<String>()
    );
    for html in [testimonials, authors, teasers] {
        assert_ne!(typed(&html), Some(Schema::ForumThread), "{html}");
    }
}

#[test]
fn comments_under_an_article_in_main_arent_a_thread() {
    let comments = three(|n| {
        format!(
            r#"<div class="comment"><span class="author">Kit</span> <time datetime="2026-10-2{n}">Oct</time><p>{BODY}</p></div>"#
        )
    });
    let html = format!(
        r#"<main><h1>The night market returns</h1><p class="byline">By <span class="author">Ana</span>, <time datetime="2026-10-20">Oct 20</time></p>{}<div class="comments">{comments}</div></main>"#,
        format!("<p>{BODY} {BODY}</p>").repeat(5)
    );
    assert!(read(Schema::ForumThread, &html).probability < 0.4);
}

#[test]
fn discourses_noscript_posts_are_read() {
    let html = format!(
        r#"<main><h1>Flicker</h1><noscript data-path="/t/flicker/88">{}</noscript></main>"#,
        three(|n| format!(
            r#"<div class="topic-body crawler-post"><div class="crawler-post-meta"><span class="creator"><a href="/u/kit{n}"><span>kit{n}</span></a></span>
            <span class="crawler-post-infos"><time datetime="2026-10-2{n}T10:00:00Z" class="post-time">October 2{n}, 2026, 10:00am</time></span></div>
            <div class="post"><p>Post {n}. {BODY}</p></div></div>"#
        ))
    );
    assert_eq!(texts(&read(Schema::ForumThread, &html)).len(), 3);
}

#[test]
fn many_classless_posts_are_quick() {
    let posts: String = (0..20_000)
        .map(|n| format!(r#"<div><div><span class="author">u{n}</span> <time datetime="2026-10-20">Oct</time><p>{BODY}</p></div></div>"#))
        .collect();
    let html = format!("<main><h1>T</h1>{posts}</main>");
    let doc = Document::parse(&html);
    let start = std::time::Instant::now();
    extract(
        Schema::ForumThread,
        doc.root(),
        None,
        None,
        Civil::date(2026, 10, 25),
    );
    assert!(start.elapsed().as_secs_f64() < 6.0, "{:?}", start.elapsed());
}
