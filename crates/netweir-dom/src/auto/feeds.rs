//! Pages of many things: lists of articles (ArticleList), where a blog
//! or a jobs board leads (ArticleNavigation, JobPostingNavigation), and
//! forum threads (ForumThread). Structured data first (a Blog's posts,
//! an ItemList, several JobPostings, a DiscussionForumPosting and its
//! comments), then the layout: a group of cards each led by a linked
//! heading, or a group of posts each with a date or a poster, most of
//! them naming who wrote them.

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

/// Whether the page has an article of its own in its structured data:
/// one whose URL is the page's, or whose headline is the page heading.
pub(crate) fn own_article(page: &Page<'_, '_>) -> bool {
    let heading = page
        .root
        .descendants()
        .filter(|n| n.tag() == Some("h1"))
        .find_map(page_heading);
    of_type(&page.things, "Article").iter().any(|a| {
        a.props
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| page.is_page_url(u))
            // An article with no URL whose headline is the page heading;
            // one linking elsewhere is an entry, whatever its headline.
            || (!a.props.contains_key("url")
                && heading.as_deref().is_some_and(|h| {
                    a.props.get("headline").and_then(f::text).as_deref() == Some(h)
                }))
    })
}

/// The articles a page's structured data lists, and where from: a Blog's
/// posts or an ItemList's articles (the page's own left out), or several
/// Articles on a page that has none of its own.
pub(crate) fn listed_articles(page: &Page<'_, '_>) -> Option<(Vec<Map<String, Value>>, String)> {
    let lang = page_language(page);
    let lang = lang.as_deref();
    let entries = |list: &[&Map<String, Value>]| -> Vec<Map<String, Value>> {
        list.iter()
            .filter(|a| {
                !a.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|u| page.is_page_url(u))
            })
            .map(|a| article_entry(page, a, lang))
            .filter(|e| e.contains_key("headline") || e.contains_key("url"))
            .collect()
    };
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
    // Several Articles count only when the page is known not to be one of
    // them: its own article, in any source, makes the rest its teasers.
    if page.page.is_none() || own_article(page) {
        return None;
    }
    for source in [Source::JsonLd, Source::Microdata] {
        let articles: Vec<&Thing> = of_type(&page.things, "Article")
            .into_iter()
            .filter(|t| t.source == source && !t.is("DiscussionForumPosting"))
            .collect();
        let linked = articles
            .iter()
            .all(|a| a.props.get("url").and_then(Value::as_str).is_some());
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
/// of results. Three cards at least. Each level of heading is tried on
/// its own, so a card's kicker (`<h4><a>Opinion</a></h4>` above its
/// `<h2>`) doesn't split it.
fn cards(root: Node<'_>) -> Option<(Node<'_>, Vec<Card<'_>>)> {
    let mut by_level: [Vec<(Node<'_>, Node<'_>)>; 3] = Default::default();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() != NodeKind::Element && n.kind() != NodeKind::Document {
            continue;
        }
        if says_boilerplate(n) || crate::markdown::unseen(n, false) {
            continue;
        }
        let level = match n.tag() {
            Some("h2") => Some(0),
            Some("h3") => Some(1),
            Some("h4") => Some(2),
            _ => None,
        };
        if let Some(level) = level
            && let Some(a) = std::iter::once(n)
                .chain(n.descendants())
                .chain(n.parent().filter(|p| p.tag() == Some("a")))
                .find(|d| d.tag() == Some("a") && goes_somewhere(*d))
        {
            // A heading naming a member is a post's byline, not a title.
            if !profile_link(a) {
                by_level[level].push((n, a));
            }
            continue;
        }
        let at = stack.len();
        stack.extend(n.children());
        stack[at..].reverse();
    }
    let mut best: Option<Vec<Card<'_>>> = None;
    for headings in by_level {
        if let Some(group) = card_group(headings)
            && best.as_ref().is_none_or(|b| group.len() > b.len())
        {
            best = Some(group);
        }
    }
    let best = best?;
    let container = common_parent(&best)?;
    Some((container, best))
}

