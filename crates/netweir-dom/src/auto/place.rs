//! Business places: a shop's, a restaurant's or a surgery's page, from
//! its schema.org LocalBusiness (or a kind of one), Facebook's business
//! tags, or the address, phone number and opening hours it shows.

use serde_json::{Map, Value, json};

use super::fields as f;
use super::schema_org::{Source, Thing, of_type};
use super::{Builder, Page};
use crate::Node;

pub(crate) const FIELDS: &[&str] = &[
    "url",
    "place_id",
    "name",
    "actions",
    "additional_properties",
    "address",
    "reservation_action",
    "categories",
    "description",
    "features",
    "map",
    "images",
    "amenity_features",
    "aggregate_rating",
    "star_rating",
    "contained_in_place",
    "opening_hours",
    "review_sites",
    "telephone",
    "price_range",
    "timezone",
    "is_verified",
    "website",
    "tags",
];

/// What a place that may be the site's rather than the page's counts
/// for in `page_type`: just over what types a page at all, so any other
/// type the page reads as wins.
const FALLBACK: f64 = 0.41;

/// Types too general to be a place's category.
const GENERAL: [&str; 5] = [
    "Place",
    "LocalBusiness",
    "Organization",
    "Corporation",
    "Thing",
];

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    // A place inside another thing (an event's venue, a hotel's suite)
    // isn't the page's.
    let places: Vec<&Thing> = of_type(&page.things, "Place")
        .into_iter()
        .filter(|t| !t.nested)
        .collect();
    let mut chosen: Vec<&Thing> = Vec::new();
    let mut several = false;
    for source in [Source::JsonLd, Source::Microdata] {
        let here: Vec<&&Thing> = places.iter().filter(|t| t.source == source).collect();
        if let Some(first) = here.first() {
            // Several places, the first linking to another page: a list
            // of branches or a directory, not a place's own page.
            several |= here.len() > 1 && links_elsewhere(page, first);
            if here.len() > 1 {
                item.note(
                    "item",
                    format!("the page has {} places; read the first", here.len()),
                );
            }
            chosen.push(first);
        }
    }
    for place in &chosen {
        from_thing(page, place, item);
    }
    let business = page
        .meta("og:type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("business.business"));
    if business {
        from_meta(page, item);
    }
    let layout = layout(page, item);
    // A business's structured data describes the business wherever it's
    // printed, often on every page of its site: a shop's home page, a
    // product, a blog post. It types a page only when nothing else does.
    if !chosen.is_empty() {
        item.type_cap = Some(FALLBACK);
    }
    let named = chosen
        .iter()
        .any(|t| t.props.contains_key("name") && t.props.contains_key("address"));
    item.probability = if several {
        0.3
    } else if named {
        0.95
    } else if !chosen.is_empty() {
        0.8
    } else if business {
        0.6
    } else {
        layout.unwrap_or(0.05)
    };
}

fn links_elsewhere(page: &Page<'_, '_>, thing: &Thing) -> bool {
    thing
        .props
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|u| !page.is_page_url(u))
}

