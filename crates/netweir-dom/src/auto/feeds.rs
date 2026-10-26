//! Pages of many things: lists of articles (ArticleList), where a blog
//! or a jobs board leads (ArticleNavigation, JobPostingNavigation), and
//! forum threads (ForumThread). Structured data first (a Blog's posts,
//! an ItemList, several JobPostings, a DiscussionForumPosting and its
//! comments), then the layout: a group of cards each led by a linked
//! heading, or a group of posts each with a date or a byline.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::article;
use super::fields as f;
use super::schema_org::{Source, Thing, of_type};
use super::{Builder, Page};
use crate::content::{in_boilerplate, page_heading, says_boilerplate, words};
use crate::{Node, NodeId, NodeKind};

pub(crate) const ARTICLE_LIST_FIELDS: &[&str] =
    &["url", "canonical_url", "articles", "breadcrumbs"];

pub(crate) const NAVIGATION_FIELDS: &[&str] = &[
    "url",
    "category_name",
    "sub_categories",
    "items",
    "next_page",
    "page_number",
];

pub(crate) const THREAD_FIELDS: &[&str] = &["url", "topic", "posts"];

/// Levels above a linked heading, or a post's date, searched for the
/// card or post holding it.
const UP: usize = 5;

/// Cards at most read from one group.
const MAX_CARDS: usize = 500;

/// An article as a list shows it, from a schema.org Article.
fn article_entry(
    page: &Page<'_, '_>,
    a: &Map<String, Value>,
    lang: Option<&str>,
) -> Map<String, Value> {
    let mut out = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v.filter(|v| !v.is_null()) {
            out.insert(k.into(), v);
        }
    };
    let get = |k: &str| a.get(k).unwrap_or(&Value::Null);
    put(
        "headline",
        f::text(get("headline"))
            .or_else(|| f::text(get("name")))
            .map(Value::String),
    );
    put(
        "url",
        get("url")
            .as_str()
            .and_then(|u| f::absolute(u, page.base))
            .map(Value::String),
    );
    put(
        "date_published",
        f::iso_date(get("datePublished"), page.now).map(Value::String),
    );
    put(
        "date_published_raw",
        f::text(get("datePublished")).map(Value::String),
    );
    put("authors", article::authors(get("author"), page));
    put("main_image", f::image(get("image"), page.base));
    put(
        "article_body",
        f::text(get("articleBody"))
            .or_else(|| f::text(get("description")))
            .map(Value::String),
    );
    put(
        "in_language",
        article::language(get("inLanguage"))
            .or(lang.map(str::to_string))
            .map(Value::String),
    );
    out
}

/// The page's language, from `<html lang>`.
fn page_language(page: &Page<'_, '_>) -> Option<String> {
    std::iter::once(page.root)
        .chain(page.root.descendants())
        .find(|n| n.tag() == Some("html"))
        .and_then(|h| h.attr("lang"))
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

/// The articles a page's structured data lists, and where from.
fn listed_articles(page: &Page<'_, '_>) -> Option<(Vec<Map<String, Value>>, String)> {
    let lang = page_language(page);
    let lang = lang.as_deref();
    let entries = |list: &[&Map<String, Value>]| -> Vec<Map<String, Value>> {
        list.iter()
            .map(|a| article_entry(page, a, lang))
            .filter(|e| e.contains_key("headline") || e.contains_key("url"))
            .collect()
    };
    // A Blog's posts.
    for blog in page.things.iter().filter(|t| t.is("Blog")) {
        let posts: Vec<&Map<String, Value>> =
            each(blog.props.get("blogPost").unwrap_or(&Value::Null))
                .filter_map(Value::as_object)
                .collect();
        let found = entries(&posts);
        if !found.is_empty() {
            return Some((found, label(blog.source)));
        }
    }
    // An ItemList whose entries are articles.
    for list in page
        .things
        .iter()
        .filter(|t| t.is("ItemList") && !t.is("BreadcrumbList"))
    {
        let items: Vec<&Map<String, Value>> =
            each(list.props.get("itemListElement").unwrap_or(&Value::Null))
                .filter_map(Value::as_object)
                .map(|e| match e.get("item") {
                    Some(Value::Object(inner)) => inner,
                    _ => e,
                })
                .filter(|e| typed(e, "Article"))
                .collect();
        let found = entries(&items);
        if found.len() >= 2 {
            return Some((found, label(list.source)));
        }
    }
    // Several Articles, none of them the page's own.
    for source in [Source::JsonLd, Source::Microdata] {
        let articles: Vec<&Thing> = of_type(&page.things, "Article")
            .into_iter()
            .filter(|t| t.source == source && !t.is("DiscussionForumPosting"))
            .collect();
        let linked = articles.iter().all(|a| {
            a.props
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| !page.is_page_url(u))
        });
        if articles.len() >= 2 && linked {
            let props: Vec<&Map<String, Value>> = articles.iter().map(|t| &t.props).collect();
            return Some((entries(&props), label(source)));
        }
    }
    None
}

