//! Product pages: the schema.org Product (or ProductGroup) the page is
//! about, its offer, and Open Graph's `product:` tags as a fallback.

use serde_json::{Map, Value, json};

use super::fields::{self as f, PriceFields};
use super::schema_org::{Source, Thing, of_type};
use super::{Builder, Page};

pub(crate) const FIELDS: &[&str] = &[
    "url",
    "canonical_url",
    "name",
    "price",
    "regular_price",
    "currency",
    "currency_raw",
    "availability",
    "sku",
    "mpn",
    "gtin",
    "product_id",
    "brand",
    "main_image",
    "images",
    "description",
    "description_html",
    "features",
    "additional_properties",
    "aggregate_rating",
    "breadcrumbs",
    "color",
    "size",
    "style",
    "variants",
];

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    let candidates = candidates(&page.things);
    let mut chosen = Vec::new();
    for source in [Source::JsonLd, Source::Microdata] {
        let here: Vec<&Thing> = candidates
            .iter()
            .copied()
            .filter(|t| t.source == source)
            .collect();
        if let Some(main) = main_product(page, &here) {
            if here.len() > 1 {
                item.note(
                    "item",
                    format!(
                        "the page's {} has {} products; read {}",
                        if source == Source::JsonLd {
                            "JSON-LD"
                        } else {
                            "microdata"
                        },
                        here.len(),
                        f::text(main.props.get("name").unwrap_or(&Value::Null))
                            .map_or("the first".into(), |n| format!("{n:?}"))
                    ),
                );
            }
            chosen.push(main);
        }
    }
    for thing in &chosen {
        from_thing(page, thing, item);
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
    let og_product = page
        .meta("og:type")
        .and_then(Value::as_str)
        .is_some_and(|t| {
            t.trim().eq_ignore_ascii_case("product") || t.trim().eq_ignore_ascii_case("og:product")
        });
    if !chosen.is_empty() || og_product {
        from_meta(page, item);
    }
    let strong = chosen.iter().any(|t| {
        let p = &t.props;
        p.contains_key("name")
            && (p.contains_key("offers") || p.contains_key("sku") || item.has("gtin"))
    });
    item.probability = match (strong, !chosen.is_empty(), og_product) {
        (true, _, _) => 0.95,
        (_, true, _) => 0.8,
        (_, _, true) => 0.5,
        _ => 0.05,
    };
}

/// Products that aren't someone else's variant, related product or
/// accessory: those are read as part of the product they hang off.
pub(super) fn candidates(things: &[Thing]) -> Vec<&Thing> {
    let products = of_type(things, "Product");
    let mut nested: Vec<&Map<String, Value>> = Vec::new();
    for p in &products {
        for (key, value) in &p.props {
            if key == "offers" {
                continue;
            }
            for v in each(value) {
                if let Value::Object(o) = v {
                    nested.push(o);
                }
            }
        }
    }
    products
        .into_iter()
        .filter(|p| {
            !nested.iter().any(|n| {
                n.iter().filter(|(k, _)| !k.starts_with('@')).count() == p.props.len()
                    && p.props.iter().all(|(k, v)| n.get(k) == Some(v))
            })
        })
        .collect()
}

/// The product the page is about: the one whose URL is the page's,
/// else the first with an offer, else the first.
fn main_product<'t>(page: &Page<'_, '_>, products: &[&'t Thing]) -> Option<&'t Thing> {
    let canonical = page
        .meta("canonical")
        .and_then(Value::as_str)
        .and_then(|c| f::absolute(c, page.base));
    let same = |u: &str| {
        let Some(u) = f::absolute(u, page.base) else {
            return false;
        };
        let strip = |s: &str| {
            s.split('#')
                .next()
                .unwrap_or(s)
                .trim_end_matches('/')
                .to_string()
        };
        page.page.is_some_and(|p| strip(p.as_str()) == strip(&u))
            || canonical.as_deref().is_some_and(|c| strip(c) == strip(&u))
    };
    products
        .iter()
        .find(|p| p.props.get("url").and_then(Value::as_str).is_some_and(same))
        .or_else(|| products.iter().find(|p| p.props.contains_key("offers")))
        .or_else(|| products.first())
        .copied()
}

