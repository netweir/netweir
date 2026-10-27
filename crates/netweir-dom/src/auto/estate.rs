//! Property pages: a house or flat for sale or to rent, from schema.org's
//! RealEstateListing (or the Accommodation it's about), or from the
//! price, rooms and floor area the page shows.

use serde_json::{Map, Value, json};

use super::fields as f;
use super::schema_org::{Source, Thing};
use super::{Builder, Page};
use crate::Node;

pub(crate) const FIELDS: &[&str] = &[
    "url",
    "breadcrumbs",
    "real_estate_id",
    "name",
    "date_published",
    "date_published_raw",
    "description",
    "main_image",
    "images",
    "address",
    "area",
    "number_of_bathrooms_total",
    "number_of_full_bathrooms",
    "number_of_partial_bathrooms",
    "number_of_bedrooms",
    "number_of_rooms",
    "trade_type",
    "price",
    "rental_period",
    "currency_raw",
    "currency",
    "additional_properties",
    "property_type",
    "year_built",
    "virtual_tour_url",
];

/// What a schema.org type of home is called, as people say it.
const KINDS: [(&str, &str); 7] = [
    ("Apartment", "apartment"),
    ("House", "house"),
    ("SingleFamilyResidence", "house"),
    ("Room", "room"),
    ("Suite", "suite"),
    ("Residence", "residence"),
    ("ApartmentComplex", "apartment complex"),
];

fn is_home(t: &Thing) -> bool {
    t.types.iter().any(|ty| {
        ty == "RealEstateListing" || ty == "Accommodation" || KINDS.iter().any(|(k, _)| k == ty)
    })
}

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    // A listing's home, or a hotel's suite, is read with what holds it.
    let homes: Vec<&Thing> = page
        .things
        .iter()
        .filter(|t| is_home(t) && !t.nested)
        .collect();
    let mut chosen: Vec<&Thing> = Vec::new();
    let mut several = false;
    for source in [Source::JsonLd, Source::Microdata] {
        let here: Vec<&Thing> = homes
            .iter()
            .copied()
            .filter(|t| t.source == source)
            .collect();
        if let Some(first) = here.first() {
            several |= here.len() > 1
                && first
                    .props
                    .get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|u| !page.is_page_url(u));
            if here.len() > 1 {
                item.note(
                    "item",
                    format!("the page has {} properties; read the first", here.len()),
                );
            }
            chosen.push(first);
        }
    }
    for home in &chosen {
        from_thing(page, home, item);
    }
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
    let layout = layout(page, item);
    item.probability = if several {
        0.3
    } else if !chosen.is_empty() && item.has("price") {
        0.95
    } else if !chosen.is_empty() {
        0.8
    } else {
        layout.unwrap_or(0.05)
    };
}

