//! Reading items from a page's layout, for pages whose structured data
//! leaves fields out or that have none.
//!
//! A product page shows its name as the page's heading and its price
//! close to it, with a struck price beside it when it's on sale; a
//! category page shows cards, each a link with a price. Prices are found
//! first, then the cards they sit in, so a product page's strip of
//! related products is never read as the product.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use url::Url;

use super::fields::{self as f, FieldKind, PriceFields};
use super::{Builder, Page};
use crate::content::{in_boilerplate, lead_image, page_heading, words};
use crate::{Node, NodeId, NodeKind};

/// Text pieces looked at for prices at most; past it, the rest of the
/// page isn't read for them.
const MAX_PRICES: usize = 2000;

/// Levels above a price searched for the card holding it.
const CARD_DEPTH: usize = 8;

/// A price the page shows: the element holding it and its reading.
pub(crate) struct Shown<'a> {
    pub node: Node<'a>,
    pub money: PriceFields,
    /// Struck through, or marked as the old price.
    pub struck: bool,
}

/// The prices on a page and the cards that hold them.
pub(crate) struct Layout<'a> {
    pub shown: Vec<Shown<'a>>,
    /// Groups of cards that look alike, each with most members priced.
    pub groups: Vec<Vec<Node<'a>>>,
    member: HashSet<NodeId>,
}