/// Whether a nested JSON-LD object is of `ty` or one of its subtypes.
fn typed(o: &Map<String, Value>, ty: &str) -> bool {
    let types: Vec<&str> = match o.get("@type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(l)) => l.iter().filter_map(Value::as_str).collect(),
        _ => return false,
    };
    types.iter().any(|t| super::schema_org::is_a(t, ty))
}

fn label(source: Source) -> String {
    match source {
        Source::Microdata => "microdata".into(),
        _ => "json-ld".into(),
    }
}

/// A card in a listing: the element, its heading and the heading's link.
struct Card<'a> {
    node: Node<'a>,
    heading: Node<'a>,
    link: Node<'a>,
}

/// The page's largest group of cards, each led by a heading that links
/// somewhere, outside boilerplate: a blog's index, a jobs board, a list
/// of results. Three cards at least.
fn cards(root: Node<'_>) -> Option<(Node<'_>, Vec<Card<'_>>)> {
    // Linked headings outside boilerplate, with how many sit under each
    // element a few levels up.
    let mut headings: Vec<(Node<'_>, Node<'_>)> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() != NodeKind::Element && n.kind() != NodeKind::Document {
            continue;
        }
        if says_boilerplate(n) || crate::markdown::unseen(n, false) {
            continue;
        }
        if matches!(n.tag(), Some("h2" | "h3" | "h4"))
            && let Some(a) = std::iter::once(n)
                .chain(n.descendants())
                .chain(n.parent().filter(|p| p.tag() == Some("a")))
                .find(|d| d.tag() == Some("a") && goes_somewhere(*d))
        {
            headings.push((n, a));
            continue;
        }
        let at = stack.len();
        stack.extend(n.children());
        stack[at..].reverse();
    }
    let mut under: HashMap<NodeId, usize> = HashMap::new();
    for (h, _) in &headings {
        for a in std::iter::successors(h.parent(), |n| n.parent()).take(UP + 1) {
            *under.entry(a.id()).or_default() += 1;
        }
    }
    // Each heading's card: the highest element above it holding no other
    // linked heading.
    let mut groups: HashMap<(String, Option<NodeId>), Vec<Card<'_>>> = HashMap::new();
    let mut order: Vec<(String, Option<NodeId>)> = Vec::new();
    for (h, link) in headings {
        let mut card = h;
        for a in std::iter::successors(h.parent(), |n| n.parent()).take(UP) {
            if under.get(&a.id()).copied().unwrap_or(0) > 1
                || matches!(a.tag(), Some("body" | "html" | "main"))
            {
                break;
            }
            card = a;
        }
        // Cards of one listing share their tag and class, and sit under
        // one element (a grid's rows under one more).
        let shape = format!(
            "{}.{}",
            card.tag().unwrap_or_default(),
            card.attr("class").unwrap_or_default()
        );
        let key = (
            shape,
            card.parent().and_then(|p| p.parent()).map(|g| g.id()),
        );
        let group = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        if group.len() < MAX_CARDS {
            group.push(Card {
                node: card,
                heading: h,
                link,
            });
        }
    }
    let best = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .filter(|g| g.len() >= 3)
        .max_by_key(|g| g.len())?;
    let container = common_parent(&best)?;
    Some((container, best))
}