/// Linked headings of one level grouped into cards: each heading's card is
/// the highest element above it holding no other, and cards of one
/// listing share their tag and sit under one element (a grid's rows under
/// one more), with classes alike.
fn card_group<'a>(headings: Vec<(Node<'a>, Node<'a>)>) -> Option<Vec<Card<'a>>> {
    let mut under: HashMap<NodeId, usize> = HashMap::new();
    for (h, _) in &headings {
        for a in std::iter::successors(h.parent(), |n| n.parent()).take(UP + 1) {
            *under.entry(a.id()).or_default() += 1;
        }
    }
    let mut groups: HashMap<(String, Option<NodeId>), Vec<Card<'a>>> = HashMap::new();
    let mut order = Vec::new();
    for (h, link) in headings {
        let card = block_above(h, UP, |a| under.get(&a.id()).copied().unwrap_or(0) > 1);
        let key = (
            card.tag().unwrap_or_default().to_string(),
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
    order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .map(alike)
        .filter(|g| g.len() >= 3)
        .max_by_key(|g| g.len())
}

/// The highest element above `n`, up to `up` levels, before one that
/// `stop` turns down (or `<main>`, `<body>`).
fn block_above<'a>(n: Node<'a>, up: usize, stop: impl Fn(Node<'_>) -> bool) -> Node<'a> {
    let mut block = n;
    for a in std::iter::successors(n.parent(), |p| p.parent()).take(up) {
        if stop(a) || matches!(a.tag(), Some("body" | "html" | "main")) {
            break;
        }
        block = a;
    }
    block
}

/// The members of a group whose classes look like the rest's, leaving out
/// classes that tell blocks apart (`post-12`, `bg2`, `odd`,
/// `category-news`).
fn alike<T: Blocked>(group: Vec<T>) -> Vec<T> {
    let classes = |n: Node<'_>| -> Vec<String> {
        let mut out: Vec<String> = n
            .attr("class")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .filter(|c| {
                !c.bytes().any(|b| b.is_ascii_digit())
                    && !matches!(
                        c.as_str(),
                        "odd"
                            | "even"
                            | "alt"
                            | "first"
                            | "last"
                            | "active"
                            | "sticky"
                            | "featured"
                            | "current"
                            | "selected"
                            | "new"
                            | "unread"
                            | "read"
                    )
                    && ![
                        "category-",
                        "tag-",
                        "format-",
                        "status-",
                        "type-",
                        "author-",
                    ]
                    .iter()
                    .any(|p| c.starts_with(p))
            })
            .collect();
        out.sort();
        out.dedup();
        out
    };
    // The classes most members have; a member needs half of them, so an
    // opening post with extra classes stays with its replies.
    let all: Vec<Vec<String>> = group.iter().map(|g| classes(g.block())).collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for c in all.iter().flatten() {
        *counts.entry(c.as_str()).or_default() += 1;
    }
    let core: Vec<&str> = counts
        .iter()
        .filter(|(_, n)| **n * 2 > group.len())
        .map(|(c, _)| *c)
        .collect();
    let keep: Vec<bool> = all
        .iter()
        .map(|mine| {
            let shared = core.iter().filter(|c| mine.iter().any(|m| m == *c)).count();
            shared * 2 >= core.len()
        })
        .collect();
    group
        .into_iter()
        .zip(keep)
        .filter_map(|(g, k)| k.then_some(g))
        .collect()
}

/// Something sitting in one block of a group.
trait Blocked {
    fn block(&self) -> Node<'_>;
}

impl Blocked for Card<'_> {
    fn block(&self) -> Node<'_> {
        self.node
    }
}

impl Blocked for Node<'_> {
    fn block(&self) -> Node<'_> {
        *self
    }
}

/// A link to a member's profile, as forum and comment software links its
/// posters: `/members/kit.1/`, `memberlist.php?u=2`, `/user/kit`,
/// `member.php?u=3`, `user?id=kit`.
fn profile_link(a: Node<'_>) -> bool {
    links_to(
        a,
        &[
            "members/",
            "member/",
            "users/",
            "user/",
            "memberlist.php",
            "member.php",
            "user?id=",
        ],
    )
}

