//! Any page's content: its main text and where that sits, its headings,
//! and its menus, breadcrumbs and pager kept apart from the text.

use serde_json::{Value, json};

use super::fields as f;
use super::{Builder, Page};
use crate::Node;

pub(crate) const FIELDS: &[&str] = &[
    "url",
    "canonical_url",
    "headline",
    "title",
    "item_main",
    "item_main_xpath",
    "breadcrumbs",
    "navigation_header",
    "navigation_footer",
    "navigation_sidebar",
    "pagination",
    "next_page",
];

/// The most links a menu keeps.
const MAX_LINKS: usize = 200;

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    let root = page.root;
    if let Some(c) = page.meta("canonical").and_then(Value::as_str) {
        item.offer(
            "canonical_url",
            f::absolute(c, page.base).map(Value::String),
            "meta:canonical".into(),
        );
    }
    let all: Vec<Node<'_>> = root.descendants().filter(|n| n.tag().is_some()).collect();
    if let Some(h1) = all.iter().find(|n| n.tag() == Some("h1")) {
        item.offer(
            "headline",
            words(*h1).map(Value::String),
            super::dom::source(*h1),
        );
    }
    item.offer(
        "headline",
        page.meta("og:title").and_then(f::text).map(Value::String),
        "meta:og:title".into(),
    );
    if let Some(t) = all.iter().find(|n| n.tag() == Some("title")) {
        item.offer("title", words(*t).map(Value::String), "title".into());
    }
    let main = crate::content::main_content(root).map(|m| m.node);
    let article = crate::content::article(root, page.base);
    item.offer(
        "item_main",
        article.text.map(Value::String),
        main.map_or("dom".into(), super::dom::source),
    );
    item.offer(
        "item_main_xpath",
        main.and_then(crate::css_path::xpath_path)
            .map(Value::String),
        "dom".into(),
    );
    if let Some(trail) = super::dom::breadcrumbs(root) {
        item.offer(
            "breadcrumbs",
            f::from_element(f::FieldKind::Breadcrumbs, trail, page.base, page.now),
            super::dom::source(trail),
        );
    }
    let inside = |n: Node<'_>, of: Node<'_>| {
        std::iter::successors(Some(n), |a| a.parent()).any(|a| a.id() == of.id())
    };
    let outside_main = |n: &&Node<'_>| main.is_none_or(|m| !inside(**n, m) && !inside(m, **n));
    let before_main = |n: &&Node<'_>| main.is_none_or(|m| n.order() < m.order());
    let header = all
        .iter()
        .filter(outside_main)
        .find(|n| n.tag() == Some("header"))
        .or_else(|| {
            all.iter()
                .filter(outside_main)
                .filter(before_main)
                .find(|n| n.tag() == Some("nav") && super::dom::breadcrumbs(**n).is_none())
        });
    let footer = all
        .iter()
        .rev()
        .filter(outside_main)
        .find(|n| n.tag() == Some("footer"));
    let asides: Vec<Node<'_>> = all
        .iter()
        .filter(outside_main)
        .filter(|n| n.tag() == Some("aside"))
        .copied()
        .collect();
    for (field, at) in [
        ("navigation_header", header.map(|h| vec![*h])),
        ("navigation_footer", footer.map(|h| vec![*h])),
        ("navigation_sidebar", (!asides.is_empty()).then_some(asides)),
    ] {
        let Some(at) = at else { continue };
        item.offer(field, links(&at, page), super::dom::source(at[0]));
    }
    if let Some(pager) = super::dom::pagers(root).next() {
        item.offer(
            "pagination",
            links(&[pager], page),
            super::dom::source(pager),
        );
    }
    if let Some((url, name, source)) = super::dom::next_page(root, page.page, page.base) {
        item.offer("next_page", Some(json!({"url": url, "name": name})), source);
    }
    item.probability = if main.is_some() { 1.0 } else { 0.5 };
}

/// `[{name, url}]` for the links under `within`, absolute, once each.
fn links(within: &[Node<'_>], page: &Page<'_, '_>) -> Option<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for a in within
        .iter()
        .flat_map(|w| w.descendants())
        .filter(|n| n.tag() == Some("a"))
    {
        let Some(href) = a.attr("href").map(str::trim) else {
            continue;
        };
        if href.is_empty()
            || href.starts_with('#')
            || href.to_ascii_lowercase().starts_with("javascript:")
        {
            continue;
        }
        let Some(url) = f::absolute(href, page.base) else {
            continue;
        };
        if seen.contains(&url) {
            continue;
        }
        seen.push(url.clone());
        out.push(json!({"name": words(a), "url": url}));
        if out.len() >= MAX_LINKS {
            break;
        }
    }
    (!out.is_empty()).then_some(Value::Array(out))
}

fn words(n: Node<'_>) -> Option<String> {
    let t = crate::markdown::clean_text(n);
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}