fn goes_somewhere(a: Node<'_>) -> bool {
    a.attr("href").map(str::trim).is_some_and(|h| {
        !h.is_empty() && !h.starts_with('#') && !h.to_ascii_lowercase().starts_with("javascript:")
    })
}

/// The smallest element holding every card.
fn common_parent<'a>(cards: &[Card<'a>]) -> Option<Node<'a>> {
    let first = cards.first()?.node;
    let mut above: Vec<Node<'a>> = std::iter::successors(first.parent(), |n| n.parent()).collect();
    for c in &cards[1..] {
        let mine: HashSet<NodeId> = std::iter::successors(c.node.parent(), |n| n.parent())
            .map(|n| n.id())
            .collect();
        above.retain(|a| mine.contains(&a.id()));
    }
    above.first().copied()
}

/// One card as a listed article.
fn card_article(page: &Page<'_, '_>, c: &Card<'_>, lang: Option<&str>) -> Map<String, Value> {
    let mut out = Map::new();
    let tidy = |t: String| {
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        (!t.is_empty()).then_some(t)
    };
    if let Some(h) = tidy(crate::markdown::clean_text(c.heading)) {
        out.insert("headline".into(), Value::String(h));
    }
    if let Some(u) = c.link.attr("href").and_then(|h| f::absolute(h, page.base)) {
        out.insert("url".into(), Value::String(u));
    }
    let time = c.node.descendants().find(|n| n.tag() == Some("time"));
    if let Some(t) = time {
        let raw = tidy(t.text());
        let when = t
            .attr("datetime")
            .map(|d| Value::String(d.to_string()))
            .or_else(|| raw.clone().map(Value::String));
        if let Some(iso) = when.and_then(|w| f::iso_date(&w, page.now)) {
            out.insert("date_published".into(), Value::String(iso));
        }
        if let Some(r) = raw {
            out.insert("date_published_raw".into(), Value::String(r));
        }
    }
    if let Some(name) = c
        .node
        .descendants()
        .filter(|n| n.kind() == NodeKind::Element)
        .find_map(crate::content::author_of)
    {
        out.insert(
            "authors".into(),
            json!([{"name": name.clone(), "name_raw": name}]),
        );
    }
    if let Some(src) = c
        .node
        .descendants()
        .filter(|n| n.tag() == Some("img"))
        .find_map(crate::content::lead_image)
        .and_then(|s| f::absolute(&s, page.base))
    {
        out.insert("main_image".into(), json!({ "url": src }));
    }
    // The teaser: the first paragraph that isn't the heading, the date or
    // the byline.
    let marker = |n: Node<'_>| {
        n.descendants()
            .any(|d| d.tag() == Some("time") || crate::content::author_of(d).is_some())
    };
    if let Some(p) = c.node.descendants().find(|n| {
        n.tag() == Some("p")
            && !marker(*n)
            && !std::iter::successors(Some(*n), |a| a.parent())
                .take_while(|a| a.id() != c.node.id())
                .any(|a| a.id() == c.heading.id())
    }) && let Some(t) = tidy(crate::markdown::clean_text(p))
    {
        out.insert("article_body".into(), Value::String(t));
    }
    if let Some(l) = lang {
        out.insert("in_language".into(), Value::String(l.to_string()));
    }
    out
}