impl<'a> Layout<'a> {
    pub fn of(root: Node<'a>) -> Layout<'a> {
        let shown = prices(root);
        let linked = linked(root);
        // The nearest element above each price that holds a link, and
        // the elements that look like it: the cards of a listing.
        let mut group_of: HashMap<NodeId, usize> = HashMap::new();
        let mut groups: Vec<Vec<Node<'a>>> = Vec::new();
        let mut priced: Vec<HashSet<NodeId>> = Vec::new();
        for s in shown.iter().filter(|s| !s.struck) {
            let holder = std::iter::successors(Some(s.node), |n| n.parent())
                .take(CARD_DEPTH)
                .take_while(|n| !matches!(n.tag(), Some("body" | "html") | None))
                .find(|n| linked.contains(&n.id()));
            let Some(holder) = holder else { continue };
            let g = match group_of.get(&holder.id()) {
                Some(g) => *g,
                None => {
                    let mut members = holder.similar(crate::similar::SIMILAR);
                    members.push(holder);
                    members.sort_by_key(|n| n.order());
                    let g = groups.len();
                    for m in &members {
                        group_of.entry(m.id()).or_insert(g);
                    }
                    groups.push(members);
                    priced.push(HashSet::new());
                    g
                }
            };
            priced[g].insert(holder.id());
        }
        let kept: Vec<Vec<Node<'a>>> = groups
            .into_iter()
            .zip(&priced)
            .filter(|(members, p)| members.len() >= 3 && p.len() * 2 >= members.len())
            .map(|(members, _)| members)
            .collect();
        let member = kept.iter().flatten().map(|n| n.id()).collect();
        Layout {
            shown,
            groups: kept,
            member,
        }
    }

    /// Whether `n` is inside one of the cards.
    pub fn in_card(&self, n: Node<'_>) -> bool {
        std::iter::successors(Some(n), |a| a.parent()).any(|a| self.member.contains(&a.id()))
    }
}

/// Elements with a link to somewhere else at or below them, at most
/// [`CARD_DEPTH`] levels up from the link.
fn linked(root: Node<'_>) -> HashSet<NodeId> {
    let mut out = HashSet::new();
    for a in root.descendants().filter(|n| n.tag() == Some("a")) {
        let goes = a.attr("href").map(str::trim).is_some_and(|h| {
            !h.is_empty()
                && !h.starts_with('#')
                && !h.to_ascii_lowercase().starts_with("javascript:")
        });
        if goes {
            out.extend(
                std::iter::successors(Some(a), |n| n.parent())
                    .take(CARD_DEPTH)
                    .map(|n| n.id()),
            );
        }
    }
    out
}

/// Every price the page shows outside what nobody sees: short texts that
/// read as a price with a currency, or as an amount inside an element
/// marked as a price.
fn prices(root: Node<'_>) -> Vec<Shown<'_>> {
    let mut out: Vec<Shown<'_>> = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut looked = 0;
    let mut stack: Vec<Node<'_>> = vec![root];
    while let Some(n) = stack.pop() {
        match n.kind() {
            NodeKind::Text => {
                let text = n.data().unwrap_or_default().trim();
                if text.is_empty() || text.len() > 60 || !text.bytes().any(|b| b.is_ascii_digit()) {
                    continue;
                }
                looked += 1;
                if looked > MAX_PRICES {
                    break;
                }
                // A price split across elements (`£<b>49</b>.99`) is read
                // from the smallest element around it that holds all of it.
                // The text itself first, so `<s>£120</s> £99` reads £99
                // for the element holding both.
                let tries = std::iter::successors(n.parent(), |a| a.parent())
                    .take(3)
                    .map(|el| (el, el.text()));
                let own = n.parent().map(|p| (p, text.to_string()));
                for (el, whole) in own.into_iter().chain(tries) {
                    let whole = whole.split_whitespace().collect::<Vec<_>>().join(" ");
                    if whole.len() > 40 || wordy(&whole) {
                        break;
                    }
                    let money = f::price_text(&whole);
                    if money.price.is_none() {
                        continue;
                    }
                    if money.currency_raw.is_none() && !marked(el, &PRICE_WORDS) {
                        continue;
                    }
                    if seen.insert(el.id()) {
                        out.push(Shown {
                            node: el,
                            money,
                            struck: struck(el),
                        });
                    }
                    break;
                }
            }
            NodeKind::Element if crate::markdown::unseen(n, false) => {}
            _ => {
                let at = stack.len();
                stack.extend(n.children());
                stack[at..].reverse();
            }
        }
    }
    out
}

/// Whether text says more than a price does: "Free delivery on orders
/// over £50" is a sentence with a price in it.
fn wordy(text: &str) -> bool {
    const ALLOWED: [&str; 16] = [
        "from", "now", "only", "was", "price", "sale", "inc", "incl", "vat", "each", "per", "unit",
        "our", "today", "save", "off",
    ];
    text.split(|c: char| !c.is_alphabetic())
        .filter(|w| w.chars().count() > 3)
        .filter(|w| !ALLOWED.iter().any(|a| w.eq_ignore_ascii_case(a)))
        .count()
        > 1
}

const PRICE_WORDS: [&str; 5] = ["price", "prices", "pricing", "amount", "cost"];

const STRUCK_WORDS: [&str; 10] = [
    "was",
    "old",
    "compare",
    "strike",
    "strikethrough",
    "original",
    "before",
    "rrp",
    "msrp",
    "crossed",
];

/// Whether `n` or one of the two elements above it has a class or id
/// word from `list` (or, for prices, `itemprop=price`).
fn marked(n: Node<'_>, list: &[&str]) -> bool {
    std::iter::successors(Some(n), |a| a.parent())
        .take(3)
        .any(|a| {
            words(a).any(|w| list.iter().any(|l| w.eq_ignore_ascii_case(l)))
                || a.attr("itemprop").is_some_and(|p| list.contains(&p.trim()))
        })
}

/// A price struck through or marked as the old one.
fn struck(n: Node<'_>) -> bool {
    std::iter::successors(Some(n), |a| a.parent())
        .take(3)
        .any(|a| {
            matches!(a.tag(), Some("del" | "s" | "strike"))
                || a.attr("style")
                    .is_some_and(|s| s.to_ascii_lowercase().contains("line-through"))
        })
        || marked(n, &STRUCK_WORDS)
}

pub(crate) fn source(n: Node<'_>) -> String {
    match crate::css_path::css_path(n) {
        Some(p) => format!("dom:{p}"),
        None => "dom".into(),
    }
}

/// The page's heading, outside boilerplate and cards.
fn heading<'a>(root: Node<'a>, layout: &Layout<'_>) -> Option<(Node<'a>, String)> {
    root.descendants()
        .filter(|n| n.tag() == Some("h1") && !layout.in_card(*n))
        .find_map(|n| Some((n, page_heading(n)?)))
}

/// The price nearest the heading outside cards and boilerplate, a
/// marked price counting as half as far: the product's.
fn main_price<'l, 'a>(layout: &'l Layout<'a>, h1: Node<'_>) -> Option<&'l Shown<'a>> {
    let at = h1.order() as f64;
    let distance = |s: &Shown<'_>| {
        let d = (s.node.order() as f64 - at).abs();
        if marked(s.node, &PRICE_WORDS) {
            d / 2.0
        } else {
            d
        }
    };
    layout
        .shown
        .iter()
        .filter(|s| !s.struck && !layout.in_card(s.node) && !in_boilerplate(s.node))
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
}