fn from_thing(page: &Page<'_, '_>, home: &Thing, item: &mut Builder) {
    // A listing's facts about the home sit in the home it's `about`.
    let inner = ["about", "mainEntity", "itemOffered"]
        .iter()
        .find_map(|k| home.props.get(*k).and_then(Value::as_object));
    let empty = Map::new();
    let inner = inner.unwrap_or(&empty);
    let get = |k: &str| {
        home.props
            .get(k)
            .or_else(|| inner.get(k))
            .unwrap_or(&Value::Null)
    };
    let src = |k: &str| page.source(home, k);
    let id = match get("identifier") {
        Value::Object(o) => o.get("value").and_then(f::text),
        Value::Null => f::text(get("sku")),
        other => f::text(other),
    };
    item.offer("real_estate_id", id.map(Value::String), src("identifier"));
    item.offer("name", f::text(get("name")).map(Value::String), src("name"));
    let posted = if home.props.contains_key("datePosted") {
        get("datePosted")
    } else {
        get("datePublished")
    };
    item.offer(
        "date_published",
        f::iso_date(posted, page.now).map(Value::String),
        src("datePosted"),
    );
    item.offer(
        "date_published_raw",
        f::text(posted).map(Value::String),
        src("datePosted"),
    );
    item.offer(
        "description",
        f::text(get("description")).map(Value::String),
        src("description"),
    );
    let image = if home.props.contains_key("image") || inner.contains_key("image") {
        get("image")
    } else {
        get("photo")
    };
    item.offer("main_image", f::image(image, page.base), src("image"));
    item.offer("images", f::images(image, page.base), src("image"));
    item.offer(
        "address",
        f::with_geo(f::address(get("address")), get("geo")),
        src("address"),
    );
    let sized = |k: &str, kind: &str| {
        f::area(get(k)).map(|mut a| {
            a["area_type"] = json!(kind);
            a
        })
    };
    item.offer(
        "area",
        sized("floorSize", "FLOOR").or_else(|| sized("lotSize", "LOT")),
        src("floorSize"),
    );
    for (field, prop) in [
        ("number_of_bathrooms_total", "numberOfBathroomsTotal"),
        ("number_of_full_bathrooms", "numberOfFullBathrooms"),
        ("number_of_partial_bathrooms", "numberOfPartialBathrooms"),
        ("number_of_bedrooms", "numberOfBedrooms"),
        ("number_of_rooms", "numberOfRooms"),
        ("year_built", "yearBuilt"),
    ] {
        item.offer(field, f::whole(get(prop)).map(Value::from), src(prop));
    }
    item.offer(
        "virtual_tour_url",
        get("tourBookingPage")
            .as_str()
            .and_then(|u| f::absolute(u, page.base))
            .map(Value::String),
        src("tourBookingPage"),
    );
    let kind = f::text(get("accommodationCategory"))
        .map(|c| c.to_lowercase())
        .or_else(|| {
            inner
                .get("@type")
                .into_iter()
                .chain(std::iter::once(&Value::Null))
                .filter_map(Value::as_str)
                .chain(home.types.iter().map(String::as_str))
                .find_map(|ty| {
                    KINDS
                        .iter()
                        .find(|(k, _)| *k == ty)
                        .map(|(_, v)| v.to_string())
                })
        });
    item.offer("property_type", kind.map(Value::String), src("@type"));
    let offer = match get("offers") {
        Value::Array(list) => list.first().and_then(Value::as_object),
        Value::Object(o) => Some(o),
        _ => None,
    };
    if let Some(offer) = offer {
        let spec = match offer.get("priceSpecification") {
            Some(Value::Array(list)) => list.first().and_then(Value::as_object),
            Some(Value::Object(o)) => Some(o),
            _ => None,
        };
        let price = offer
            .get("price")
            .or_else(|| spec.and_then(|s| s.get("price")))
            .unwrap_or(&Value::Null);
        let money = f::price_fields(price);
        let currency = f::currency(
            offer
                .get("priceCurrency")
                .or_else(|| spec.and_then(|s| s.get("priceCurrency")))
                .unwrap_or(&Value::Null),
        );
        item.offer("price", money.price.map(Value::String), src("offers"));
        item.offer(
            "currency",
            currency.currency.or(money.currency).map(Value::String),
            src("offers"),
        );
        item.offer(
            "currency_raw",
            currency
                .currency_raw
                .or(money.currency_raw)
                .map(Value::String),
            src("offers"),
        );
        let period = spec
            .and_then(|s| s.get("unitCode").or_else(|| s.get("unitText")))
            .and_then(f::text)
            .and_then(|u| period(&u));
        let function = offer
            .get("businessFunction")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let trade = if function.ends_with("LeaseOut") || period.is_some() {
            Some("rent")
        } else if function.ends_with("Sell") {
            Some("sale")
        } else {
            None
        };
        item.offer("rental_period", period.map(Value::from), src("offers"));
        item.offer("trade_type", trade.map(Value::from), src("offers"));
    }
}