/// Whether most cards show a price: a shop's grid, not a blog's.
fn priced(found: &[Card<'_>]) -> bool {
    let with_price = found
        .iter()
        .filter(|c| {
            c.node.descendants().any(|d| {
                d.kind() == NodeKind::Text
                    && d.data().is_some_and(|t| {
                        t.trim().len() <= 40
                            && crate::price::parse_price(t).is_some_and(|p| p.currency.is_some())
                    })
            })
        })
        .count();
    with_price * 2 > found.len()
}

/// Whether the page has an article of its own beside the cards: main
/// content that neither holds the cards nor sits inside them (a story
/// with "related stories" under it).
fn article_beside(root: Node<'_>, container: Node<'_>) -> bool {
    let Some(main) = crate::content::main_content(root) else {
        return false;
    };
    let holds = |outer: Node<'_>, inner: Node<'_>| {
        std::iter::successors(Some(inner), |n| n.parent()).any(|n| n.id() == outer.id())
    };
    !holds(main.node, container) && !holds(container, main.node)
}

pub(crate) fn read_article_list(page: &Page<'_, '_>, item: &mut Builder) {
    let mut probability = 0.05;
    if let Some((articles, source)) = listed_articles(page) {
        item.offer(
            "articles",
            Some(Value::Array(
                articles.into_iter().map(Value::Object).collect(),
            )),
            source,
        );
        probability = 0.9;
    } else if let Some((container, found)) = cards(page.root) {
        let lang = page_language(page);
        let articles: Vec<Value> = found
            .iter()
            .map(|c| Value::Object(card_article(page, c, lang.as_deref())))
            .collect();
        item.offer(
            "articles",
            Some(Value::Array(articles)),
            super::dom::source(container),
        );
        probability = if article_beside(page.root, container) || priced(&found) {
            0.3
        } else {
            0.7
        };
    }
    breadcrumbs(page, item);
    if let Some(c) = canonical(page) {
        item.offer("canonical_url", Some(Value::String(c.0)), c.1);
    }
    item.probability = probability;
}

fn breadcrumbs(page: &Page<'_, '_>, item: &mut Builder) {
    if let Some(list) = page
        .things
        .iter()
        .find(|t| t.types.iter().any(|ty| ty == "BreadcrumbList"))
    {
        item.offer(
            "breadcrumbs",
            f::breadcrumbs(&Value::Object(list.props.clone()), page.base),
            page.source(list, "itemListElement"),
        );
    }
    if let Some(trail) = super::dom::breadcrumbs(page.root) {
        item.offer(
            "breadcrumbs",
            f::from_element(f::FieldKind::Breadcrumbs, trail, page.base, page.now),
            super::dom::source(trail),
        );
    }
}

fn canonical(page: &Page<'_, '_>) -> Option<(String, String)> {
    let (key, c) = page
        .meta("canonical")
        .and_then(Value::as_str)
        .map(|c| ("canonical", c))
        .or_else(|| {
            page.meta("og:url")
                .and_then(Value::as_str)
                .map(|c| ("og:url", c))
        })?;
    Some((f::absolute(c, page.base)?, format!("meta:{key}")))
}

/// Which navigation: where a blog leads, or where a jobs board does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leads {
    Articles,
    Jobs,
}