/// What a page's layout says about the product it shows, offered after
/// the structured sources. Returns the probability it gives, or None
/// when the page doesn't look like a product: no heading, or no price
/// outside a listing's cards.
pub(crate) fn product(page: &Page<'_, '_>, item: &mut Builder) -> Option<f64> {
    let layout = Layout::of(page.root);
    let (h1, name) = heading(page.root, &layout)?;
    let price = main_price(&layout, h1)?;
    let usable = |s: &&Shown<'_>| !layout.in_card(s.node) && !in_boilerplate(s.node);
    item.offer("name", Some(Value::String(name)), source(h1));
    let ours = price.money.price.clone();
    let agrees = !item.has("price") || item.get("price").as_str() == ours.as_deref();
    item.offer("price", ours.clone().map(Value::String), source(price.node));
    item.offer(
        "currency",
        price.money.currency.clone().map(Value::String),
        source(price.node),
    );
    item.offer(
        "currency_raw",
        price.money.currency_raw.clone().map(Value::String),
        source(price.node),
    );
    let amount = |s: &Option<String>| s.as_deref().and_then(|a| a.parse::<f64>().ok());
    if agrees
        && let Some(was) = layout
            .shown
            .iter()
            .filter(usable)
            .filter(|s| s.struck && amount(&s.money.price) > amount(&ours))
            .min_by_key(|s| s.node.order().abs_diff(price.node.order()))
            .filter(|s| s.node.order().abs_diff(price.node.order()) <= 40)
    {
        item.offer(
            "regular_price",
            was.money.price.clone().map(Value::String),
            source(was.node),
        );
    }
    let region = region(h1, price.node, &layout);
    let images: Vec<Value> = region
        .descendants()
        .filter(|n| n.tag() == Some("img") && !layout.in_card(*n) && !links_away(*n))
        .filter_map(lead_image)
        .map(Value::String)
        .collect();
    let images = Value::Array(images);
    item.offer("main_image", f::image(&images, page.base), source(region));
    item.offer("images", f::images(&images, page.base), source(region));
    let control = buy_control(region);
    item.offer(
        "availability",
        availability(region).map(|a| Value::String(a.into())),
        source(region),
    );
    for (field, value, node) in labelled(page.root, &layout) {
        item.offer(field, Some(Value::String(value)), source(node));
    }
    if let Some(d) = description(page.root, &layout) {
        let (text, html) = described(d);
        item.offer("description", text.map(Value::String), source(d));
        item.offer("description_html", html.map(Value::String), source(d));
    }
    if let Some(trail) = breadcrumbs(page.root) {
        item.offer(
            "breadcrumbs",
            f::from_element(FieldKind::Breadcrumbs, trail, page.base, page.now),
            source(trail),
        );
    }
    Some(if control { 0.7 } else { 0.55 })
}

