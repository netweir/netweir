//! Search engine results pages: the organic results, read from the
//! page's layout (no engine marks them up), with the query and the
//! count of results in `metadata`.

use serde_json::{Value, json};
use url::Url;

use super::fields as f;
use super::{Builder, Page};
use crate::Node;

pub(crate) const FIELDS: &[&str] = &["url", "organic_results", "page_number", "ai_overview"];

/// URL parameters engines put the query in.
const QUERY: [&str; 5] = ["q", "query", "p", "text", "search_query"];

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    let searched = page.page.and_then(|u| {
        u.query_pairs()
            .find(|(k, v)| QUERY.contains(&k.as_ref()) && !v.trim().is_empty())
            .map(|(_, v)| v.trim().to_string())
    });
    let displayed = displayed_query(page.root);
    // A query in the address counts when the page shows it back, in its
    // search box or its title: `?p=123` on a blog names a post, not a
    // search.
    let title = page
        .root
        .descendants()
        .find(|n| n.tag() == Some("title"))
        .and_then(words)
        .map(|t| t.to_lowercase());
    let searched = searched.filter(|q| {
        let q = q.to_lowercase();
        displayed.as_ref().is_some_and(|d| d.to_lowercase() == q)
            || title.as_ref().is_some_and(|t| t.contains(&q))
    });
    let Some((container, cards)) = super::feeds::cards(page.root) else {
        item.probability = 0.05;
        return;
    };
    let host = page
        .page
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    let results: Vec<(Node<'_>, Node<'_>, String)> = cards
        .iter()
        .filter_map(|c| {
            let href = c.link.attr("href")?;
            let url = target(href, page.base, host.as_deref())?;
            Some((c.node, c.heading, url))
        })
        .collect();
    // Results lead to other sites: a shop's or a blog's own search
    // leads back into the site.
    let away = results
        .iter()
        .filter(|(_, _, u)| {
            Url::parse(u)
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                != host
        })
        .count();
    if away < 3 || away * 3 < cards.len() * 2 {
        item.probability = 0.05;
        return;
    }
    // A result leads elsewhere; the engine's own links among them
    // (its other tabs, its help) aren't results.
    let organic: Vec<Value> = results
        .iter()
        .filter(|(_, _, u)| {
            Url::parse(u)
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                != host
        })
        .enumerate()
        .map(|(i, (card, heading, url))| {
            let cite = card.descendants().find(|n| n.tag() == Some("cite"));
            json!({
                "name": words(*heading),
                "url": url,
                "description": snippet(*card, *heading),
                "rank": i + 1,
                "displayed_url_text": cite.and_then(words),
                "guessed_origin": null,
                "guessed_url": null,
            })
        })
        .collect();
    let count = organic.len();
    item.offer(
        "organic_results",
        Some(Value::Array(organic)),
        super::dom::source(container),
    );
    let number = super::dom::page_number(page.root, page.page)
        .map(|(n, _)| n)
        .or_else(|| page.page.and_then(|u| from_offset(u, count)));
    item.offer("page_number", number.map(Value::from), "page".into());
    item.meta("searched_query", json!(searched));
    item.meta("displayed_query", json!(displayed));
    item.meta("total_organic_results", json!(total(page.root)));
    if searched.is_some() {
        item.probability = 0.85;
    } else {
        // Links to other sites under headings, and nothing saying what
        // was searched for: a directory or a links page as likely.
        item.probability = 0.4;
        item.type_cap = Some(0.35);
    }
}

/// Where a result's link goes: the link itself, or, for an engine's own
/// redirect (`/url?q=https://...`), the address it carries.
fn target(href: &str, base: Option<&Url>, host: Option<&str>) -> Option<String> {
    let abs = f::absolute(href, base)?;
    let url = Url::parse(&abs).ok()?;
    if url.host_str().map(str::to_ascii_lowercase).as_deref() == host {
        let carried = url
            .query_pairs()
            .find(|(k, _)| matches!(k.as_ref(), "url" | "q" | "u" | "uddg"))
            .and_then(|(_, v)| Url::parse(&v).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"));
        if let Some(u) = carried {
            return Some(u.to_string());
        }
    }
    matches!(url.scheme(), "http" | "https").then_some(abs)
}

/// The query in the page's search box.
fn displayed_query(root: Node<'_>) -> Option<String> {
    root.descendants().find_map(|n| {
        let name = n.attr("name").unwrap_or_default();
        match n.tag()? {
            "input"
                if n.attr("type")
                    .is_some_and(|t| t.eq_ignore_ascii_case("search"))
                    || QUERY.contains(&name) =>
            {
                n.attr("value")
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            }
            "textarea" if QUERY.contains(&name) => words(n),
            _ => None,
        }
    })
}

/// The result's excerpt: the longest block of text in its card outside
/// its heading, its links and its displayed address. A card's first few
/// hundred nodes are read, and each block's first few hundred
/// characters, so a card built to be deep costs little.
fn snippet(card: Node<'_>, heading: Node<'_>) -> Option<String> {
    let mut best: Option<String> = None;
    let mut stack = vec![card];
    let mut visited = 0;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > 400 {
            break;
        }
        if n.id() == heading.id() || matches!(n.tag(), Some("a" | "cite" | "script" | "style")) {
            continue;
        }
        if n.kind() == crate::NodeKind::Text && !n.data().unwrap_or_default().trim().is_empty() {
            // The block the text sits in, said whole if it's short.
            let block = std::iter::successors(n.parent(), |p| p.parent())
                .take(4)
                .find(|p| {
                    matches!(
                        p.tag(),
                        Some("p" | "div" | "span" | "li" | "td" | "section")
                    )
                })
                .unwrap_or(card);
            let text = if block.id() == card.id() {
                n.data().map(str::to_string)
            } else {
                super::dom::short_text(block, 600).or_else(|| n.data().map(str::to_string))
            };
            let text = text.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "));
            if let Some(t) = text.filter(|t| !t.is_empty())
                && best.as_ref().is_none_or(|b| t.len() > b.len())
            {
                best = Some(t);
            }
            continue;
        }
        let at = stack.len();
        stack.extend(n.children());
        stack[at..].reverse();
    }
    best
}

/// The page number from `start=10` or `first=11` and the results a page
/// shows.
fn from_offset(url: &Url, per_page: usize) -> Option<i64> {
    let per = per_page.max(1) as i64;
    url.query_pairs().find_map(|(k, v)| {
        let v: i64 = v.trim().parse().ok()?;
        match k.as_ref() {
            "start" => Some(v / per + 1),
            "first" => Some((v - 1).max(0) / per + 1),
            _ => None,
        }
    })
}

/// The count of results the page states: `About 1,230 results`.
fn total(root: Node<'_>) -> Option<i64> {
    static COUNT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(\d[\d,.\u{a0} ]*)\s+results\b").unwrap()
    });
    root.descendants()
        .filter(|n| n.tag().is_some() && n.children().all(|c| c.tag().is_none()))
        .filter_map(words)
        .filter(|t| t.len() <= 80)
        .find_map(|t| {
            let c = COUNT.captures(&t)?;
            c[1].chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
}

fn words(n: Node<'_>) -> Option<String> {
    let t = crate::markdown::clean_text(n);
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}