fn from_thing(page: &Page<'_, '_>, place: &Thing, item: &mut Builder) {
    let p = &place.props;
    let get = |k: &str| p.get(k).unwrap_or(&Value::Null);
    let src = |k: &str| page.source(place, k);
    let string = |k: &str| f::text(get(k)).map(Value::String);
    item.offer("name", string("name"), src("name"));
    let address = f::with_geo(f::address(get("address")), get("geo"));
    item.offer("address", address, src("address"));
    item.offer(
        "reservation_action",
        each(get("potentialAction"))
            .filter_map(Value::as_object)
            .find(|a| is_type(a, "ReserveAction"))
            .and_then(|a| reservation(a, page)),
        src("potentialAction"),
    );
    let mut categories: Vec<String> = place
        .types
        .iter()
        .filter(|t| !GENERAL.contains(&t.as_str()))
        .cloned()
        .collect();
    for cuisine in each(get("servesCuisine")).filter_map(f::text) {
        if !categories.contains(&cuisine) {
            categories.push(cuisine);
        }
    }
    item.offer(
        "categories",
        (!categories.is_empty()).then(|| json!(categories)),
        src("@type"),
    );
    item.offer("description", string("description"), src("description"));
    let map = match get("hasMap") {
        Value::Object(o) => o.get("url").and_then(Value::as_str),
        other => other.as_str(),
    };
    item.offer(
        "map",
        map.and_then(|m| f::absolute(m, page.base))
            .map(Value::String),
        src("hasMap"),
    );
    let images = if p.contains_key("image") {
        get("image")
    } else {
        get("photo")
    };
    item.offer("images", f::images(images, page.base), src("image"));
    let amenities: Vec<Value> = each(get("amenityFeature"))
        .filter_map(Value::as_object)
        .filter_map(|a| {
            let name = a.get("name").and_then(f::text)?;
            let value = match a.get("value")? {
                Value::Bool(b) => *b,
                Value::String(s) if s.eq_ignore_ascii_case("true") => true,
                Value::String(s) if s.eq_ignore_ascii_case("false") => false,
                _ => return None,
            };
            Some(json!({"name": name, "value": value}))
        })
        .collect();
    item.offer(
        "amenity_features",
        (!amenities.is_empty()).then_some(Value::Array(amenities)),
        src("amenityFeature"),
    );
    item.offer(
        "aggregate_rating",
        f::rating(get("aggregateRating")),
        src("aggregateRating"),
    );
    let star = match get("starRating") {
        Value::Object(o) => o.get("ratingValue"),
        _ => None,
    };
    item.offer(
        "star_rating",
        star.and_then(|v| {
            let raw = f::text(v)?;
            Some(json!({"raw": raw, "rating_value": raw.parse::<f64>().ok()}))
        }),
        src("starRating"),
    );
    item.offer(
        "contained_in_place",
        match get("containedInPlace") {
            Value::Object(o) => o
                .get("name")
                .and_then(f::text)
                .map(|name| json!({"name": name, "place_id": null})),
            _ => None,
        },
        src("containedInPlace"),
    );
    let hours = if p.contains_key("openingHoursSpecification") {
        "openingHoursSpecification"
    } else {
        "openingHours"
    };
    item.offer("opening_hours", f::opening_hours(get(hours)), src(hours));
    item.offer("telephone", string("telephone"), src("telephone"));
    item.offer("price_range", string("priceRange"), src("priceRange"));
    let page_host = page.page.and_then(|u| u.host_str().map(str::to_string));
    let website = get("url")
        .as_str()
        .and_then(|u| f::absolute(u, page.base))
        .filter(|u| {
            let host = url::Url::parse(u)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string));
            host.is_some() && host != page_host
        });
    item.offer("website", website.map(Value::String), src("url"));
}

fn is_type(o: &Map<String, Value>, ty: &str) -> bool {
    each(o.get("@type").unwrap_or(&Value::Null)).any(|t| t.as_str() == Some(ty))
}

/// `{name, url}` from a ReserveAction: its name, and its target's page.
fn reservation(action: &Map<String, Value>, page: &Page<'_, '_>) -> Option<Value> {
    let target = action.get("target")?;
    let url = match target {
        Value::Object(o) => ["urlTemplate", "url"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str)),
        other => other.as_str(),
    }?;
    Some(json!({
        "name": action.get("name").and_then(f::text),
        "url": f::absolute(url, page.base)?,
    }))
}

/// Facebook's business tags: `business:contact_data:street_address`
/// and the rest.
fn from_meta(page: &Page<'_, '_>, item: &mut Builder) {
    let tag = |k: &str| {
        page.meta(&format!("business:contact_data:{k}"))
            .and_then(f::text)
    };
    let mut parts = Map::new();
    for (k, prop) in [
        ("street_address", "streetAddress"),
        ("locality", "addressLocality"),
        ("region", "addressRegion"),
        ("postal_code", "postalCode"),
        ("country_name", "addressCountry"),
    ] {
        if let Some(v) = tag(k) {
            parts.insert(prop.into(), Value::String(v));
        }
    }
    let mut address = f::address(&Value::Object(parts));
    let coordinate = |k: &str| {
        page.meta(&format!("place:location:{k}"))
            .and_then(f::text)
            .and_then(|s| s.parse::<f64>().ok())
    };
    if let (Some(a), Some(lat), Some(lon)) = (
        address.as_mut(),
        coordinate("latitude"),
        coordinate("longitude"),
    ) {
        a["latitude"] = json!(lat);
        a["longitude"] = json!(lon);
    }
    item.offer("address", address, "meta:business:contact_data".into());
    item.offer(
        "telephone",
        tag("phone_number").map(Value::String),
        "meta:business:contact_data:phone_number".into(),
    );
    item.offer(
        "name",
        page.meta("og:title").and_then(f::text).map(Value::String),
        "meta:og:title".into(),
    );
}