fn from_thing(page: &Page<'_, '_>, thing: &Thing, item: &mut Builder) {
    let p = &thing.props;
    let src = |prop: &str| page.source(thing, prop);
    let get = |k: &str| p.get(k).unwrap_or(&Value::Null);
    item.offer("name", f::text(get("name")).map(Value::String), src("name"));
    if let Some(offer) = main_offer(p) {
        let money = offer_money(offer);
        // In microdata the offer is an item of its own, whose properties
        // know their elements: the price's source is its <span>.
        let offer_thing = thing.element_of("offers").and_then(|e| {
            page.things
                .iter()
                .find(|t| t.source == Source::Microdata && t.element == Some(e))
        });
        let osrc = |prop: &str| match offer_thing {
            Some(o) if o.element_of(prop).is_some() => page.source(o, prop),
            _ => src("offers"),
        };
        let price_prop = if offer.contains_key("price") {
            "price"
        } else {
            "lowPrice"
        };
        item.offer("price", money.price.map(Value::String), osrc(price_prop));
        item.offer(
            "currency",
            money.currency.map(Value::String),
            osrc("priceCurrency"),
        );
        item.offer(
            "currency_raw",
            money.currency_raw.map(Value::String),
            osrc("priceCurrency"),
        );
        let offer_src = src("offers");
        if let Some(regular) =
            regular_price(offer).filter(|r| item.get("price").as_str() != Some(r))
        {
            item.offer(
                "regular_price",
                Some(Value::String(regular)),
                offer_src.clone(),
            );
        }
        item.offer(
            "availability",
            offer
                .get("availability")
                .and_then(f::availability)
                .map(Value::String),
            osrc("availability"),
        );
    }
    // The product's own code before its offer's.
    item.offer("sku", f::text(get("sku")).map(Value::String), src("sku"));
    if let Some(offer) = main_offer(p) {
        item.offer(
            "sku",
            offer.get("sku").and_then(f::text).map(Value::String),
            src("offers"),
        );
    }
    item.offer("mpn", f::text(get("mpn")).map(Value::String), src("mpn"));
    let codes = ["gtin13", "gtin12", "gtin14", "gtin8", "gtin", "isbn"]
        .into_iter()
        .find(|k| p.contains_key(*k))
        .unwrap_or("gtin");
    item.offer("gtin", f::gtins(p), src(codes));
    let id = p.get("productID").or_else(|| p.get("productGroupID"));
    item.offer(
        "product_id",
        id.and_then(f::text).map(Value::String),
        src(if p.contains_key("productID") {
            "productID"
        } else {
            "productGroupID"
        }),
    );
    item.offer("brand", f::brand(get("brand")), src("brand"));
    item.offer(
        "main_image",
        f::image(get("image"), page.base),
        src("image"),
    );
    item.offer("images", f::images(get("image"), page.base), src("image"));
    item.offer(
        "description",
        f::text(get("description")).map(Value::String),
        src("description"),
    );
    item.offer(
        "additional_properties",
        properties(get("additionalProperty")),
        src("additionalProperty"),
    );
    item.offer(
        "aggregate_rating",
        f::rating(get("aggregateRating")),
        src("aggregateRating"),
    );
    item.offer(
        "color",
        f::text(get("color")).map(Value::String),
        src("color"),
    );
    item.offer("size", size(get("size")).map(Value::String), src("size"));
    if thing.is("ProductGroup") || p.contains_key("hasVariant") {
        let variants: Vec<Value> = each(get("hasVariant"))
            .filter_map(|v| v.as_object())
            .map(|v| variant(page, v))
            .filter(|v| !v.is_empty())
            .map(Value::Object)
            .collect();
        if !variants.is_empty() {
            item.offer("variants", Some(Value::Array(variants)), src("hasVariant"));
        }
    }
}

/// One variant of a ProductGroup, with only the fields it has.
fn variant(page: &Page<'_, '_>, v: &Map<String, Value>) -> Map<String, Value> {
    let get = |k: &str| v.get(k).unwrap_or(&Value::Null);
    let mut out = Map::new();
    let mut put = |k: &str, value: Option<Value>| {
        if let Some(value) = value.filter(|x| !x.is_null()) {
            out.insert(k.into(), value);
        }
    };
    put("name", f::text(get("name")).map(Value::String));
    put("sku", f::text(get("sku")).map(Value::String));
    put("mpn", f::text(get("mpn")).map(Value::String));
    put("product_id", f::text(get("productID")).map(Value::String));
    put("gtin", f::gtins(v));
    put("color", f::text(get("color")).map(Value::String));
    put("size", size(get("size")).map(Value::String));
    put(
        "url",
        get("url")
            .as_str()
            .and_then(|u| f::absolute(u, page.base))
            .map(Value::String),
    );
    if let Some(offer) = main_offer(v) {
        let money = offer_money(offer);
        put("price", money.price.map(Value::String));
        put("regular_price", regular_price(offer).map(Value::String));
        put("currency", money.currency.map(Value::String));
        put("currency_raw", money.currency_raw.map(Value::String));
        put(
            "availability",
            offer
                .get("availability")
                .and_then(f::availability)
                .map(Value::String),
        );
    }
    put("main_image", f::image(get("image"), page.base));
    out
}