/// The part of the page about the product: the smallest element holding
/// the heading and the price, widened up to three levels to take in the
/// images beside them.
fn region<'a>(h1: Node<'a>, price: Node<'a>, layout: &Layout<'_>) -> Node<'a> {
    let above: Vec<NodeId> = std::iter::successors(Some(h1), |n| n.parent())
        .map(|n| n.id())
        .collect();
    let mut area = std::iter::successors(Some(price), |n| n.parent())
        .find(|n| above.contains(&n.id()))
        .unwrap_or(h1);
    let has_image = |n: Node<'_>| {
        n.descendants().any(|d| {
            d.tag() == Some("img")
                && !layout.in_card(d)
                && !links_away(d)
                && lead_image(d).is_some()
        })
    };
    for _ in 0..3 {
        if has_image(area) {
            break;
        }
        match area.parent() {
            Some(p) if !matches!(p.tag(), Some("body" | "html") | None) => area = p,
            _ => break,
        }
    }
    area
}

/// An image inside a link to another page (a thumbnail of something
/// else), as opposed to one linking to its larger self.
fn links_away(img: Node<'_>) -> bool {
    std::iter::successors(img.parent(), |n| n.parent())
        .take(4)
        .find(|n| n.tag() == Some("a"))
        .and_then(|a| a.attr("href"))
        .is_some_and(|h| {
            let h = h.trim().to_ascii_lowercase();
            let h = h.split(['?', '#']).next().unwrap_or_default();
            !h.is_empty()
                && ![".jpg", ".jpeg", ".png", ".webp", ".gif", ".avif"]
                    .iter()
                    .any(|e| h.ends_with(e))
        })
}

/// What a shop writes on its buy button, and on the button when it can't
/// sell.
const BUY: [&str; 12] = [
    "add to cart",
    "add to basket",
    "add to bag",
    "add to trolley",
    "buy now",
    "buy it now",
    "sold out",
    "out of stock",
    "notify me",
    "pre-order",
    "preorder",
    "in den warenkorb",
];

fn control_text(n: Node<'_>) -> Option<String> {
    let text = match n.tag()? {
        "button" | "a" => n.text(),
        "input" => n.attr("value")?.to_string(),
        _ => return None,
    };
    Some(
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase(),
    )
}

fn buy_control(region: Node<'_>) -> bool {
    region
        .descendants()
        .filter_map(control_text)
        .any(|t| t.len() <= 40 && BUY.iter().any(|b| t.contains(b)))
}

/// `InStock` or `OutOfStock` from a short text in the product's part of
/// the page that says so, or from its buy button.
fn availability(region: Node<'_>) -> Option<&'static str> {
    let read = |t: &str| {
        if t.len() > 80 || t.len() < 7 {
            return None;
        }
        let t = t
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if t.len() > 40 {
            return None;
        }
        if ["out of stock", "sold out"].iter().any(|p| t.contains(p)) {
            Some("OutOfStock")
        } else if t.contains("in stock") && !t.contains("not in stock") {
            Some("InStock")
        } else {
            None
        }
    };
    region.descendants().find_map(|n| match n.kind() {
        NodeKind::Text => read(n.data().unwrap_or_default()),
        _ => control_text(n).and_then(|t| read(&t)),
    })
}

/// Labels of product codes, and the field each fills.
const LABELS: [(&str, &str); 12] = [
    ("sku", "sku"),
    ("item #", "sku"),
    ("item no", "sku"),
    ("item number", "sku"),
    ("product code", "sku"),
    ("article number", "sku"),
    ("art. no", "sku"),
    ("model", "mpn"),
    ("model number", "mpn"),
    ("model no", "mpn"),
    ("mpn", "mpn"),
    ("manufacturer part number", "mpn"),
];

/// Product codes written as a label and a value: `SKU: ARC-01`, a
/// `<dt>`/`<dd>` or `<th>`/`<td>` pair, or a label element followed by
/// its value. The first of each kind, outside boilerplate and cards.
fn labelled<'a>(root: Node<'a>, layout: &Layout<'_>) -> Vec<(&'static str, String, Node<'a>)> {
    let field_of = |label: &str| {
        let l = label
            .trim()
            .trim_end_matches([':', '.'])
            .trim()
            .to_lowercase();
        LABELS.iter().find(|(k, _)| *k == l).map(|(_, f)| *f)
    };
    let code = |v: &str| {
        let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
        (!v.is_empty() && v.len() <= 40 && v.split(' ').count() <= 2).then_some(v)
    };
    let mut out: Vec<(&'static str, String, Node<'a>)> = Vec::new();
    for n in root.descendants() {
        if out.len() == 2 {
            break;
        }
        let found = match n.kind() {
            NodeKind::Element if matches!(n.tag(), Some("dt" | "th")) => {
                let value = std::iter::successors(n.next_sibling(), |s| s.next_sibling())
                    .find(|s| s.kind() == NodeKind::Element);
                field_of(&n.text())
                    .zip(value)
                    .and_then(|(f, v)| Some((f, code(&v.text())?, v)))
            }
            NodeKind::Text => {
                let text = n.data().unwrap_or_default();
                let parent = n.parent();
                text.split_once(':').and_then(|(label, value)| {
                    let field = field_of(label)?;
                    let value = if value.trim().is_empty() {
                        let next =
                            std::iter::successors(parent?.next_sibling(), |s| s.next_sibling())
                                .find(|s| {
                                    s.kind() != NodeKind::Text || !s.text().trim().is_empty()
                                })?;
                        code(&next.text())?
                    } else {
                        code(value)?
                    };
                    Some((field, value, parent?))
                })
            }
            _ => None,
        };
        if let Some((field, value, node)) = found
            && !out.iter().any(|(f, _, _)| *f == field)
            && !layout.in_card(node)
            && !in_boilerplate(node)
        {
            out.push((field, value, node));
        }
    }
    out
}