/// The place a page shows without structured data: a phone link, an
/// `<address>`, and opening hours or a map. All three, or nothing.
fn layout(page: &Page<'_, '_>, item: &mut Builder) -> Option<f64> {
    let root = page.root;
    let all: Vec<Node<'_>> = root.descendants().collect();
    let phone = all.iter().find(|n| {
        n.tag() == Some("a")
            && n.attr("href").is_some_and(|h| {
                h.trim()
                    .get(..4)
                    .is_some_and(|s| s.eq_ignore_ascii_case("tel:"))
            })
    })?;
    let address = all.iter().find(|n| n.tag() == Some("address"))?;
    let address_text = words(*address)?;
    let map = all.iter().find(|n| {
        n.tag() == Some("a")
            && n.attr("href").is_some_and(|h| {
                [
                    "maps.google.",
                    "google.com/maps",
                    "openstreetmap.org",
                    "maps.apple.com",
                    "bing.com/maps",
                ]
                .iter()
                .any(|m| h.contains(m))
            })
    });
    let hours = hours(&all);
    if map.is_none() && hours.is_none() {
        return None;
    }
    let number = phone.attr("href").unwrap_or_default().trim()[4..]
        .trim()
        .to_string();
    item.offer(
        "telephone",
        (!number.is_empty()).then_some(Value::String(number)),
        super::dom::source(*phone),
    );
    item.offer(
        "address",
        Some(f::address_text(&address_text)),
        super::dom::source(*address),
    );
    if let Some(m) = map {
        item.offer(
            "map",
            m.attr("href")
                .and_then(|h| f::absolute(h, page.base))
                .map(Value::String),
            super::dom::source(*m),
        );
    }
    if let Some((list, at)) = hours {
        item.offer("opening_hours", Some(list), super::dom::source(at));
    }
    if let Some(h1) = all.iter().find(|n| n.tag() == Some("h1")) {
        item.offer(
            "name",
            words(*h1).map(Value::String),
            super::dom::source(*h1),
        );
    }
    Some(0.5)
}

/// Opening hours shown as a table or a list, a row a day (or a run of
/// days): the first table or list with three rows that read as hours.
/// Each row is read once, and only its first few dozen characters.
fn hours<'a>(all: &[Node<'a>]) -> Option<(Value, Node<'a>)> {
    let short = |n: Node<'_>| {
        super::dom::short_text(n, 80).map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
    };
    let mut rows: Vec<(Node<'a>, Vec<Value>)> = Vec::new();
    let mut index: std::collections::HashMap<crate::NodeId, usize> = Default::default();
    for row in all {
        let (days, rest) = match row.tag() {
            Some("tr") => {
                let cells: Vec<String> = row
                    .children()
                    .filter(|c| matches!(c.tag(), Some("td" | "th")))
                    .take(4)
                    .filter_map(short)
                    .collect();
                let Some((first, rest)) = cells.split_first() else {
                    continue;
                };
                (first.clone(), rest.join(" "))
            }
            Some("li") => {
                let Some(text) = short(*row) else { continue };
                let at = text
                    .find(|c: char| c.is_ascii_digit())
                    .unwrap_or(text.len());
                (text[..at].to_string(), text[at..].to_string())
            }
            Some("dt") => {
                let Some(dd) = std::iter::successors(row.next_sibling(), |n| n.next_sibling())
                    .find(|n| n.tag().is_some())
                    .filter(|n| n.tag() == Some("dd"))
                else {
                    continue;
                };
                (
                    short(*row).unwrap_or_default(),
                    short(dd).unwrap_or_default(),
                )
            }
            _ => continue,
        };
        let found = f::hours_row(&days, &rest);
        if found.is_empty() {
            continue;
        }
        // A table's rows sit under its tbody; a list's items under it.
        let Some(container) = std::iter::successors(row.parent(), |n| n.parent())
            .take(2)
            .find(|n| matches!(n.tag(), Some("table" | "ul" | "ol" | "dl")))
        else {
            continue;
        };
        let at = *index.entry(container.id()).or_insert_with(|| {
            rows.push((container, Vec::new()));
            rows.len() - 1
        });
        rows[at].1.push(Value::Array(found));
    }
    rows.into_iter()
        .find(|(_, r)| r.len() >= 3)
        .map(|(container, r)| {
            let flat: Vec<Value> = r
                .into_iter()
                .flat_map(|v| v.as_array().cloned().unwrap_or_default())
                .collect();
            (Value::Array(flat), container)
        })
}

fn words(n: Node<'_>) -> Option<String> {
    let t = crate::markdown::clean_text(n);
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