pub(crate) fn read_navigation(page: &Page<'_, '_>, item: &mut Builder, leads: Leads) {
    let mut probability = 0.05;
    let mut listing = false;
    let link = |url: Option<&Value>, name: Option<&Value>| {
        let mut out = Map::new();
        out.insert("url".into(), url?.clone());
        if let Some(n) = name {
            out.insert("name".into(), n.clone());
        }
        Some(Value::Object(out))
    };
    let structured = match leads {
        Leads::Articles => listed_articles(page).map(|(list, source)| {
            let items = list
                .iter()
                .filter_map(|a| link(a.get("url"), a.get("headline")))
                .collect::<Vec<_>>();
            (items, source)
        }),
        Leads::Jobs => listed_jobs(page),
    };
    if let Some((items, source)) = structured.filter(|(i, _)| !i.is_empty()) {
        item.offer("items", Some(Value::Array(items)), source);
        probability = 0.9;
        listing = true;
    } else if let Some((container, found)) = cards(page.root) {
        let items: Vec<Value> = found
            .iter()
            .filter_map(|c| {
                let url = c
                    .link
                    .attr("href")
                    .and_then(|h| f::absolute(h, page.base))?;
                let name = f::text(&Value::String(crate::markdown::clean_text(c.heading)));
                link(Some(&Value::String(url)), name.map(Value::String).as_ref())
            })
            .collect();
        item.offer(
            "items",
            Some(Value::Array(items)),
            super::dom::source(container),
        );
        listing = true;
        // A board of cards looks like a blog's index: without structured
        // data, only the article reading is sure of itself.
        probability = match leads {
            _ if article_beside(page.root, container) || priced(&found) => 0.3,
            Leads::Articles => 0.7,
            Leads::Jobs => 0.35,
        };
    }
    if let Some((h1, name)) = page
        .root
        .descendants()
        .filter(|n| n.tag() == Some("h1") && !in_boilerplate(*n))
        .find_map(|n| Some((n, page_heading(n)?)))
    {
        item.offer(
            "category_name",
            Some(Value::String(name)),
            super::dom::source(h1),
        );
    }
    if listing {
        if let Some((url, text, source)) = super::list::next_link(page, true) {
            let mut request = Map::new();
            request.insert("url".into(), Value::String(url));
            if let Some(t) = text {
                request.insert("name".into(), Value::String(t));
            }
            item.offer("next_page", Some(Value::Object(request)), source);
        }
        if let Some((n, source)) = super::dom::page_number(page.root, page.page) {
            item.offer("page_number", Some(Value::from(n)), source);
        }
    }
    item.probability = probability;
}

/// The jobs a page's structured data lists: several JobPostings linking
/// elsewhere, or an ItemList of them.
fn listed_jobs(page: &Page<'_, '_>) -> Option<(Vec<Value>, String)> {
    let entry = |o: &Map<String, Value>| {
        let url = o
            .get("url")
            .and_then(Value::as_str)
            .and_then(|u| f::absolute(u, page.base))?;
        let mut out = Map::new();
        out.insert("url".into(), Value::String(url));
        if let Some(t) = o.get("title").or_else(|| o.get("name")).and_then(f::text) {
            out.insert("name".into(), Value::String(t));
        }
        Some(Value::Object(out))
    };
    for list in page
        .things
        .iter()
        .filter(|t| t.is("ItemList") && !t.is("BreadcrumbList"))
    {
        let items: Vec<Value> = each(list.props.get("itemListElement").unwrap_or(&Value::Null))
            .filter_map(Value::as_object)
            .map(|e| match e.get("item") {
                Some(Value::Object(inner)) => inner,
                _ => e,
            })
            .filter(|e| typed(e, "JobPosting"))
            .filter_map(entry)
            .collect();
        if items.len() >= 2 {
            return Some((items, label(list.source)));
        }
    }
    for source in [Source::JsonLd, Source::Microdata] {
        let jobs: Vec<&Thing> = of_type(&page.things, "JobPosting")
            .into_iter()
            .filter(|t| t.source == source)
            .collect();
        let linked = jobs.iter().all(|j| {
            j.props
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| !page.is_page_url(u))
        });
        if jobs.len() >= 2 && linked {
            let items: Vec<Value> = jobs.iter().filter_map(|t| entry(&t.props)).collect();
            return Some((items, label(source)));
        }
    }
    None
}

/// Thread types: a forum post, a social post, a question with answers.
const THREADS: [&str; 3] = ["DiscussionForumPosting", "SocialMediaPosting", "Question"];