/// Whether a link's path has a segment starting with one of `starts`,
/// relative (`members/kit.1/`, as XenForo 1 writes it against
/// `<base>`) or not.
fn links_to(a: Node<'_>, starts: &[&str]) -> bool {
    a.attr("href").is_some_and(|h| {
        let h = h.trim().to_ascii_lowercase();
        let path = h.split_once("://").map_or(h.as_str(), |(_, rest)| {
            rest.find('/').map_or("", |i| &rest[i..])
        });
        let path = path.trim_start_matches("./");
        starts
            .iter()
            .any(|s| path.starts_with(s) || path.contains(&format!("/{s}")))
    })
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
    // The teaser: the longest paragraph that isn't the heading, the date,
    // the byline or a label (`<p class="category">Opinion</p>`).
    let marker = |n: Node<'_>| {
        n.descendants()
            .any(|d| d.tag() == Some("time") || crate::content::author_of(d).is_some())
    };
    let label = |n: Node<'_>| {
        words(n).any(|w| {
            [
                "category", "kicker", "label", "tag", "tags", "eyebrow", "section", "meta",
            ]
            .iter()
            .any(|l| w.eq_ignore_ascii_case(l))
        })
    };
    if let Some(t) = c
        .node
        .descendants()
        .filter(|n| {
            n.tag() == Some("p")
                && !marker(*n)
                && !label(*n)
                && !std::iter::successors(Some(*n), |a| a.parent())
                    .take_while(|a| a.id() != c.node.id())
                    .any(|a| a.id() == c.heading.id())
        })
        .filter_map(|p| tidy(crate::markdown::clean_text(p)))
        .max_by_key(String::len)
    {
        out.insert("article_body".into(), Value::String(t));
    }
    if let Some(l) = lang {
        out.insert("in_language".into(), Value::String(l.to_string()));
    }
    out
}

