//! Category and search pages: the products a page lists (ProductList)
//! and where it leads (ProductNavigation), from schema.org lists and
//! `rel=next` links.

use serde_json::{Map, Value};

use super::fields as f;
use super::schema_org::{Source, Thing};
use super::{Builder, Page};
use super::{dom, product};

pub(crate) const LIST_FIELDS: &[&str] = &[
    "url",
    "canonical_url",
    "products",
    "breadcrumbs",
    "category_name",
    "page_number",
    "pagination_next",
];

pub(crate) const NAVIGATION_FIELDS: &[&str] = &[
    "url",
    "category_name",
    "sub_categories",
    "items",
    "next_page",
    "page_number",
];

/// The products a page lists, where the list came from, and its name.
struct Listed {
    products: Vec<Map<String, Value>>,
    source: String,
    name: Option<(String, String)>,
}

pub(crate) fn read_list(page: &Page<'_, '_>, item: &mut Builder) {
    let listed = listed(page);
    if let Some(l) = &listed {
        let products = l.products.iter().cloned().map(Value::Object).collect();
        item.offer("products", Some(Value::Array(products)), l.source.clone());
        if let Some((name, source)) = &l.name {
            item.offer(
                "category_name",
                Some(Value::String(name.clone())),
                source.clone(),
            );
        }
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
    let cards = listed
        .is_none()
        .then(|| dom::listing(page.root, page.base))
        .flatten();
    if let Some(c) = &cards {
        let products = c.products.iter().cloned().map(Value::Object).collect();
        item.offer("products", Some(Value::Array(products)), c.source.clone());
        if let Some((name, source)) = &c.heading {
            item.offer(
                "category_name",
                Some(Value::String(name.clone())),
                source.clone(),
            );
        }
    }
    if let Some(trail) = dom::breadcrumbs(page.root) {
        item.offer(
            "breadcrumbs",
            f::from_element(f::FieldKind::Breadcrumbs, trail, page.base, page.now),
            dom::source(trail),
        );
    }
    let m = |k: &str| page.meta(k).and_then(Value::as_str);
    if let Some((key, c)) = m("canonical")
        .map(|c| ("canonical", c))
        .or_else(|| m("og:url").map(|c| ("og:url", c)))
    {
        item.offer(
            "canonical_url",
            f::absolute(c, page.base).map(Value::String),
            format!("meta:{key}"),
        );
    }
    // A pager on a page with no listing is a thread's or an archive's.
    let listing = listed.is_some() || cards.is_some();
    let next = next_link(page, listing);
    if let Some((url, text, source)) = &next {
        let mut link = Map::new();
        link.insert("url".into(), Value::String(url.clone()));
        if let Some(t) = text {
            link.insert("text".into(), Value::String(t.clone()));
        }
        item.offer("pagination_next", Some(Value::Object(link)), source.clone());
    }
    if let Some((n, source)) = dom::page_number(page.root, page.page).filter(|_| listing) {
        item.offer("page_number", Some(Value::from(n)), source);
    }
    item.probability = probability(listed.as_ref(), cards.as_ref(), next.is_some());
}

pub(crate) fn read_navigation(page: &Page<'_, '_>, item: &mut Builder) {
    let listed = listed(page);
    if let Some(l) = &listed {
        let items: Vec<Value> = l
            .products
            .iter()
            .filter_map(|p| {
                let mut out = Map::new();
                out.insert("url".into(), p.get("url")?.clone());
                if let Some(name) = p.get("name") {
                    out.insert("name".into(), name.clone());
                }
                Some(Value::Object(out))
            })
            .collect();
        if !items.is_empty() {
            item.offer("items", Some(Value::Array(items)), l.source.clone());
        }
        if let Some((name, source)) = &l.name {
            item.offer(
                "category_name",
                Some(Value::String(name.clone())),
                source.clone(),
            );
        }
    }
    let cards = listed
        .is_none()
        .then(|| dom::listing(page.root, page.base))
        .flatten();
    if let Some(c) = &cards {
        let items: Vec<Value> = c
            .products
            .iter()
            .filter_map(|p| {
                let mut out = Map::new();
                out.insert("url".into(), p.get("url")?.clone());
                if let Some(name) = p.get("name") {
                    out.insert("name".into(), name.clone());
                }
                Some(Value::Object(out))
            })
            .collect();
        if !items.is_empty() {
            item.offer("items", Some(Value::Array(items)), c.source.clone());
        }
        if let Some((name, source)) = &c.heading {
            item.offer(
                "category_name",
                Some(Value::String(name.clone())),
                source.clone(),
            );
        }
    }
    // A pager on a page with no listing is a thread's or an archive's.
    let listing = listed.is_some() || cards.is_some();
    let next = next_link(page, listing);
    if let Some((url, text, source)) = &next {
        let mut request = Map::new();
        request.insert("url".into(), Value::String(url.clone()));
        if let Some(t) = text {
            request.insert("name".into(), Value::String(t.clone()));
        }
        item.offer("next_page", Some(Value::Object(request)), source.clone());
    }
    if let Some((n, source)) = dom::page_number(page.root, page.page).filter(|_| listing) {
        item.offer("page_number", Some(Value::from(n)), source);
    }
    item.probability = probability(listed.as_ref(), cards.as_ref(), next.is_some());
}

fn probability(listed: Option<&Listed>, cards: Option<&dom::Listing>, next: bool) -> f64 {
    match (listed.map_or(0, |l| l.products.len()), cards) {
        (n, _) if n >= 2 => 0.9,
        (1, _) => 0.6,
        // A product page's related products are a list too, but not the
        // page's.
        (_, Some(c)) if c.product_page => 0.3,
        (_, Some(_)) => 0.75,
        _ if next => 0.2,
        _ => 0.05,
    }
}

/// The page's list of products: the first schema.org ItemList (or
/// OfferCatalog) that names any, else the Products on the page when
/// there are several.
fn listed(page: &Page<'_, '_>) -> Option<Listed> {
    for list in page
        .things
        .iter()
        .filter(|t| t.is("ItemList") && !t.is("BreadcrumbList"))
    {
        let elements = sorted(list.props.get("itemListElement").unwrap_or(&Value::Null));
        let products: Vec<Map<String, Value>> = elements
            .into_iter()
            .filter_map(|e| entry(page, e))
            .collect();
        if products.is_empty() {
            continue;
        }
        let name = list
            .props
            .get("name")
            .and_then(f::text)
            .map(|n| (n, page.source(list, "name")))
            .or_else(|| collection_name(page));
        return Some(Listed {
            products,
            source: label(list.source),
            name,
        });
    }
    for source in [Source::JsonLd, Source::Microdata] {
        let products: Vec<&Thing> = product::candidates(&page.things)
            .into_iter()
            .filter(|t| t.source == source)
            .collect();
        if products.len() >= 2 {
            return Some(Listed {
                products: products
                    .iter()
                    .map(|t| from_product(page, &t.props, None))
                    .filter(|p| !p.is_empty())
                    .collect(),
                source: label(source),
                name: collection_name(page),
            });
        }
    }
    // Apps that render in the browser keep their list in their state.
    super::js_state::listing(page)
        .filter(|(products, _)| products.len() >= 2)
        .map(|(products, source)| Listed {
            products,
            source,
            name: collection_name(page),
        })
}

fn label(source: Source) -> String {
    match source {
        Source::Microdata => "microdata".into(),
        _ => "json-ld".into(),
    }
}

/// The name of the CollectionPage or SearchResultsPage the list is on.
fn collection_name(page: &Page<'_, '_>) -> Option<(String, String)> {
    page.things
        .iter()
        .filter(|t| {
            t.types
                .iter()
                .any(|ty| ty == "CollectionPage" || ty == "SearchResultsPage")
        })
        .find_map(|t| {
            Some((
                t.props.get("name").and_then(f::text)?,
                page.source(t, "name"),
            ))
        })
}

/// List elements in position order, those without one where they stand.
fn sorted(v: &Value) -> Vec<&Value> {
    let mut list: Vec<(f64, usize, &Value)> = each(v)
        .enumerate()
        .map(|(i, e)| {
            let position = e.get("position").and_then(|p| match p {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            });
            (position.unwrap_or(f64::MAX), i, e)
        })
        .collect();
    list.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    list.into_iter().map(|(_, _, e)| e).collect()
}

/// One listed product: a ListItem's Product, an Offer's itemOffered, a
/// bare Product, or a ListItem with only a URL and a name.
fn entry(page: &Page<'_, '_>, e: &Value) -> Option<Map<String, Value>> {
    let o = match e {
        Value::String(url) => {
            let mut out = Map::new();
            out.insert("url".into(), Value::String(f::absolute(url, page.base)?));
            return Some(out);
        }
        Value::Object(o) => o,
        _ => return None,
    };
    let is = |o: &Map<String, Value>, ty: &str| {
        o.get("@type").is_some_and(|t| match t {
            Value::String(s) => s == ty,
            Value::Array(l) => l.iter().any(|x| x.as_str() == Some(ty)),
            _ => false,
        })
    };
    let out = match o.get("item") {
        Some(Value::Object(inner)) if is(inner, "Offer") => offered(page, inner),
        Some(Value::Object(inner)) => from_product(page, inner, None),
        Some(Value::String(url)) => {
            let mut out = from_product(page, o, None);
            if let Some(u) = f::absolute(url, page.base) {
                out.insert("url".into(), Value::String(u));
            }
            out
        }
        _ if is(o, "Offer") => offered(page, o),
        _ => from_product(page, o, None),
    };
    (!out.is_empty() && (out.contains_key("url") || out.contains_key("name"))).then_some(out)
}

/// An Offer's product, with the offer's price.
fn offered(page: &Page<'_, '_>, offer: &Map<String, Value>) -> Map<String, Value> {
    match offer.get("itemOffered") {
        Some(Value::Object(p)) => from_product(page, p, Some(offer)),
        _ => Map::new(),
    }
}

/// A product as a list shows it: name, price, regular price, currency,
/// URL, image and ID, with only the fields it has.
fn from_product(
    page: &Page<'_, '_>,
    p: &Map<String, Value>,
    offer: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let mut out = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v.filter(|v| !v.is_null()) {
            out.insert(k.into(), v);
        }
    };
    let get = |k: &str| p.get(k).unwrap_or(&Value::Null);
    put("name", f::text(get("name")).map(Value::String));
    let offer = offer.or_else(|| product::main_offer(p));
    if let Some(offer) = offer {
        let money = product::offer_money(offer);
        put("price", money.price.map(Value::String));
        put(
            "regular_price",
            product::regular_price(offer).map(Value::String),
        );
        put("currency", money.currency.map(Value::String));
        put("currency_raw", money.currency_raw.map(Value::String));
    }
    put(
        "url",
        get("url")
            .as_str()
            .and_then(|u| f::absolute(u, page.base))
            .map(Value::String),
    );
    put("main_image", f::image(get("image"), page.base));
    put("product_id", f::text(get("productID")).map(Value::String));
    out
}

/// The next page: `<link rel=next>` or `<a rel=next>`, its URL absolute,
/// the link's text if it has any, and where it was.
/// The next page: `rel=next`, else, on a page with a listing, its
/// pagination widget's next link.
fn next_link(page: &Page<'_, '_>, listing: bool) -> Option<(String, Option<String>, String)> {
    rel_next(page).or_else(|| {
        listing
            .then(|| dom::next_page(page.root, page.page, page.base))
            .flatten()
    })
}

fn rel_next(page: &Page<'_, '_>) -> Option<(String, Option<String>, String)> {
    page.root.descendants().find_map(|n| {
        let tag = n.tag()?;
        if !matches!(tag, "link" | "a") {
            return None;
        }
        let rel = n.attr("rel")?;
        if !rel
            .split_whitespace()
            .any(|r| r.eq_ignore_ascii_case("next"))
        {
            return None;
        }
        let url = f::absolute(n.attr("href")?, page.base)?;
        let text = f::text(&Value::String(n.text()));
        Some((url, text, format!("dom:{tag}[rel=next]")))
    })
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