pub(crate) fn read_thread(page: &Page<'_, '_>, item: &mut Builder) {
    let thread = page
        .things
        .iter()
        .find(|t| THREADS.iter().any(|ty| t.types.iter().any(|x| x == ty)));
    if let Some(t) = thread {
        let p = &t.props;
        let topic = p
            .get("headline")
            .or_else(|| p.get("name"))
            .and_then(f::text);
        item.offer(
            "topic",
            topic.map(|n| json!({ "name": n })),
            page.source(t, "headline"),
        );
        let mut posts = vec![post(page, p)];
        for key in ["acceptedAnswer", "suggestedAnswer", "comment"] {
            posts.extend(
                each(p.get(key).unwrap_or(&Value::Null))
                    .filter_map(Value::as_object)
                    .map(|c| post(page, c)),
            );
        }
        let posts: Vec<Value> = posts
            .into_iter()
            .filter(|p| p.contains_key("text"))
            .map(Value::Object)
            .collect();
        if !posts.is_empty() {
            item.offer("posts", Some(Value::Array(posts)), label(t.source));
        }
        item.probability = 0.9;
        return;
    }
    let mut probability = 0.05;
    if let Some((container, found)) = posts(page.root, page.now) {
        item.offer(
            "posts",
            Some(Value::Array(found.into_iter().map(Value::Object).collect())),
            super::dom::source(container),
        );
        probability = if article_beside(page.root, container) {
            0.25
        } else {
            0.65
        };
    }
    if let Some((h1, name)) = page
        .root
        .descendants()
        .filter(|n| n.tag() == Some("h1") && !in_boilerplate(*n))
        .find_map(|n| Some((n, page_heading(n)?)))
        .filter(|_| probability > 0.05)
    {
        item.offer(
            "topic",
            Some(json!({ "name": name })),
            super::dom::source(h1),
        );
    }
    item.probability = probability;
}