/// `monthly` and the like, from a UN/CEFACT code (MON, WEE, ANN) or
/// words such as `pcm`, `per week` or `/yr`, matched as whole words.
fn period(text: &str) -> Option<&'static str> {
    let t = text.trim().to_ascii_lowercase();
    match t.as_str() {
        "mon" => return Some("monthly"),
        "wee" => return Some("weekly"),
        "ann" => return Some("yearly"),
        _ => {}
    }
    // Words apart, `/` kept: "£1,250/mo" is "1 250/mo".
    let spaced: String = t
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '/' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let spaced = format!(
        " {} ",
        spaced.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let has = |words: &[&str]| {
        words.iter().any(|w| {
            spaced.contains(&format!(" {w} "))
                || (w.starts_with('/') && spaced.contains(&format!("{w} ")))
        })
    };
    if has(&[
        "pcm",
        "per month",
        "a month",
        "/month",
        "/mo",
        "monthly",
        "p/m",
        "per calendar month",
    ]) {
        Some("monthly")
    } else if has(&["pw", "per week", "a week", "/week", "/wk", "weekly", "p/w"]) {
        Some("weekly")
    } else if has(&["per quarter", "quarterly"]) {
        Some("quarterly")
    } else if has(&[
        "per year",
        "per annum",
        "a year",
        "/year",
        "/yr",
        "yearly",
        "annually",
        "pa",
        "p/a",
    ]) {
        Some("yearly")
    } else {
        None
    }
}