/// The element holding the product's description: one whose class or id
/// says so, with some text in it.
fn description<'a>(root: Node<'a>, layout: &Layout<'_>) -> Option<Node<'a>> {
    root.descendants().find(|n| {
        n.kind() == NodeKind::Element
            && !matches!(n.tag(), Some("meta" | "body" | "html"))
            && words(*n).any(|w| w.eq_ignore_ascii_case("description"))
            && n.text().trim().len() >= 40
            && !layout.in_card(*n)
            && !in_boilerplate(*n)
    })
}

/// A description's text and HTML, without a heading that only says
/// "Description".
fn described(d: Node<'_>) -> (Option<String>, Option<String>) {
    let parts: Vec<Node<'_>> = d
        .children()
        .filter(|c| !matches!(c.tag(), Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6")))
        .collect();
    let text: Vec<String> = parts
        .iter()
        .map(|p| {
            crate::markdown::clean_text(*p)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
        .collect();
    let html: String = parts
        .iter()
        .filter(|p| p.kind() == NodeKind::Element)
        .map(|p| crate::content::clean_html_fragment(&p.html()))
        .collect::<Vec<_>>()
        .join("");
    (
        (!text.is_empty()).then(|| text.join("\n\n")),
        (!html.is_empty()).then_some(html),
    )
}

/// The page's breadcrumb trail: the outermost element labelled or
/// classed as one.
pub(crate) fn breadcrumbs(root: Node<'_>) -> Option<Node<'_>> {
    root.descendants().find(|n| {
        n.kind() == NodeKind::Element
            && (n
                .attr("aria-label")
                .is_some_and(|l| l.to_ascii_lowercase().contains("breadcrumb"))
                || words(*n).any(|w| {
                    ["breadcrumb", "breadcrumbs", "crumbs"]
                        .iter()
                        .any(|b| w.eq_ignore_ascii_case(b))
                }))
            && n.descendants().any(|d| d.tag() == Some("a"))
    })
}

/// A listing read from the page's cards.
pub(crate) struct Listing {
    /// Each card as a list shows a product: name, price, regular price,
    /// currency, URL and image, with only the fields it has.
    pub products: Vec<Map<String, Value>>,
    /// The element holding the cards.
    pub source: String,
    /// The page's heading and its source, unless the page is a product's
    /// (whose cards are related products, not a category).
    pub heading: Option<(String, String)>,
    /// The page shows a product of its own beside the cards.
    pub product_page: bool,
}

/// The page's listing: its largest group of priced cards outside
/// boilerplate.
pub(crate) fn listing(root: Node<'_>, base: Option<&Url>) -> Option<Listing> {
    let layout = Layout::of(root);
    let cards = layout
        .groups
        .iter()
        .filter(|g| !in_boilerplate(g[0]))
        .max_by_key(|g| g.len())?;
    let products: Vec<Map<String, Value>> = cards
        .iter()
        .map(|c| card(*c, &layout, base))
        .filter(|p| p.contains_key("url") || p.contains_key("name"))
        .collect();
    if products.len() < 3 {
        return None;
    }
    let found = heading(root, &layout);
    let product_page = found
        .as_ref()
        .is_some_and(|(h1, _)| main_price(&layout, *h1).is_some());
    let container = cards[0].parent().unwrap_or(cards[0]);
    Some(Listing {
        products,
        source: source(container),
        heading: found
            .filter(|_| !product_page)
            .map(|(h1, text)| (text, source(h1))),
        product_page,
    })
}

/// One card as a listed product.
fn card(c: Node<'_>, layout: &Layout<'_>, base: Option<&Url>) -> Map<String, Value> {
    let mut out = Map::new();
    let heading = c
        .descendants()
        .find(|n| matches!(n.tag(), Some("h2" | "h3" | "h4" | "h5" | "h6")));
    let links = || {
        std::iter::once(c)
            .chain(c.descendants())
            .filter(|n| n.tag() == Some("a") && n.attr("href").is_some())
    };
    let link = heading
        .and_then(|h| {
            std::iter::successors(Some(h), |n| n.parent())
                .take_while(|n| n.id() != c.parent().map_or(c.id(), |p| p.id()))
                .find(|n| n.tag() == Some("a"))
                .or_else(|| h.descendants().find(|n| n.tag() == Some("a")))
        })
        .or_else(|| links().max_by_key(|a| a.text().trim().len()));
    let tidy = |t: String| {
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        (!t.is_empty()).then_some(t)
    };
    let image = c.descendants().find(|n| n.tag() == Some("img"));
    let name = heading
        .and_then(|h| tidy(crate::markdown::clean_text(h)))
        .or_else(|| link.and_then(|a| tidy(a.text())))
        .or_else(|| image.and_then(|i| tidy(i.attr("alt")?.to_string())));
    if let Some(n) = name {
        out.insert("name".into(), Value::String(n));
    }
    let inside = |s: &&Shown<'_>| {
        std::iter::successors(Some(s.node), |n| n.parent()).any(|n| n.id() == c.id())
    };
    let shown: Vec<&Shown<'_>> = layout.shown.iter().filter(inside).collect();
    let amount = |s: &Shown<'_>| s.money.price.as_deref().and_then(|a| a.parse::<f64>().ok());
    if let Some(now) = shown.iter().find(|s| !s.struck) {
        let money = &now.money;
        for (k, v) in [
            ("price", &money.price),
            ("currency", &money.currency),
            ("currency_raw", &money.currency_raw),
        ] {
            if let Some(v) = v {
                out.insert(k.into(), Value::String(v.clone()));
            }
        }
        if let Some(was) = shown
            .iter()
            .find(|s| s.struck && amount(s) > amount(now))
            .and_then(|s| s.money.price.clone())
        {
            out.insert("regular_price".into(), Value::String(was));
        }
    }
    if let Some(u) = link
        .and_then(|a| a.attr("href"))
        .and_then(|h| f::absolute(h, base))
    {
        out.insert("url".into(), Value::String(u));
    }
    if let Some(src) = image
        .and_then(crate::markdown::image_source)
        .and_then(|s| f::absolute(s, base))
    {
        out.insert("main_image".into(), serde_json::json!({ "url": src }));
    }
    // Keys in the order the list's other readers write them.
    let order = [
        "name",
        "price",
        "regular_price",
        "currency",
        "currency_raw",
        "url",
        "main_image",
    ];
    order
        .iter()
        .filter_map(|k| Some((k.to_string(), out.remove(*k)?)))
        .collect()
}

/// A page's pagination widget: an element labelled or classed as one,
/// with links in it.
fn pagers(root: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    root.descendants().filter(|n| {
        n.kind() == NodeKind::Element
            && (n
                .attr("aria-label")
                .is_some_and(|l| l.to_ascii_lowercase().contains("pag"))
                || words(*n).any(|w| {
                    ["pagination", "pager", "paging", "paginator", "pages"]
                        .iter()
                        .any(|p| w.eq_ignore_ascii_case(p))
                }))
            && n.descendants().any(|d| d.tag() == Some("a"))
    })
}

/// Whether a link, or the element it sits in, says it's switched off.
fn disabled(a: Node<'_>) -> bool {
    std::iter::successors(Some(a), |n| n.parent())
        .take(2)
        .any(|n| {
            n.attr("aria-disabled")
                .is_some_and(|d| d.trim().eq_ignore_ascii_case("true"))
                || n.attr("disabled").is_some()
                || words(n).any(|w| w.eq_ignore_ascii_case("disabled"))
        })
}

/// The next page from a pagination widget: its link whose text, label
/// or class says next, unless it's switched off or goes nowhere. The
/// URL, the link's text and its source.
pub(crate) fn next_page(
    root: Node<'_>,
    page: Option<&Url>,
    base: Option<&Url>,
) -> Option<(String, Option<String>, String)> {
    let says_next = |a: Node<'_>| {
        let text = a.text().split_whitespace().collect::<Vec<_>>().join(" ");
        let lower = text.to_lowercase();
        let label = [a.attr("aria-label"), a.attr("title"), a.attr("rel")]
            .into_iter()
            .flatten()
            .any(|l| l.to_lowercase().split_whitespace().any(|w| w == "next"));
        label
            || words(a).any(|w| w.eq_ignore_ascii_case("next"))
            || (lower.len() <= 20 && lower.starts_with("next"))
            || matches!(lower.as_str(), "›" | "»" | "→" | ">" | ">>")
    };
    pagers(root).find_map(|pager| {
        pager.descendants().find_map(|a| {
            if a.tag() != Some("a") || disabled(a) || !says_next(a) {
                return None;
            }
            let href = a.attr("href")?.trim();
            if href.is_empty()
                || href.starts_with('#')
                || href.to_ascii_lowercase().starts_with("javascript:")
            {
                return None;
            }
            let url = f::absolute(href, base)?;
            if page.is_some_and(|p| p.as_str() == url) {
                return None;
            }
            let text = f::text(&Value::String(a.text()));
            Some((url, text, source(a)))
        })
    })
}

/// The number of the page: the current entry of its pagination widget,
/// else a page parameter or `/page/N` in its URL.
pub(crate) fn page_number(root: Node<'_>, page: Option<&Url>) -> Option<(i64, String)> {
    let number = |t: &str| t.trim().parse::<i64>().ok().filter(|n| *n > 0);
    let current = pagers(root).find_map(|pager| {
        pager.descendants().find_map(|n| {
            let marked = n
                .attr("aria-current")
                .is_some_and(|c| !c.trim().eq_ignore_ascii_case("false"))
                || words(n).any(|w| {
                    ["current", "active", "selected"]
                        .iter()
                        .any(|c| w.eq_ignore_ascii_case(c))
                });
            (n.kind() == NodeKind::Element && marked)
                .then(|| number(&n.text()))
                .flatten()
                .map(|p| (p, source(n)))
        })
    });
    current.or_else(|| {
        let url = page?;
        let from_query = url.query_pairs().find_map(|(k, v)| {
            ["page", "pg", "pagenum", "page_number", "pagenumber"]
                .iter()
                .any(|name| k.eq_ignore_ascii_case(name))
                .then(|| number(&v))
                .flatten()
        });
        let from_path = || {
            let segments: Vec<&str> = url.path_segments()?.collect();
            segments
                .windows(2)
                .find(|w| w[0].eq_ignore_ascii_case("page"))
                .and_then(|w| number(w[1]))
        };
        from_query
            .or_else(from_path)
            .map(|p| (p, "url".to_string()))
    })
}