/// One post from a schema.org posting, comment or answer.
fn post(page: &Page<'_, '_>, p: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(t) = p
        .get("text")
        .or_else(|| p.get("articleBody"))
        .and_then(f::text)
    {
        out.insert("text".into(), Value::String(t));
    }
    if let Some(d) = p.get("datePublished").or_else(|| p.get("dateCreated")) {
        if let Some(iso) = f::iso_date(d, page.now) {
            out.insert("date_published".into(), Value::String(iso));
        }
        if let Some(raw) = f::text(d) {
            out.insert("date_published_raw".into(), Value::String(raw));
        }
    }
    let mut reactions = Map::new();
    for stat in
        each(p.get("interactionStatistic").unwrap_or(&Value::Null)).filter_map(Value::as_object)
    {
        let kind = stat
            .get("interactionType")
            .map(|t| match t {
                Value::Object(o) => o
                    .get("@type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                other => f::text(other).unwrap_or_default(),
            })
            .unwrap_or_default();
        let count = stat.get("userInteractionCount").and_then(|c| match c {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        });
        let field = if kind.ends_with("LikeAction") || kind.ends_with("AgreeAction") {
            "likes"
        } else if kind.ends_with("CommentAction") || kind.ends_with("ReplyAction") {
            "replies"
        } else {
            continue;
        };
        if let Some(c) = count.filter(|c| *c >= 0) {
            reactions.insert(field.into(), Value::from(c));
        }
    }
    if let Some(up) = p
        .get("upvoteCount")
        .and_then(Value::as_i64)
        .filter(|c| *c >= 0)
    {
        reactions.entry("likes").or_insert(Value::from(up));
    }
    if let Some(n) = p
        .get("commentCount")
        .or_else(|| p.get("answerCount"))
        .and_then(Value::as_i64)
        .filter(|c| *c >= 0)
    {
        reactions.entry("replies").or_insert(Value::from(n));
    }
    if !reactions.is_empty() {
        out.insert("reactions".into(), Value::Object(reactions));
    }
    out
}

/// The page's posts: its largest group of look-alike blocks, each with
/// one date or one byline and some text, outside boilerplate. Two at
/// least.
fn posts(root: Node<'_>, now: crate::date::Civil) -> Option<(Node<'_>, Vec<Map<String, Value>>)> {
    let mut marks: Vec<Node<'_>> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() != NodeKind::Element && n.kind() != NodeKind::Document {
            continue;
        }
        if says_boilerplate(n) || crate::markdown::unseen(n, false) {
            continue;
        }
        if n.tag() == Some("time") {
            marks.push(n);
            continue;
        }
        let at = stack.len();
        stack.extend(n.children());
        stack[at..].reverse();
    }
    let mut under: HashMap<NodeId, usize> = HashMap::new();
    for t in &marks {
        for a in std::iter::successors(t.parent(), |n| n.parent()).take(UP + 1) {
            *under.entry(a.id()).or_default() += 1;
        }
    }
    let mut groups: HashMap<(String, Option<NodeId>), Vec<Node<'_>>> = HashMap::new();
    let mut order = Vec::new();
    for t in marks {
        let mut block = t;
        for a in std::iter::successors(t.parent(), |n| n.parent()).take(UP) {
            if under.get(&a.id()).copied().unwrap_or(0) > 1
                || matches!(a.tag(), Some("body" | "html" | "main"))
            {
                break;
            }
            block = a;
        }
        let key = (
            format!(
                "{}.{}",
                block.tag().unwrap_or_default(),
                block.attr("class").unwrap_or_default()
            ),
            block.parent().map(|p| p.id()),
        );
        let g = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        if g.len() < MAX_CARDS {
            g.push(block);
        }
    }
    let best = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .filter(|g| g.len() >= 2)
        .map(|g| {
            g.into_iter()
                .filter_map(|b| read_post(b, now))
                .collect::<Vec<_>>()
        })
        .filter(|g: &Vec<(Node<'_>, Map<String, Value>)>| g.len() >= 2)
        .max_by_key(|g| g.len())?;
    let container = best[0].0.parent()?;
    Some((container, best.into_iter().map(|(_, p)| p).collect()))
}

/// A post block's text (its biggest block of text that isn't its date,
/// byline or buttons), date and likes; None when it has no text to speak
/// of.
fn read_post(block: Node<'_>, now: crate::date::Civil) -> Option<(Node<'_>, Map<String, Value>)> {
    let furniture = |n: Node<'_>| {
        n.descendants().any(|d| d.tag() == Some("time"))
            || words(n).any(|w| {
                [
                    "meta",
                    "author",
                    "byline",
                    "actions",
                    "footer",
                    "header",
                    "likes",
                    "reactions",
                ]
                .iter()
                .any(|x| w.eq_ignore_ascii_case(x))
            })
    };
    let body = block
        .children()
        .filter(|c| c.kind() == NodeKind::Element && !furniture(*c))
        .max_by_key(|c| c.text().trim().len())?;
    let text = crate::markdown::clean_text(body);
    let text = text
        .split("\n\n")
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.len() < 20 {
        return None;
    }
    let mut out = Map::new();
    out.insert("text".into(), Value::String(text));
    if let Some(t) = block.descendants().find(|n| n.tag() == Some("time")) {
        let raw = f::text(&Value::String(t.text()));
        let when = t
            .attr("datetime")
            .map(|d| Value::String(d.to_string()))
            .or_else(|| raw.clone().map(Value::String));
        if let Some(iso) = when.and_then(|w| f::iso_date(&w, now)) {
            out.insert("date_published".into(), Value::String(iso));
        }
        if let Some(r) = raw {
            out.insert("date_published_raw".into(), Value::String(r));
        }
    }
    let likes = block
        .descendants()
        .filter(|n| n.kind() == NodeKind::Element)
        .find(|n| {
            words(*n).any(|w| {
                ["likes", "like", "upvotes", "votes", "score", "points"]
                    .iter()
                    .any(|x| w.eq_ignore_ascii_case(x))
            })
        })
        .and_then(|n| {
            n.text()
                .split(|c: char| !c.is_ascii_digit())
                .find(|d| !d.is_empty())
                .and_then(|d| d.parse::<i64>().ok())
        });
    if let Some(l) = likes {
        out.insert("reactions".into(), json!({ "likes": l }));
    }
    Some((block, out))
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