/// The offer to read the price from: the first with a price.
pub(super) fn main_offer(p: &Map<String, Value>) -> Option<&Map<String, Value>> {
    let offers: Vec<&Map<String, Value>> = each(p.get("offers").unwrap_or(&Value::Null))
        .filter_map(Value::as_object)
        .collect();
    offers
        .iter()
        .find(|o| {
            ["price", "lowPrice", "priceSpecification"]
                .iter()
                .any(|k| o.contains_key(*k))
        })
        .or(offers.first())
        .copied()
}

/// An offer's price and currency: `price`, else an AggregateOffer's
/// `lowPrice`, else its price specification that isn't a list price.
pub(super) fn offer_money(offer: &Map<String, Value>) -> PriceFields {
    let specs: Vec<&Map<String, Value>> =
        each(offer.get("priceSpecification").unwrap_or(&Value::Null))
            .filter_map(Value::as_object)
            .collect();
    let current = specs.iter().find(|s| !is_list_price(s));
    let price_value = offer
        .get("price")
        .or_else(|| offer.get("lowPrice"))
        .or_else(|| current.and_then(|s| s.get("price")));
    let mut money = price_value.map(f::price_fields).unwrap_or_default();
    let code = offer
        .get("priceCurrency")
        .or_else(|| current.and_then(|s| s.get("priceCurrency")));
    if let Some(c) = code.map(f::currency).filter(|c| c.currency_raw.is_some()) {
        money.currency = c.currency;
        money.currency_raw = c.currency_raw;
    }
    money
}

fn is_list_price(spec: &Map<String, Value>) -> bool {
    spec.get("priceType")
        .and_then(Value::as_str)
        .is_some_and(|t| {
            ["ListPrice", "StrikethroughPrice", "MSRP", "SRP"]
                .iter()
                .any(|k| t.ends_with(k))
        })
}

/// The price before a discount: a list or strikethrough price.
pub(super) fn regular_price(offer: &Map<String, Value>) -> Option<String> {
    each(offer.get("priceSpecification").unwrap_or(&Value::Null))
        .filter_map(Value::as_object)
        .find(|s| is_list_price(s))
        .and_then(|s| s.get("price"))
        .and_then(|p| f::price_fields(p).price)
}

fn properties(v: &Value) -> Option<Value> {
    let list: Vec<Value> = each(v)
        .filter_map(Value::as_object)
        .filter_map(|p| {
            let name = f::text(p.get("name")?)?;
            let value = f::text(p.get("value")?)?;
            Some(json!({"name": name, "value": value}))
        })
        .collect();
    (!list.is_empty()).then_some(Value::Array(list))
}

/// A size as words: `"M"`, or a SizeSpecification's name.
fn size(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => o.get("name").and_then(f::text),
        other => f::text(other),
    }
}

fn from_meta(page: &Page<'_, '_>, item: &mut Builder) {
    let m = |k: &str| page.meta(k).cloned();
    let label = |k: &str| format!("meta:{k}");
    if let Some(c) = m("canonical").or_else(|| m("og:url")) {
        let key = if page.meta("canonical").is_some() {
            "canonical"
        } else {
            "og:url"
        };
        item.offer(
            "canonical_url",
            c.as_str()
                .and_then(|u| f::absolute(u, page.base))
                .map(Value::String),
            label(key),
        );
    }
    let text = |k: &str| m(k).as_ref().and_then(f::text).map(Value::String);
    item.offer("name", text("og:title"), label("og:title"));
    item.offer(
        "description",
        text("og:description"),
        label("og:description"),
    );
    if let Some(img) = m("og:image") {
        item.offer("main_image", f::image(&img, page.base), label("og:image"));
        item.offer("images", f::images(&img, page.base), label("og:image"));
    }
    for (amount, currency) in [
        ("product:price:amount", "product:price:currency"),
        ("og:price:amount", "og:price:currency"),
    ] {
        if let Some(a) = m(amount) {
            let mut money = f::price_fields(&a);
            if let Some(c) = m(currency)
                .map(|c| f::currency(&c))
                .filter(|c| c.currency_raw.is_some())
            {
                money.currency = c.currency;
                money.currency_raw = c.currency_raw;
            }
            item.offer("price", money.price.map(Value::String), label(amount));
            item.offer(
                "currency",
                money.currency.map(Value::String),
                label(currency),
            );
            item.offer(
                "currency_raw",
                money.currency_raw.map(Value::String),
                label(currency),
            );
        }
    }
    item.offer(
        "availability",
        m("product:availability")
            .as_ref()
            .and_then(f::availability)
            .map(Value::String),
        label("product:availability"),
    );
    item.offer(
        "brand",
        m("product:brand").as_ref().and_then(f::brand),
        label("product:brand"),
    );
    item.offer(
        "sku",
        text("product:retailer_item_id"),
        label("product:retailer_item_id"),
    );
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