/// Whether most cards have a date or a byline, as articles do and shop
/// grids, jobs boards and link menus don't.
fn dated(found: &[Card<'_>]) -> bool {
    let with = found
        .iter()
        .filter(|c| {
            c.node.descendants().any(|d| {
                d.tag() == Some("time")
                    || (d.kind() == NodeKind::Element && crate::content::author_of(d).is_some())
            })
        })
        .count();
    with * 2 >= found.len()
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

fn holds(outer: Node<'_>, inner: Node<'_>) -> bool {
    std::iter::successors(Some(inner), |n| n.parent()).any(|n| n.id() == outer.id())
}

/// Whether posts are the comments or reviews of something the page is
/// about: an article (its own in structured data or Open Graph, an
/// `<article>` with the heading and some prose outside the posts, or
/// main content beside them) or a product.
fn comments_of(page: &Page<'_, '_>, container: Node<'_>) -> bool {
    let product = page
        .meta("og:type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("product"));
    if product
        || own_article(page)
        || article_beside(page.root, container)
        || page.things.iter().any(|t| t.is("Product"))
        || super::dom::shows_product(page.root)
    {
        return true;
    }
    // Paragraphs between the heading and the posts that no poster
    // signs: the article the posts are comments on. A question's prose
    // sits in a block with its asker, so it doesn't count.
    let Some(h1) = page
        .root
        .descendants()
        .find(|n| n.tag() == Some("h1") && page_heading(*n).is_some())
    else {
        return false;
    };
    let Some(shared) = common_ancestor([h1, container].into_iter()) else {
        return false;
    };
    let (from, to) = (h1.order(), container.order());
    let mut prose = 0;
    for p in shared.descendants().filter(|n| n.tag() == Some("p")) {
        if p.order() <= from || p.order() >= to || holds(container, p) {
            continue;
        }
        let signed = std::iter::once(p)
            .chain(
                std::iter::successors(p.parent(), |a| a.parent())
                    .take_while(|a| a.id() != shared.id()),
            )
            .any(|a| a.descendants().any(poster));
        if !signed {
            prose += p.text().trim().len();
            if prose >= 400 {
                return true;
            }
        }
    }
    false
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
        } else if dated(&found) {
            0.7
        } else {
            0.35
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
            Leads::Articles if dated(&found) => 0.7,
            Leads::Articles => 0.35,
            Leads::Jobs => 0.35,
        };
    }
    if let Some((h1, name)) = page
        .root
        .descendants()
        .filter(|n| n.tag() == Some("h1") && !in_boilerplate(*n))
        .find_map(|n| Some((n, page_heading(n)?)))
        .filter(|_| listing)
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
    super::list::offer_sub_categories(page, item);
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

/// Comments read at most from one thread's structured data, and how deep
/// replies to replies go.
const MAX_POSTS: usize = 2000;
const MAX_REPLY_DEPTH: usize = 16;

pub(crate) fn read_thread(page: &Page<'_, '_>, item: &mut Builder) {
    let threads: Vec<&Thing> = page
        .things
        .iter()
        .filter(|t| THREADS.iter().any(|ty| t.types.iter().any(|x| x == ty)))
        .collect();
    // A thread that links to another page is an entry in a list of them.
    let here = |t: &&Thing| {
        t.props
            .get("url")
            .and_then(Value::as_str)
            .is_none_or(|u| page.is_page_url(u))
    };
    if let Some(t) = threads.iter().copied().find(here) {
        let p = &t.props;
        let (key, topic) = ["headline", "name"]
            .iter()
            .find_map(|k| Some((*k, p.get(*k).and_then(f::text)?)))
            .unzip();
        item.offer(
            "topic",
            topic.map(|n| json!({ "name": n })),
            page.source(t, key.unwrap_or("headline")),
        );
        let mut posts = vec![post(page, p)];
        replies(page, p, 0, &mut posts);
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
    if !threads.is_empty() {
        item.probability = 0.3;
        return;
    }
    let mut probability = 0.05;
    if let Some((container, found)) = posts(page.root, page.now) {
        item.offer(
            "posts",
            Some(Value::Array(found.into_iter().map(Value::Object).collect())),
            super::dom::source(container),
        );
        // Posts beside an article are its comments, wherever they sit;
        // dated teasers that the cards reading finds too are a list.
        let listed = cards(page.root).is_some_and(|(c, found)| {
            dated(&found)
                && (c.id() == container.id() || holds(c, container) || holds(container, c))
        });
        probability = if listed || comments_of(page, container) {
            0.25
        } else {
            0.75
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

/// A posting's answers and comments, and their comments in turn, in the
/// order written.
fn replies(
    page: &Page<'_, '_>,
    p: &Map<String, Value>,
    depth: usize,
    out: &mut Vec<Map<String, Value>>,
) {
    if depth >= MAX_REPLY_DEPTH {
        return;
    }
    for key in ["acceptedAnswer", "suggestedAnswer", "comment"] {
        for c in each(p.get(key).unwrap_or(&Value::Null)).filter_map(Value::as_object) {
            if out.len() >= MAX_POSTS {
                return;
            }
            out.push(post(page, c));
            replies(page, c, depth + 1, out);
        }
    }
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

/// A post's poster: an element marked as the author or the user, or a
/// link to a member's profile.
fn poster(n: Node<'_>) -> bool {
    n.kind() == NodeKind::Element
        && ((n.tag() == Some("a") && member_link(n))
            || n.attr("itemprop")
                .is_some_and(|p| p.split_whitespace().any(|w| w == "author"))
            || n.attr("rel")
                .is_some_and(|r| r.split_whitespace().any(|w| w == "author"))
            || words(n).any(|w| {
                ["author", "username", "poster", "byline"]
                    .iter()
                    .any(|x| w.eq_ignore_ascii_case(x))
            }))
}

/// A link to a member's profile, the forms forum and comment software
/// use beyond [`profile_link`]'s.
fn member_link(a: Node<'_>) -> bool {
    profile_link(a) || links_to(a, &["profile/", "u/"])
}

/// A date element: `<time>`, or GitHub's `<relative-time>`.
fn is_date(n: Node<'_>) -> bool {
    matches!(n.tag(), Some("time" | "relative-time"))
}

/// A date written in an element classed as one, as forums without
/// `<time>` write it: `<span class="date">Oct 21, 2026, 10:00 AM</span>`,
/// or a timestamp in its `title`.
fn date_text(n: Node<'_>, now: crate::date::Civil) -> Option<(String, String)> {
    if is_date(n) {
        let raw = f::text(&Value::String(n.text())).unwrap_or_default();
        let when = n
            .attr("datetime")
            .map(str::to_string)
            .unwrap_or_else(|| raw.clone());
        return Some((f::iso_date(&Value::String(when), now)?, raw));
    }
    let classed = words(n).any(|w| {
        [
            "date",
            "postdate",
            "timestamp",
            "relativetime",
            "published",
            "posted",
            "age",
        ]
        .iter()
        .any(|x| w.eq_ignore_ascii_case(x))
    });
    if !classed || n.descendants().any(is_date) {
        return None;
    }
    let raw = f::text(&Value::String(n.text()))?;
    if raw.len() > 60 {
        return None;
    }
    let iso = n
        .attr("title")
        .and_then(|t| f::iso_date(&Value::String(t.to_string()), now))
        .or_else(|| f::iso_date(&Value::String(raw.clone()), now))?;
    Some((iso, raw))
}

/// Whether a poster or date sits in the prose rather than heading the
/// post: an @mention, or inside a table, or in a paragraph or item with
/// a sentence's worth of other text.
fn inline(n: Node<'_>) -> bool {
    let own = n.text();
    if own.trim_start().starts_with('@') {
        return true;
    }
    let words = |t: &str| t.split_whitespace().map(str::len).sum::<usize>();
    let own = words(&own);
    std::iter::successors(n.parent(), |a| a.parent())
        .take(4)
        .any(|a| match a.tag() {
            Some("table") => true,
            Some("p" | "dd") => words(&a.text()).saturating_sub(own) >= 60,
            _ => false,
        })
}

/// Whether an element is boilerplate for reading posts: comment
/// sections are boilerplate beside an article but they're the posts
/// here, so only boilerplate that isn't a comment section counts.
fn not_posts(n: Node<'_>) -> bool {
    says_boilerplate(n)
        && !words(n).any(|w| {
            let w = w.to_ascii_lowercase();
            ["comment", "repl", "discussion", "thing"]
                .iter()
                .any(|x| w.contains(x))
        })
}

/// Classes and ids of the parts of a post that aren't what it says.
const FURNITURE: [&str; 21] = [
    "meta",
    "byline",
    "actions",
    "footer",
    "header",
    "likes",
    "reactions",
    "signature",
    "attribution",
    "avatar",
    "userinfo",
    "postprofile",
    "buttons",
    "votecell",
    "postfoot",
    "nodecontrols",
    "notice",
    "attachbox",
    "attachments",
    "attachment",
    "sig",
];

fn furniture(n: Node<'_>) -> bool {
    matches!(
        n.tag(),
        Some("button" | "header" | "footer" | "nav" | "img" | "select")
    ) || words(n).any(|w| FURNITURE.iter().any(|x| w.eq_ignore_ascii_case(x)))
}

/// What an element holds, worked out once for the whole page.
#[derive(Default, Clone, Copy)]
struct Holds {
    poster: bool,
    date: bool,
    furniture: bool,
    text: usize,
}

/// Every element's [`Holds`], in one walk: posters and dates count
/// unless they're inside a quote or the prose.
fn holdings(root: Node<'_>, now: crate::date::Civil) -> HashMap<NodeId, Holds> {
    let mut order: Vec<(Node<'_>, bool)> = Vec::new();
    let mut stack: Vec<(Node<'_>, bool)> = vec![(root, false)];
    while let Some((n, quoted)) = stack.pop() {
        match n.kind() {
            NodeKind::Element | NodeKind::Document => {
                // Discourse gives crawlers its posts inside <noscript>.
                if not_posts(n) || crate::markdown::unseen(n, true) {
                    continue;
                }
                let quoted = quoted || n.tag() == Some("blockquote");
                order.push((n, quoted));
                stack.extend(n.children().map(|c| (c, quoted)));
            }
            NodeKind::Text => order.push((n, quoted)),
            _ => {}
        }
    }
    let mut out: HashMap<NodeId, Holds> = HashMap::new();
    for (n, quoted) in order.into_iter().rev() {
        let mut h = out.remove(&n.id()).unwrap_or_default();
        if n.kind() == NodeKind::Text {
            h.text = n.data().unwrap_or_default().trim().len();
        } else if !quoted {
            // Asking whether a mark is inline reads text round it, so
            // only marks are asked.
            let is_poster = poster(n);
            let is_date = date_text(n, now).is_some();
            if (is_poster || is_date) && !inline(n) {
                h.poster |= is_poster;
                h.date |= is_date;
            }
            h.furniture |= furniture(n);
        }
        if let Some(p) = n.parent() {
            let up = out.entry(p.id()).or_default();
            up.poster |= h.poster;
            up.date |= h.date;
            up.furniture |= h.furniture;
            up.text = up.text.saturating_add(h.text);
        }
        if n.kind() != NodeKind::Text {
            out.insert(n.id(), h);
        }
    }
    out
}

/// An element's tag and its classes, leaving out classes that tell one
/// block from the next (`post-12`, `bg2`, `odd`, `category-news`).
fn shape(n: Node<'_>) -> (String, Vec<String>) {
    let mut classes: Vec<String> = n
        .attr("class")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .filter(|c| {
            !c.bytes().any(|b| b.is_ascii_digit())
                && !matches!(
                    c.as_str(),
                    "odd"
                        | "even"
                        | "alt"
                        | "first"
                        | "last"
                        | "active"
                        | "sticky"
                        | "featured"
                        | "current"
                        | "selected"
                        | "new"
                        | "unread"
                        | "read"
                        | "unvoted"
                        | "likes"
                        | "dislikes"
                )
                && ![
                    "category-",
                    "tag-",
                    "format-",
                    "status-",
                    "type-",
                    "author-",
                ]
                .iter()
                .any(|p| c.starts_with(p))
        })
        .collect();
    classes.sort();
    classes.dedup();
    (n.tag().unwrap_or_default().to_string(), classes)
}

/// The page's posts: look-alike sibling blocks, each with a poster or a
/// date heading some text, most with a poster, few led by a title
/// linking elsewhere, and dated or with some prose (a member directory
/// names people too). Blocks of one classed shape under different
/// parents are one thread, replies to replies included; classless ones
/// count only side by side. Two at least.
fn posts(root: Node<'_>, now: crate::date::Civil) -> Option<(Node<'_>, Vec<Map<String, Value>>)> {
    let holds = holdings(root, now);
    let get = |n: Node<'_>| holds.get(&n.id()).copied().unwrap_or_default();
    // Candidate blocks by shape: classed shapes across the page,
    // classless ones per parent.
    let mut groups: HashMap<(String, Vec<String>, Option<NodeId>), Vec<Node<'_>>> = HashMap::new();
    let mut order = Vec::new();
    let mut anchor: HashMap<NodeId, Option<NodeId>> = HashMap::new();
    for n in std::iter::once(root).chain(root.descendants()) {
        if n.kind() != NodeKind::Element || !holds.contains_key(&n.id()) {
            continue;
        }
        let h = get(n);
        if !(h.poster || h.date) || h.text < 20 {
            continue;
        }
        let (tag, classes) = shape(n);
        if matches!(
            tag.as_str(),
            "html" | "body" | "main" | "a" | "span" | "time" | "p"
        ) {
            continue;
        }
        // Classless blocks count side by side, and with the blocks of
        // their tag nested in them (replies in `<li><ul><li>`): they share
        // the outermost one's parent. Custom elements (`<shreddit-comment>`)
        // name what they are, as a class would.
        let parent = if classes.is_empty() && !tag.contains('-') {
            let nested = std::iter::successors(n.parent(), |a| a.parent())
                .take(12)
                .find(|a| a.tag() == Some(tag.as_str()) && anchor.contains_key(&a.id()));
            let at = match nested {
                Some(a) => anchor[&a.id()],
                None => n.parent().map(|p| p.id()),
            };
            anchor.insert(n.id(), at);
            at
        } else {
            None
        };
        let key = (tag, classes, parent);
        let g = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        if g.len() < MAX_CARDS {
            g.push(n);
        }
    }
    // An opening post's extra classes (`post original-post`) keep it with
    // the replies: a shape whose classes hold another's joins it.
    // Classed shapes only, compared within their tag.
    let mut by_tag: HashMap<&str, Vec<_>> = HashMap::new();
    for key in order.iter().filter(|k| !k.1.is_empty()) {
        by_tag.entry(key.0.as_str()).or_default().push(key.clone());
    }
    for shapes in by_tag.values() {
        if shapes.len() > 200 {
            continue;
        }
        for key in shapes {
            for other in shapes {
                if other != key
                    && other.1.len() > key.1.len()
                    && key.1.iter().all(|c| other.1.contains(c))
                    && other.2 == key.2
                    && groups.contains_key(key)
                    && let Some(extra) = groups.remove(other)
                {
                    let g = groups.get_mut(key).expect("checked");
                    g.extend(extra);
                    g.sort_by_key(|n| n.order());
                }
            }
        }
    }
    let mut best: Option<Vec<(Node<'_>, Map<String, Value>)>> = None;
    for key in order {
        let Some(members) = groups.remove(&key) else {
            continue;
        };
        if members.len() < 2 {
            continue;
        }
        // A block inside another of its shape is a reply to it; one that
        // holds the whole group is a wrapper, not a post.
        let ids: HashSet<NodeId> = members.iter().map(|m| m.id()).collect();
        let signed = members.iter().filter(|m| get(**m).poster).count();
        if signed * 2 < members.len() {
            continue;
        }
        let read: Vec<(Node<'_>, Map<String, Value>, bool)> = members
            .iter()
            .filter_map(|m| read_post(*m, &ids, &holds, now))
            .collect();
        if read.len() < 2 || read.iter().filter(|(_, _, titled)| *titled).count() * 2 >= read.len()
        {
            continue;
        }
        let dated = read
            .iter()
            .filter(|(_, p, _)| p.contains_key("date_published"))
            .count();
        let prose = read
            .iter()
            .filter(|(_, p, _)| p["text"].as_str().is_some_and(|t| t.len() >= 80))
            .count();
        // Undated, they need prose and members' profile links: a
        // member directory has the links, testimonials and author pages
        // have the prose, a thread has both.
        let linked = read
            .iter()
            .filter(|(n, _, _)| {
                n.descendants()
                    .any(|d| d.tag() == Some("a") && member_link(d))
            })
            .count();
        if dated * 2 < read.len() && (prose * 2 < read.len() || linked * 2 < read.len()) {
            continue;
        }
        let found: Vec<(Node<'_>, Map<String, Value>)> =
            read.into_iter().map(|(n, p, _)| (n, p)).collect();
        if best.as_ref().is_none_or(|b| found.len() > b.len()) {
            best = Some(found);
        }
    }
    let best = best?;
    let container = common_ancestor(best.iter().map(|(n, _)| *n))?;
    Some((container, best.into_iter().map(|(_, p)| p).collect()))
}

/// The smallest element holding all of `nodes`.
fn common_ancestor<'a>(nodes: impl Iterator<Item = Node<'a>>) -> Option<Node<'a>> {
    let mut above: Option<Vec<Node<'a>>> = None;
    for n in nodes {
        let mine: Vec<Node<'a>> = std::iter::successors(n.parent(), |p| p.parent()).collect();
        above = Some(match above {
            None => mine,
            Some(prev) => {
                let ids: HashSet<NodeId> = mine.iter().map(|m| m.id()).collect();
                prev.into_iter().filter(|p| ids.contains(&p.id())).collect()
            }
        });
    }
    above?.first().copied()
}

/// A post block read: its text (its biggest clean block of text, with
/// the clean blocks beside it), date and likes, and whether a title
/// linking elsewhere comes before the text, as on a teaser. Clean means
/// holding no poster, date or furniture and no reply. None when it has
/// no text to speak of.
fn read_post<'a>(
    block: Node<'a>,
    members: &HashSet<NodeId>,
    holds: &HashMap<NodeId, Holds>,
    now: crate::date::Civil,
) -> Option<(Node<'a>, Map<String, Value>, bool)> {
    let get = |n: Node<'_>| holds.get(&n.id()).copied().unwrap_or_default();
    // Replies inside this post, and what holds them.
    // The post's own elements (replies inside it left out), each with
    // its depth and whether it's inside a quote.
    let mut has_reply: HashSet<NodeId> = HashSet::new();
    let mut walk: Vec<(Node<'a>, usize, bool)> = block.children().map(|c| (c, 1, false)).collect();
    let mut inside: Vec<(Node<'a>, usize, bool)> = Vec::new();
    while let Some((n, depth, quoted)) = walk.pop() {
        if n.kind() != NodeKind::Element || !holds.contains_key(&n.id()) {
            continue;
        }
        if members.contains(&n.id()) {
            for a in std::iter::successors(n.parent(), |p| p.parent()) {
                if a.id() == block.id() || !has_reply.insert(a.id()) {
                    break;
                }
            }
            has_reply.insert(n.id());
            continue;
        }
        let quoted = quoted || n.tag() == Some("blockquote");
        inside.push((n, depth, quoted));
        walk.extend(n.children().map(|c| (c, depth + 1, quoted)));
    }
    let clean = |n: Node<'_>| {
        let h = get(n);
        !h.poster && !h.date && !h.furniture && !has_reply.contains(&n.id())
    };
    let candidates: Vec<(Node<'a>, usize)> = inside
        .iter()
        .filter(|(n, _, _)| clean(*n) && !has_reply.contains(&n.id()))
        .map(|(n, d, _)| (*n, *d))
        .collect();
    let most = candidates.iter().map(|(n, _)| get(*n).text).max()?;
    if most < 20 {
        return None;
    }
    // The deepest block holding nearly all of it: the post's own text
    // without a title or label wrapped round it.
    let mut body = candidates
        .iter()
        .filter(|(n, _)| get(*n).text * 10 >= most * 8)
        .max_by_key(|(_, depth)| *depth)?
        .0;
    // A reply under a long quote: the quote isn't the post, the block
    // holding the quote and the reply is. (A blockquote that is the body,
    // as vBulletin writes posts, stays the body.)
    if let Some(quote) = std::iter::successors(body.parent(), |p| p.parent())
        .take_while(|p| p.id() != block.id())
        .filter(|p| p.tag() == Some("blockquote"))
        .last()
        && let Some(up) = quote.parent().filter(|p| p.id() != block.id() && clean(*p))
    {
        body = up;
    }
    let tidy = |n: Node<'_>| {
        crate::markdown::clean_text(n)
            .split("\n\n")
            .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    // Clean paragraphs on either side of it belong to it.
    let part = |n: Node<'_>| match n.kind() {
        NodeKind::Text => {
            Some(n.data().unwrap_or_default().trim().to_string()).filter(|t| !t.is_empty())
        }
        // Not a heading ("Re: Flicker") and not a row of links (Reply,
        // Quote, Report).
        NodeKind::Element
            if clean(n)
                && holds.contains_key(&n.id())
                && !matches!(n.tag(), Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
                && n.descendants().any(|d| {
                    d.kind() == NodeKind::Text
                        && !d.data().unwrap_or_default().trim().is_empty()
                        && !std::iter::successors(d.parent(), |p| p.parent())
                            .take_while(|p| p.id() != n.id())
                            .chain(std::iter::once(n))
                            .any(|p| p.tag() == Some("a"))
                }) =>
        {
            Some(tidy(n)).filter(|t| !t.is_empty())
        }
        _ => None,
    };
    let mut before: Vec<String> = Vec::new();
    for s in std::iter::successors(body.prev_sibling(), |s| s.prev_sibling()) {
        if s.kind() == NodeKind::Text && s.data().unwrap_or_default().trim().is_empty() {
            continue;
        }
        match part(s) {
            Some(t) => before.push(t),
            None => break,
        }
    }
    before.reverse();
    let mut parts = before;
    parts.push(tidy(body));
    for s in std::iter::successors(body.next_sibling(), |s| s.next_sibling()) {
        if s.kind() == NodeKind::Text && s.data().unwrap_or_default().trim().is_empty() {
            continue;
        }
        match part(s) {
            Some(t) => parts.push(t),
            None => break,
        }
    }
    let text = parts.join("\n\n");
    let mut out = Map::new();
    out.insert("text".into(), Value::String(text));
    inside.sort_by_key(|(n, _, _)| n.order());
    let ordered: Vec<Node<'a>> = inside.iter().map(|(n, _, _)| *n).collect();
    // The post's date: its first, not a quote's and not one in the prose
    // (which is asked last, as it reads text round the date).
    if let Some((iso, raw)) = inside
        .iter()
        .filter(|(_, _, quoted)| !quoted)
        .find_map(|(n, _, _)| date_text(*n, now).filter(|_| !inline(*n)))
    {
        out.insert("date_published".into(), Value::String(iso));
        out.insert("date_published_raw".into(), Value::String(raw));
    }
    let likes = ordered
        .iter()
        .filter(|n| {
            words(**n).any(|w| {
                [
                    "likes", "like", "upvotes", "votes", "vote", "score", "points",
                ]
                .iter()
                .any(|x| w.eq_ignore_ascii_case(x))
            }) || n
                .attr("itemprop")
                .is_some_and(|p| p.trim().eq_ignore_ascii_case("upvoteCount"))
        })
        .find_map(|n| count(&n.text()));
    if let Some(l) = likes {
        out.insert("reactions".into(), json!({ "likes": l }));
    }
    // A title before the text: a linked heading, or a link of a few
    // words, that isn't the poster or the post's own date.
    let titled = ordered
        .iter()
        .filter(|n| n.order() < body.order())
        .any(|n| {
            let link_ok = |a: Node<'_>| {
                a.tag() == Some("a")
                    && goes_somewhere(a)
                    && !member_link(a)
                    && !poster(a)
                    && !a.descendants().any(|d| date_text(d, now).is_some())
                    && !std::iter::successors(a.parent(), |p| p.parent())
                        .take_while(|p| p.id() != block.id())
                        .any(|p| poster(p) || date_text(p, now).is_some())
            };
            match n.tag() {
                Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6") => n.descendants().any(link_ok),
                Some("a") => link_ok(*n) && n.text().split_whitespace().count() >= 3,
                _ => false,
            }
        });
    // A teaser ends with its way to the whole story.
    let more = ordered.iter().any(|n| {
        n.tag() == Some("a") && {
            let t = n.text().to_lowercase();
            let t = t.trim();
            [
                "read more",
                "continue reading",
                "read the full",
                "full story",
                "read on",
            ]
            .iter()
            .any(|m| t.starts_with(m))
        }
    });
    Some((block, out, titled || more))
}

/// A count as people write it: `12`, `1,204`, `1.2k`, `3M`; None for a
/// negative score, which no count of likes is.
fn count(text: &str) -> Option<i64> {
    let t = text.trim();
    let start = t.find(|c: char| c.is_ascii_digit())?;
    if t[..start].trim_end().ends_with(['-', '−']) {
        return None;
    }
    let rest = &t[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(rest.len());
    let number: f64 = rest[..end].replace(',', "").parse().ok()?;
    let scale = match rest[end..]
        .trim_start()
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
    {
        Some('k') => 1_000.0,
        Some('m') => 1_000_000.0,
        _ => 1.0,
    };
    let n = (number * scale).round();
    (n.is_finite() && n < 1e15).then_some(n as i64)
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