/// A property a page shows without structured data: a price beside its
/// heading, and two of bedrooms, bathrooms and floor area.
fn layout(page: &Page<'_, '_>, item: &mut Builder) -> Option<f64> {
    static BEDS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(\d{1,2})\s*(?:-\s*)?bed(?:room)?s?\b|\bbed(?:room)?s?\s*:?\s*(\d{1,2})\b",
        )
        .unwrap()
    });
    static BATHS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            // "Bath: 2" is as likely the city; the label form wants
            // `baths` or `bathroom(s)`.
            r"(?i)\b(\d{1,2})\s*(?:-\s*)?bath(?:room)?s?\b|\b(?:baths|bathrooms?)\s*:?\s*(\d{1,2})\b",
        )
        .unwrap()
    });
    static ROOMS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\b(\d{1,2})\s*rooms?\b").unwrap());
    static BUILT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:built in|year built\s*:?|built\s*:)\s*(\d{4})\b").unwrap()
    });
    let root = page.root;
    let main = root
        .descendants()
        .find(|n| n.tag() == Some("main"))
        .unwrap_or(root);
    let (price_node, money) = shown_price(main)?;
    // Each text, once: an element's own words, not all it holds, so a
    // page nested thousands deep is read in one pass.
    let texts: Vec<(Node<'_>, String)> = main
        .descendants()
        .filter(|n| n.kind() == crate::NodeKind::Text)
        .filter_map(|n| {
            let t = n.data()?.split_whitespace().collect::<Vec<_>>().join(" ");
            Some((n.parent()?, t)).filter(|(_, t)| !t.is_empty())
        })
        .collect();
    // A property's facts come near the top of its page; reading only
    // that much keeps a page of a million words quick.
    let mut text = words(main).unwrap_or_default();
    if text.len() > READ {
        let mut end = READ;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    // The fact, and the element that says it whole (or the main content
    // when it's split across elements).
    let fact = |re: &regex::Regex| -> Option<(i64, Node<'_>)> {
        let number = |t: &str| {
            let c = re.captures(t)?;
            c.get(1).or(c.get(2))?.as_str().parse::<i64>().ok()
        };
        let value = number(&text)?;
        let at = texts
            .iter()
            .find(|(_, t)| number(t) == Some(value))
            .map_or(main, |(n, _)| holder(*n));
        Some((value, at))
    };
    let beds = fact(&BEDS);
    let baths = fact(&BATHS);
    let area = f::area(&Value::String(text.clone()));
    if [beds.is_some(), baths.is_some(), area.is_some()]
        .iter()
        .filter(|b| **b)
        .count()
        < 2
    {
        return None;
    }
    let src = |n: Node<'_>| super::dom::source(n);
    if let Some((n, at)) = beds {
        item.offer("number_of_bedrooms", Some(Value::from(n)), src(at));
    }
    if let Some((n, at)) = baths {
        item.offer("number_of_bathrooms_total", Some(Value::from(n)), src(at));
    }
    if let Some((n, at)) = fact(&ROOMS).filter(|(n, _)| Some(*n) != beds.map(|b| b.0)) {
        item.offer("number_of_rooms", Some(Value::from(n)), src(at));
    }
    item.offer("area", area, src(main));
    if let Some(year) = BUILT.captures(&text).and_then(|c| c[1].parse::<i64>().ok()) {
        item.offer("year_built", Some(Value::from(year)), src(main));
    }
    item.offer("price", money.price.map(Value::String), src(price_node));
    item.offer(
        "currency",
        money.currency.map(Value::String),
        src(price_node),
    );
    item.offer(
        "currency_raw",
        money.currency_raw.map(Value::String),
        src(price_node),
    );
    // The rent's period is written with the price (`£1,250 pcm`) or
    // right after it.
    let after = std::iter::successors(price_node.next_sibling(), |n| n.next_sibling())
        .take(3)
        .find_map(|n| super::dom::short_text(n, 40))
        .unwrap_or_default();
    let beside = format!(
        "{} {after}",
        super::dom::short_text(price_node, 60).unwrap_or_default()
    );
    let rental = period(&beside);
    let lower = text.to_lowercase();
    // A rent's period beside the price says rent; otherwise the page's
    // own "For sale" outranks a mention of renting elsewhere on it.
    let trade = if rental.is_some() {
        Some("rent")
    } else if lower.contains("for sale") {
        Some("sale")
    } else if ["for rent", "to rent", "to let", "for lease"]
        .iter()
        .any(|w| lower.contains(w))
    {
        Some("rent")
    } else {
        None
    };
    item.offer("rental_period", rental.map(Value::from), src(price_node));
    item.offer("trade_type", trade.map(Value::from), src(main));
    if let Some(h1) = root.descendants().find(|n| n.tag() == Some("h1")) {
        item.offer("name", words(h1).map(Value::String), src(h1));
    }
    let address = root.descendants().find(|n| {
        n.tag() == Some("address") || n.attr("itemprop").is_some_and(|p| p.trim() == "address")
    });
    if let Some(a) = address {
        item.offer("address", words(a).map(|t| f::address_text(&t)), src(a));
    }
    Some(if address.is_some() { 0.6 } else { 0.55 })
}

/// How much of the main content's text the facts are read from.
const READ: usize = 20_000;

/// The price the page shows: the first short text in the main content
/// that reads as an amount with a currency, one marked as a price
/// (`class="price"`) first. Not in a link, which would be another
/// property's.
fn shown_price(main: Node<'_>) -> Option<(Node<'_>, f::PriceFields)> {
    // A property's own price comes early; a page of thousands of prices
    // isn't one property's, and isn't read to the end.
    const LOOKED: usize = 200;
    let mut first = None;
    let mut looked = 0;
    for t in main
        .descendants()
        .filter(|n| n.kind() == crate::NodeKind::Text)
    {
        let Some(n) = t.parent() else { continue };
        let raw = t.data().unwrap_or_default();
        if raw.len() > 80 || !raw.bytes().any(|b| b.is_ascii_digit()) {
            continue;
        }
        looked += 1;
        if looked > LOOKED {
            break;
        }
        let Some(words) = super::dom::short_text(n, 40)
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        else {
            continue;
        };
        let money = f::price_text(&words);
        if money.price.is_none() || money.currency_raw.is_none() {
            continue;
        }
        if std::iter::successors(Some(n), |a| a.parent()).any(|a| a.tag() == Some("a")) {
            continue;
        }
        if super::dom::marked(n, &super::dom::PRICE_WORDS) {
            return Some((n, money));
        }
        first.get_or_insert((n, money));
    }
    first
}

/// A list item's list, a table cell's row: the element a fact sits in
/// along with its neighbours, which is what a person would pin.
fn holder(n: Node<'_>) -> Node<'_> {
    match n.tag() {
        Some("li" | "span" | "dd" | "td") => n.parent().unwrap_or(n),
        _ => n,
    }
}

fn words(n: Node<'_>) -> Option<String> {
    let t = crate::markdown::clean_text(n);
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}
