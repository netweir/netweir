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

/// Prices read at most; past it, the rest of the page isn't read for
/// them.
const MAX_PRICES: usize = 2000;

/// Short texts with digits looked at for prices at most.
const MAX_TEXTS: usize = 50_000;

/// `similar()` searches at most, each a pass over the page's index.
const MAX_GROUPS: usize = 16;

/// How far, in elements and texts, a product's price may be from its
/// heading.
const NEAR: u32 = 300;

/// Levels above a price searched for the card holding it.
const CARD_DEPTH: usize = 8;

/// A price the page shows: the element holding it and its reading.
pub(crate) struct Shown<'a> {
    pub node: Node<'a>,
    pub money: PriceFields,
    pub struck: Struck,
}

/// Whether a price is the old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Struck {
    No,
    /// Only its class says so (`price-item--regular`), which some themes
    /// put on the price itself when nothing is on sale: the price when
    /// there's no other.
    Weak,
    /// Struck through, classed as old (`was`, `compare`), or said in
    /// words (`Was £120`, `RRP £120`).
    Strong,
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
        for s in shown.iter().filter(|s| s.struck == Struck::No) {
            let holder = std::iter::successors(Some(s.node), |n| n.parent())
                .take(CARD_DEPTH)
                .take_while(|n| !matches!(n.tag(), Some("body" | "html") | None))
                .find(|n| linked.contains(&n.id()));
            let Some(holder) = holder else { continue };
            let g = match group_of.get(&holder.id()) {
                Some(g) => *g,
                None if groups.len() >= MAX_GROUPS => continue,
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
        if out.len() >= MAX_PRICES || looked >= MAX_TEXTS {
            break;
        }
        match n.kind() {
            NodeKind::Text => {
                let text = n.data().unwrap_or_default().trim();
                if text.is_empty() || text.len() > 60 || !text.bytes().any(|b| b.is_ascii_digit()) {
                    continue;
                }
                looked += 1;
                let Some(parent) = n.parent() else { continue };
                // `Was £120.00 Now £99.00` in one text: both prices.
                if let Some((was, now)) = was_now(text) {
                    if seen.insert(parent.id()) {
                        out.push(Shown {
                            node: parent,
                            money: was,
                            struck: Struck::Strong,
                        });
                        out.push(Shown {
                            node: parent,
                            money: now,
                            struck: Struck::No,
                        });
                    }
                    continue;
                }
                // The text itself first, so `<s>£120</s> £99` reads £99
                // for the element holding both; then, for a price split
                // across elements (`£<b>49</b>.99`), the smallest element
                // around it that holds all of it.
                let own = std::iter::once((parent, Some(text.to_string())));
                let tries = std::iter::successors(Some(parent), |a| a.parent())
                    .take(3)
                    .map(|el| (el, short_text(el, 40)));
                for (el, whole) in own.chain(tries) {
                    let Some(whole) = whole else { break };
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
                            struck: struck(el, &whole),
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

/// The text at and below `n`, or None once it passes `cap` bytes: read
/// no further than that, so asking costs little on a big element.
fn short_text(n: Node<'_>, cap: usize) -> Option<String> {
    let mut out = String::new();
    for (visited, d) in std::iter::once(n).chain(n.descendants()).enumerate() {
        if visited > 4 * cap + 16 {
            return None;
        }
        if d.kind() == NodeKind::Text {
            out.push_str(d.data().unwrap_or_default());
            if out.len() > cap * 2 && out.trim().len() > cap {
                return None;
            }
        }
    }
    (out.trim().len() <= cap).then_some(out)
}

/// The old and new price of `Was £120.00 Now £99.00`.
fn was_now(text: &str) -> Option<(PriceFields, PriceFields)> {
    let lower = text.to_lowercase();
    let at = lower.find("now")?;
    if !lower[..at].contains("was") || !text.is_char_boundary(at) {
        return None;
    }
    let (was, now) = (f::price_text(&text[..at]), f::price_text(&text[at..]));
    (was.price.is_some() && now.price.is_some()).then_some((was, now))
}

/// Whether text says more than a price does: "Free delivery on orders
/// over £50", "Save £20", "Tickets £15" have a price in them, but aren't
/// one.
fn wordy(text: &str) -> bool {
    const ALLOWED: [&str; 28] = [
        "from", "now", "only", "was", "price", "sale", "inc", "incl", "excl", "vat", "tax", "each",
        "per", "unit", "our", "today", "rrp", "msrp", "list", "regular", "special", "offer",
        "starting", "euro", "euros", "dollars", "pounds", "price:",
    ];
    text.split(|c: char| !c.is_alphabetic())
        .filter(|w| w.chars().count() > 2)
        .filter(|w| !ALLOWED.iter().any(|a| w.eq_ignore_ascii_case(a)))
        .any(|w| crate::price::currency_code(w).is_none())
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

/// Words that, beside a price word in one class (`list-price`,
/// `price-item--regular`), mark the price as maybe the old one.
const WEAK_WORDS: [&str; 2] = ["list", "regular"];

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

/// Whether a price, reading `text`, is the old one.
fn struck(n: Node<'_>, text: &str) -> Struck {
    let lower = text.to_lowercase();
    let said = [
        "was",
        "rrp",
        "msrp",
        "list price",
        "compare at",
        "originally",
        "before",
    ]
    .iter()
    .any(|w| lower.starts_with(w));
    let strong = said
        || std::iter::successors(Some(n), |a| a.parent())
            .take(3)
            .any(|a| {
                matches!(a.tag(), Some("del" | "s" | "strike"))
                    || a.attr("style")
                        .is_some_and(|s| s.to_ascii_lowercase().contains("line-through"))
            })
        || marked(n, &STRUCK_WORDS);
    let weak = || {
        std::iter::successors(Some(n), |a| a.parent())
            .take(2)
            .filter_map(|a| a.attr("class"))
            .flat_map(str::split_whitespace)
            .any(|class| {
                let parts: Vec<&str> = class
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .collect();
                let has = |list: &[&str]| {
                    parts
                        .iter()
                        .any(|p| list.iter().any(|l| p.eq_ignore_ascii_case(l)))
                };
                (has(&PRICE_WORDS) && has(&WEAK_WORDS))
                    || ["listprice", "regularprice"]
                        .iter()
                        .any(|c| class.eq_ignore_ascii_case(c))
            })
    };
    if strong {
        Struck::Strong
    } else if weak() {
        Struck::Weak
    } else {
        Struck::No
    }
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

/// The product's price: the one nearest the heading, outside cards,
/// boilerplate and links, a marked price counting as half as far. Only
/// a price marked as one, or with a buy button beside it, counts: a
/// stray "from £19" under a category's heading isn't a product's. A page
/// with buy buttons on three or more things is a listing, not a product.
/// A price only its class calls the old one is the price when there's
/// no other.
fn main_price<'l, 'a>(
    layout: &'l Layout<'a>,
    root: Node<'a>,
    h1: Node<'a>,
) -> Option<&'l Shown<'a>> {
    let at = h1.order();
    let distance = |s: &Shown<'_>| {
        let d = s.node.order().abs_diff(at) as f64;
        if marked(s.node, &PRICE_WORDS) {
            d / 2.0
        } else {
            d
        }
    };
    if buy_controls(root) >= 3 {
        return None;
    }
    let near = |s: &&Shown<'a>| {
        s.node.order().abs_diff(at) <= NEAR
            && !layout.in_card(s.node)
            && !in_boilerplate(s.node)
            && !std::iter::successors(Some(s.node), |n| n.parent()).any(|n| n.tag() == Some("a"))
    };
    // Nearest first; the buy-button check walks the price's part of the
    // page, so it runs once per part and only until a price passes.
    let mut buying: HashMap<NodeId, bool> = HashMap::new();
    let mut best = |struck: Struck| {
        let mut found: Vec<&'l Shown<'a>> = layout
            .shown
            .iter()
            .filter(|s| s.struck == struck)
            .filter(near)
            .collect();
        found.sort_by(|a, b| distance(a).total_cmp(&distance(b)));
        found.into_iter().find(|s| {
            marked(s.node, &PRICE_WORDS) || {
                let shared = common(h1, s.node);
                *buying
                    .entry(shared.id())
                    .or_insert_with(|| buy_control(widen(shared, layout)))
            }
        })
    };
    let now = best(Struck::No);
    now.or_else(|| best(Struck::Weak))
}

/// Whether two prices are the same amount: `49.9` is `£49.90`.
fn same_amount(a: Option<&str>, b: Option<&str>) -> bool {
    let n = |s: Option<&str>| s.and_then(|s| s.parse::<f64>().ok());
    a.is_some() && (a == b || (n(a).is_some() && n(a) == n(b)))
}

/// Whether the page shows a product of its own: a heading with a price
/// beside it, as [`product`] reads one.
pub(crate) fn shows_product(root: Node<'_>) -> bool {
    let layout = Layout::of(root);
    heading(root, &layout).is_some_and(|(h1, _)| main_price(&layout, root, h1).is_some())
}

/// What a page's layout says about the product it shows, offered after
/// the structured sources. Returns the probability it gives, or None
/// when the page doesn't look like a product: no heading, or no price
/// beside it (see [`main_price`]).
pub(crate) fn product(page: &Page<'_, '_>, item: &mut Builder) -> Option<f64> {
    let layout = Layout::of(page.root);
    let (h1, name) = heading(page.root, &layout)?;
    let price = main_price(&layout, page.root, h1)?;
    let usable = |s: &&Shown<'_>| !layout.in_card(s.node) && !in_boilerplate(s.node);
    item.offer("name", Some(Value::String(name)), source(h1));
    let ours = price.money.price.clone();
    // The layout's price is the one structured data gave, or there was
    // none: what's beside it (its currency, the old price) belongs to it.
    let agrees = !item.has("price") || same_amount(item.get("price").as_str(), ours.as_deref());
    item.offer("price", ours.clone().map(Value::String), source(price.node));
    if agrees {
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
    }
    let amount = |s: &Option<String>| s.as_deref().and_then(|a| a.parse::<f64>().ok());
    if agrees
        && let Some(was) = layout
            .shown
            .iter()
            .filter(usable)
            .filter(|s| s.struck != Struck::No && amount(&s.money.price) > amount(&ours))
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
        .filter(|n| n.tag() == Some("img") && product_image(*n, region, &layout))
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
    for (field, value, node) in labelled(page.root, &layout, &LABELS, code) {
        item.offer(field, Some(value), source(node));
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

/// Whether an image in the product's part of the page shows it: not in a
/// card, not a thumbnail linking elsewhere, and not in a section of its
/// own with its own heading (a blog teaser under the buy box).
fn product_image(img: Node<'_>, region: Node<'_>, layout: &Layout<'_>) -> bool {
    !layout.in_card(img)
        && !links_away(img)
        && !std::iter::successors(img.parent(), |n| n.parent())
            .take_while(|n| n.id() != region.id())
            .any(|n| {
                n.children()
                    .any(|c| matches!(c.tag(), Some("h2" | "h3" | "h4" | "h5" | "h6")))
            })
}

/// The part of the page about the product: the smallest element holding
/// the heading and the price, widened up to three levels to take in the
/// images beside them.
fn region<'a>(h1: Node<'a>, price: Node<'a>, layout: &Layout<'_>) -> Node<'a> {
    widen(common(h1, price), layout)
}

/// The smallest element holding both.
fn common<'a>(h1: Node<'a>, price: Node<'a>) -> Node<'a> {
    let above: HashSet<NodeId> = std::iter::successors(Some(h1), |n| n.parent())
        .map(|n| n.id())
        .collect();
    std::iter::successors(Some(price), |n| n.parent())
        .find(|n| above.contains(&n.id()))
        .unwrap_or(h1)
}

/// `area` widened up to three levels to take in the product's images.
fn widen<'a>(mut area: Node<'a>, layout: &Layout<'_>) -> Node<'a> {
    let has_image = |n: Node<'_>| {
        n.descendants().any(|d| {
            d.tag() == Some("img") && product_image(d, n, layout) && lead_image(d).is_some()
        })
    };
    for _ in 0..3 {
        if has_image(area) || matches!(area.tag(), Some("main" | "article" | "body" | "html")) {
            break;
        }
        match area.parent() {
            Some(p) if !matches!(p.tag(), Some("main" | "article" | "body" | "html") | None) => {
                area = p
            }
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
        "button" | "a" => short_text(n, 60)?,
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

fn buys(t: &str) -> bool {
    t.len() <= 40 && BUY.iter().any(|b| t.contains(b))
}

fn buy_control(region: Node<'_>) -> bool {
    region
        .descendants()
        .filter_map(control_text)
        .any(|t| buys(&t))
}

/// Buy buttons on the page outside boilerplate, counted up to three.
fn buy_controls(root: Node<'_>) -> usize {
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() != NodeKind::Element && n.kind() != NodeKind::Document {
            continue;
        }
        if crate::content::says_boilerplate(n) {
            continue;
        }
        if control_text(n).is_some_and(|t| buys(&t)) {
            count += 1;
            if count >= 3 {
                break;
            }
            continue;
        }
        stack.extend(n.children());
    }
    count
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
        } else if t.contains("in stock")
            && !["not in stock", "back in stock", "when", "soon", "notify"]
                .iter()
                .any(|p| t.contains(p))
        {
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

/// Values written as a label and a value: `SKU: ARC-01`, a `<dt>`/`<dd>`
/// or `<th>`/`<td>` pair, or a label element followed by its value. The
/// labels are `table`'s, each naming a field; `read` turns the value's
/// text into the field's value, or turns it down. The first of each
/// field, outside boilerplate and cards.
fn labelled<'a>(
    root: Node<'a>,
    layout: &Layout<'_>,
    table: &[(&str, &'static str)],
    read: impl Fn(&'static str, &str) -> Option<Value>,
) -> Vec<(&'static str, Value, Node<'a>)> {
    let field_of = |label: &str| {
        let l = label
            .trim()
            .trim_end_matches([':', '.'])
            .trim()
            .to_lowercase();
        table.iter().find(|(k, _)| *k == l).map(|(_, f)| *f)
    };
    let fields = {
        let mut f: Vec<&str> = table.iter().map(|(_, f)| *f).collect();
        f.dedup();
        f.len()
    };
    let mut out: Vec<(&'static str, Value, Node<'a>)> = Vec::new();
    for n in root.descendants() {
        if out.len() == fields {
            break;
        }
        let found = match n.kind() {
            NodeKind::Element if matches!(n.tag(), Some("dt" | "th")) => {
                let wanted = if n.tag() == Some("dt") { "dd" } else { "td" };
                let value = std::iter::successors(n.next_sibling(), |s| s.next_sibling())
                    .find(|s| s.kind() == NodeKind::Element)
                    .filter(|v| v.tag() == Some(wanted));
                short_text(n, 60)
                    .and_then(|label| field_of(&label))
                    .zip(value)
                    .and_then(|(f, v)| Some((f, read(f, &short_text(v, 300)?)?, v)))
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
                                    s.kind() != NodeKind::Text
                                        || !s.data().unwrap_or_default().trim().is_empty()
                                })?;
                        read(field, &short_text(next, 300)?)?
                    } else {
                        read(field, value)?
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

/// A product code: a word or two, not a sentence.
fn code(_: &'static str, v: &str) -> Option<Value> {
    let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
    (!v.is_empty() && v.len() <= 40 && v.split(' ').count() <= 2).then_some(Value::String(v))
}

/// The element holding the description: the first one whose class or id
/// says so, with some text in it, outside boilerplate and cards. One
/// holding the page's heading is a wrapper, not the description.
fn description<'a>(root: Node<'a>, layout: &Layout<'_>) -> Option<Node<'a>> {
    let holds_h1: HashSet<NodeId> = root
        .descendants()
        .filter(|n| n.tag() == Some("h1"))
        .flat_map(|h| std::iter::successors(Some(h), |n| n.parent()))
        .map(|n| n.id())
        .collect();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() != NodeKind::Element && n.kind() != NodeKind::Document {
            continue;
        }
        if crate::content::says_boilerplate(n) || layout.in_card(n) {
            continue;
        }
        let named = !matches!(n.tag(), Some("meta" | "body" | "html"))
            && words(n).any(|w| w.eq_ignore_ascii_case("description"));
        if named && !holds_h1.contains(&n.id()) && has_text(n, 40) {
            return Some(n);
        }
        let at = stack.len();
        stack.extend(n.children());
        stack[at..].reverse();
    }
    None
}

/// Whether there are at least `min` bytes of text at and below `n`,
/// reading no more than it takes to know.
fn has_text(n: Node<'_>, min: usize) -> bool {
    let mut total = 0;
    n.descendants()
        .filter(|d| d.kind() == NodeKind::Text)
        .any(|d| {
            total += d.data().unwrap_or_default().trim().len();
            total >= min
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
        .is_some_and(|(h1, _)| main_price(&layout, root, *h1).is_some());
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
    let current = shown
        .iter()
        .find(|s| s.struck == Struck::No)
        .or_else(|| shown.iter().find(|s| s.struck == Struck::Weak));
    if let Some(now) = current {
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
            .find(|s| s.struck != Struck::No && amount(s) > amount(now))
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
        let text = short_text(a, 40).unwrap_or_default();
        let lower = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let label = [a.attr("aria-label"), a.attr("title"), a.attr("rel")]
            .into_iter()
            .flatten()
            .any(|l| l.to_lowercase().split_whitespace().any(|w| w == "next"));
        label
            || words(a).any(|w| w.eq_ignore_ascii_case("next"))
            || (lower.len() <= 20 && lower.starts_with("next"))
            // `»` and `>>` are as often the last page as the next.
            || matches!(lower.as_str(), "›" | "→" | ">")
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
            if page.is_some_and(|p| same_page(p, &url)) {
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

/// Labels job adverts put beside their facts, and the field each fills.
const JOB_LABELS: [(&str, &str); 22] = [
    ("location", "job_location"),
    ("job location", "job_location"),
    ("where", "job_location"),
    ("salary", "base_salary"),
    ("pay", "base_salary"),
    ("compensation", "base_salary"),
    ("salary range", "base_salary"),
    ("job type", "employment_type"),
    ("employment type", "employment_type"),
    ("contract type", "employment_type"),
    ("hours", "employment_type"),
    ("company", "hiring_organization"),
    ("employer", "hiring_organization"),
    ("organisation", "hiring_organization"),
    ("organization", "hiring_organization"),
    ("closing date", "valid_through"),
    ("apply by", "valid_through"),
    ("deadline", "valid_through"),
    ("posted", "date_published"),
    ("date posted", "date_published"),
    ("start date", "job_start_date"),
    ("starts", "job_start_date"),
];

/// What a page's layout says about the job it advertises, offered after
/// the structured sources. Returns the probability it gives, or None when
/// nothing beside the heading says it's a job: an apply button, a salary,
/// or two of the facts job adverts list.
pub(crate) fn job(page: &Page<'_, '_>, item: &mut Builder) -> Option<f64> {
    let layout = Layout::of(page.root);
    let (h1, title) = heading(page.root, &layout)?;
    let schema = super::Schema::JobPosting;
    let mut facts = labelled(page.root, &layout, &JOB_LABELS, |field, text| {
        let kind = schema.kind_of(field)?;
        f::from_text(kind, text.trim(), page.base, page.now)
    });
    // A board's cards each hold another job's facts.
    facts.retain(|(_, _, n)| !in_listed_card(*n));
    let area = std::iter::successors(Some(h1), |n| n.parent())
        .find(|n| matches!(n.tag(), Some("article" | "main")))
        .or_else(|| std::iter::successors(Some(h1), |n| n.parent()).nth(3))
        .unwrap_or(h1);
    let apply = area
        .descendants()
        .filter_map(|n| Some((n, control_text(n)?)))
        .any(|(n, t)| {
            let t = t.trim_end_matches(['!', '.', '›', '→', ' ']);
            let says = t == "apply"
                || [
                    "apply now",
                    "apply for this",
                    "apply online",
                    "apply here",
                    "apply today",
                    "easy apply",
                    "quick apply",
                ]
                .iter()
                .any(|p| t.starts_with(p));
            says && !in_boilerplate(n)
        });
    let salary_text = || {
        area.descendants()
            .filter(|n| n.kind() == NodeKind::Text)
            .find_map(|n| {
                let t = n.data().unwrap_or_default().trim();
                if t.len() > 80 || !t.bytes().any(|b| b.is_ascii_digit()) {
                    return None;
                }
                // A salary as adverts write it starts with the money
                // ("£12.50 an hour", "Up to £40k"), not as a sentence
                // about pay; and it isn't a headline.
                let lower = t.to_lowercase();
                let starts = t.starts_with(|c: char| !c.is_alphabetic())
                    || ["up to ", "from ", "salary", "pay "]
                        .iter()
                        .any(|p| lower.starts_with(p));
                let headline = std::iter::successors(n.parent(), |a| a.parent())
                    .take(3)
                    .any(|a| matches!(a.tag(), Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6")));
                // No words, no rate: `£9.99` isn't a salary.
                if !starts || headline || !t.chars().any(char::is_alphabetic) {
                    return None;
                }
                let s = f::salary(&Value::String(t.to_string()))?;
                let parent = n.parent()?;
                // One of a board's cards is another job's salary.
                (!s["currency_raw"].is_null()
                    && !s["rate_type"].is_null()
                    && !in_boilerplate(parent)
                    && !in_listed_card(parent))
                .then_some((s, parent))
            })
    };
    let salary = facts
        .iter()
        .find(|(f, _, _)| *f == "base_salary")
        .map(|(_, v, n)| (v.clone(), *n))
        .or_else(salary_text);
    if !apply && salary.is_none() && facts.len() < 2 {
        return None;
    }
    item.offer("job_title", Some(Value::String(title)), source(h1));
    for (field, value, node) in &facts {
        if *field == "base_salary" {
            continue;
        }
        let value = match *field {
            "date_published" | "valid_through" | "job_start_date" => {
                let raw = format!("{field}_raw");
                let raw = schema.fields().iter().find(|f| **f == raw).copied();
                if let Some(raw) = raw {
                    let text = f::text(&Value::String(node.text()));
                    item.offer(raw, text.map(Value::String), source(*node));
                }
                value.clone()
            }
            _ => value.clone(),
        };
        item.offer(field, Some(value), source(*node));
    }
    if let Some((value, node)) = salary {
        item.offer("base_salary", Some(value), source(node));
    }
    if !item.has("hiring_organization")
        && let Some(org) = area.descendants().find(|n| {
            n.kind() == NodeKind::Element
                && words(*n).any(|w| {
                    [
                        "company",
                        "employer",
                        "organisation",
                        "organization",
                        "hiring",
                    ]
                    .iter()
                    .any(|o| w.eq_ignore_ascii_case(o))
                })
                && short_text(*n, 80).is_some_and(|t| !t.trim().is_empty())
        })
    {
        item.offer(
            "hiring_organization",
            f::from_element(FieldKind::Organization, org, page.base, page.now),
            source(org),
        );
    }
    if !item.has("employment_type")
        && let Some(n) = area.descendants().find(|n| {
            n.kind() == NodeKind::Text
                && [
                    "full-time",
                    "full time",
                    "part-time",
                    "part time",
                    "contract",
                    "temporary",
                    "permanent",
                    "internship",
                ]
                .iter()
                .any(|w| n.data().unwrap_or_default().trim().eq_ignore_ascii_case(w))
        })
    {
        let text = n.data().unwrap_or_default().trim().to_string();
        item.offer(
            "employment_type",
            f::employment_type(&Value::String(text)).map(Value::String),
            source(n.parent().unwrap_or(n)),
        );
    }
    if let Some(d) = description(page.root, &layout) {
        let (text, html) = described(d);
        item.offer("description", text.map(Value::String), source(d));
        item.offer("description_html", html.map(Value::String), source(d));
    }
    Some(0.5)
}

/// Whether `n` sits in one of several look-alike cards, each holding a
/// link: an entry of a list, not the page's own.
fn in_listed_card(n: Node<'_>) -> bool {
    std::iter::successors(n.parent(), |a| a.parent())
        .take(CARD_DEPTH)
        .take_while(|a| !matches!(a.tag(), Some("body" | "html" | "main" | "article") | None))
        .find(|a| {
            a.descendants()
                .any(|d| d.tag() == Some("a") && d.attr("href").is_some())
        })
        .is_some_and(|card| card.similar(crate::similar::SIMILAR).len() >= 2)
}

/// Whether `url` is `page` written another way: the same host, path
/// (with or without its trailing slash) and query.
fn same_page(page: &Url, url: &str) -> bool {
    let Ok(other) = Url::parse(url) else {
        return false;
    };
    let path = |u: &Url| u.path().trim_end_matches('/').to_string();
    let query = |u: &Url| {
        let mut q: Vec<(String, String)> = u.query_pairs().into_owned().collect();
        q.sort();
        q
    };
    page.host_str() == other.host_str()
        && path(page) == path(&other)
        && query(page) == query(&other)
}

/// Links read for sub-categories at most.
const MAX_SUB_CATEGORIES: usize = 200;

/// Words in a link's text that make it the site's furniture, not a
/// category.
const FURNITURE: &[&str] = &[
    "login", "log in", "sign in", "sign up", "register", "account", "basket", "cart", "bag",
    "checkout", "wishlist", "help", "contact", "about", "privacy", "terms", "cookies", "faq",
    "delivery", "returns", "careers", "home",
];

/// The categories a page leads to: links in a list marked as categories
/// (`categories`, `subcat`, `departments`, `collections`), and links one
/// level below the page's own path (`/c/lamps/desk` on `/c/lamps`). On a
/// home page with neither, its main menu. Never the page itself or
/// another page of it, an item in `items`, a link off the site, or a link
/// in a footer or the breadcrumbs, and never account, basket or help
/// links. Each is `{url, name}`, with the source of the first.
pub(crate) fn sub_categories(
    root: Node<'_>,
    page: Option<&Url>,
    base: Option<&Url>,
    items: &HashSet<String>,
) -> Option<(Vec<Value>, String)> {
    let page = page?;
    let trail = breadcrumbs(root);
    let footer = |n: Node<'_>| {
        std::iter::successors(Some(n), |p| p.parent()).any(|p| {
            p.tag() == Some("footer")
                || p.attr("role") == Some("contentinfo")
                || trail == Some(p)
                || words(p).any(|w| w.eq_ignore_ascii_case("footer"))
        })
    };
    let marked = |n: Node<'_>| {
        std::iter::successors(n.parent(), |p| p.parent())
            .take(6)
            .any(|p| {
                words(p).any(|w| {
                    let w = w.to_ascii_lowercase();
                    ["categor", "subcat", "department", "collection"]
                        .iter()
                        .any(|k| w.starts_with(k))
                }) || p.attr("aria-label").is_some_and(|l| {
                    let l = l.to_ascii_lowercase();
                    l.contains("categor") || l.contains("department")
                })
            })
    };
    let in_menu = |n: Node<'_>| {
        std::iter::successors(n.parent(), |p| p.parent())
            .any(|p| p.tag() == Some("nav") || p.attr("role") == Some("navigation"))
    };
    let own_path = page.path().trim_end_matches('/').to_string();
    let child = |u: &Url| {
        let p = u.path().trim_end_matches('/');
        p.strip_prefix(&own_path)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
    };
    let home = own_path.is_empty();
    // (url, name, node, in a marked list or a child, in a menu)
    let mut found: Vec<(String, String, Node<'_>, bool, bool)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for a in root.descendants().filter(|n| n.tag() == Some("a")) {
        if found.len() >= MAX_SUB_CATEGORIES {
            break;
        }
        let Some(href) = a.attr("href").map(str::trim) else {
            continue;
        };
        if href.is_empty()
            || href.starts_with('#')
            || href.to_ascii_lowercase().starts_with("javascript:")
        {
            continue;
        }
        let Some(mut url) = base.unwrap_or(page).join(href).ok() else {
            continue;
        };
        url.set_fragment(None);
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str() != page.host_str()
            || url.path().trim_end_matches('/') == own_path
            || items.contains(url.as_str())
        {
            continue;
        }
        let name = a.text().split_whitespace().collect::<Vec<_>>().join(" ");
        let lower = name.to_ascii_lowercase();
        if name.is_empty()
            || name.chars().count() > 60
            || FURNITURE.iter().any(|w| {
                lower == *w
                    || lower.starts_with(&format!("{w} "))
                    || lower.ends_with(&format!(" {w}"))
                    || lower.starts_with("my ")
            })
            || footer(a)
        {
            continue;
        }
        let strong = marked(a) || child(&url);
        let menu = in_menu(a);
        if !(strong || (home && menu)) {
            continue;
        }
        let url = String::from(url);
        if seen.insert(url.clone()) {
            found.push((url, name, a, strong, menu));
        }
    }
    // The main menu counts only on a home page without category lists.
    if found.iter().any(|f| f.3) {
        found.retain(|f| f.3);
    }
    let source = source(found.first()?.2);
    let list = found
        .into_iter()
        .map(|(url, name, ..)| {
            let mut m = Map::new();
            m.insert("url".into(), Value::String(url));
            m.insert("name".into(), Value::String(name));
            Value::Object(m)
        })
        .collect();
    Some((list, source))
}
