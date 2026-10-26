//! The data apps keep in their scripts: the state Next.js, Nuxt, Apollo
//! and Redux-style apps hand the browser to build the page from. A page
//! like that often has no product in its HTML at all, but the state has
//! it, as an object with a name, a price and the rest.
//!
//! Objects are found by their shape, not by where a framework puts them:
//! every object in the state is weighed by the keys it has, and the best
//! product (or job) wins. Lists of three or more of them are a listing.

use serde_json::{Map, Value, json};

use super::fields::{self as f, PriceFields};
use super::{Builder, Page};
use crate::js::{self, JsValue};
use crate::{Node, NodeKind};

/// Values looked at in all of a page's state at most.
const BUDGET: usize = 100_000;

/// Entries of a list weighed to decide whether it's a list of products.
const LIST_SAMPLE: usize = 20;

/// Entries of one list read as a listing at most.
const MAX_LISTED: usize = 1000;

/// How deep the walk goes.
const MAX_DEPTH: usize = 64;

/// Names apps assign their state to.
const NAMES: [&str; 8] = [
    "__APOLLO_STATE__",
    "__INITIAL_STATE__",
    "__PRELOADED_STATE__",
    "__STATE__",
    "__APP_STATE__",
    "__INITIAL_DATA__",
    "__DATA__",
    "__remixContext",
];

/// The page's app state, each with the name it was found under.
fn roots(root: Node<'_>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let scripts: Vec<Node<'_>> = std::iter::once(root)
        .chain(root.descendants())
        .filter(|n| n.kind() == NodeKind::Element && n.tag() == Some("script"))
        .collect();
    // JSON scripts with an id like `__NEXT_DATA__`: Next.js's, Nuxt 3's
    // and others that follow the habit.
    for s in &scripts {
        let Some(id) = s.attr("id").filter(|i| i.starts_with("__") && i.len() > 4) else {
            continue;
        };
        let kind = s.attr("type").unwrap_or_default().to_ascii_lowercase();
        if !kind.contains("json") {
            continue;
        }
        if let Some(v) = js::scripts(*s, Some(id)).into_iter().next() {
            let v = to_json(v);
            let v = if id == "__NUXT_DATA__" {
                devalue(&v)
            } else {
                Some(v)
            };
            if let Some(v) = v.filter(|v| !v.is_null()) {
                out.push((id.to_string(), v));
            }
        }
    }
    for s in &scripts {
        let kind = s.attr("type").unwrap_or_default().to_ascii_lowercase();
        if kind.contains("json") {
            continue;
        }
        let text = s.text();
        if text.contains("__NUXT__")
            && let Some(v) = js::nuxt(&text)
        {
            out.push(("__NUXT__".into(), to_json(v)));
        }
        for name in NAMES {
            if text.contains(name)
                && !out.iter().any(|(n, _)| n == name)
                && let Some(v) = js::assigned(&text, name)
            {
                let v = to_json(v);
                let v = if name == "__APOLLO_STATE__" {
                    apollo(v)
                } else {
                    v
                };
                out.push((name.to_string(), v));
            }
        }
    }
    let mut budget = BUDGET;
    out.into_iter()
        .map(|(name, v)| (name, apollo_within(v, 0, &mut budget)))
        .collect()
}

/// `v` with any Apollo cache inside it (an object with a `ROOT_QUERY`, as
/// Next.js apps pass `initialApolloState`) resolved.
fn apollo_within(v: Value, depth: usize, budget: &mut usize) -> Value {
    if depth > MAX_DEPTH || *budget == 0 {
        return v;
    }
    *budget -= 1;
    match v {
        Value::Object(o) if o.contains_key("ROOT_QUERY") => apollo(Value::Object(o)),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| (k, apollo_within(x, depth + 1, budget)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|x| apollo_within(x, depth + 1, budget))
                .collect(),
        ),
        other => other,
    }
}

fn to_json(v: JsValue) -> Value {
    match v {
        JsValue::Null => Value::Null,
        JsValue::Bool(b) => Value::Bool(b),
        JsValue::Int(i) => Value::from(i),
        JsValue::BigInt(s) => Value::String(s),
        JsValue::Float(x) => serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number),
        JsValue::Str(s) => Value::String(s),
        JsValue::Array(items) => Value::Array(items.into_iter().map(to_json).collect()),
        JsValue::Object(members) => {
            Value::Object(members.into_iter().map(|(k, v)| (k, to_json(v))).collect())
        }
    }
}

/// Nuxt 3's state, which `devalue` writes as one flat array: each object
/// and array holds the indexes of its members, and the first entry is
/// the root. Wrappers such as `["Reactive", 1]` stand for what they wrap.
fn devalue(flat: &Value) -> Option<Value> {
    let all = flat.as_array()?;
    let mut budget = BUDGET;
    fn at(all: &[Value], i: i64, depth: usize, budget: &mut usize) -> Option<Value> {
        if i < 0 {
            return Some(Value::Null);
        }
        *budget = budget.checked_sub(1)?;
        if depth > MAX_DEPTH {
            return None;
        }
        let v = all.get(i as usize)?;
        let index = |x: &Value| x.as_i64();
        Some(match v {
            Value::Array(items) => match items.first().and_then(Value::as_str) {
                Some(
                    "Reactive" | "ShallowReactive" | "Ref" | "ShallowRef" | "EmptyRef"
                    | "EmptyShallowRef",
                ) => match items.get(1).and_then(index) {
                    Some(j) => at(all, j, depth + 1, budget)?,
                    None => Value::Null,
                },
                Some("Date" | "BigInt" | "RegExp") => items.get(1).cloned().unwrap_or_default(),
                // Anything else tagged (`NuxtError`, `Island`, `URL`, a
                // Map or Set) isn't data a page is about.
                Some(_) => Value::Null,
                _ => Value::Array(
                    items
                        .iter()
                        .map(|x| index(x).and_then(|j| at(all, j, depth + 1, budget)))
                        .collect::<Option<_>>()?,
                ),
            },
            Value::Object(members) => Value::Object(
                members
                    .iter()
                    .map(|(k, x)| Some((k.clone(), at(all, index(x)?, depth + 1, budget)?)))
                    .collect::<Option<_>>()?,
            ),
            other => other.clone(),
        })
    }
    at(all, 0, 0, &mut budget)
}

/// Apollo's normalised cache, with each `{"__ref": "Product:42"}` replaced
/// by the entry it names (entries already being read stay references).
fn apollo(state: Value) -> Value {
    let Value::Object(entries) = state else {
        return state;
    };
    let mut budget = BUDGET;
    fn resolve(
        v: &Value,
        entries: &Map<String, Value>,
        open: &mut Vec<String>,
        depth: usize,
        budget: &mut usize,
    ) -> Value {
        if *budget == 0 || depth > MAX_DEPTH {
            return Value::Null;
        }
        *budget -= 1;
        match v {
            Value::Object(o) => {
                if let Some(Value::String(key)) = o.get("__ref")
                    && o.len() == 1
                {
                    if open.contains(key) {
                        return v.clone();
                    }
                    let Some(target) = entries.get(key) else {
                        return Value::Null;
                    };
                    open.push(key.clone());
                    let out = resolve(target, entries, open, depth + 1, budget);
                    open.pop();
                    return out;
                }
                Value::Object(
                    o.iter()
                        .map(|(k, x)| (k.clone(), resolve(x, entries, open, depth + 1, budget)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|x| resolve(x, entries, open, depth + 1, budget))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut out = Map::new();
    for (key, entry) in &entries {
        let mut open = vec![key.clone()];
        out.insert(
            key.clone(),
            resolve(entry, &entries, &mut open, 0, &mut budget),
        );
    }
    Value::Object(out)
}

/// Whether key `k`, lowercased and without `_` or `-`, is `want`.
fn key_is(k: &str, want: &str) -> bool {
    let mut got = k
        .bytes()
        .filter(|b| *b != b'_' && *b != b'-')
        .map(|b| b.to_ascii_lowercase());
    let mut want = want.bytes();
    loop {
        match (got.next(), want.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

/// The first of `keys` the object has with a value, and the key as
/// written.
fn get<'o>(o: &'o Map<String, Value>, keys: &[&str]) -> Option<(&'o str, &'o Value)> {
    keys.iter().find_map(|want| {
        o.iter()
            .find(|(k, v)| key_is(k, want) && !v.is_null())
            .map(|(k, v)| (k.as_str(), v))
    })
}

const NAME: [&str; 5] = [
    "name",
    "title",
    "productname",
    "displayname",
    "producttitle",
];
/// Price keys, the price after any discount first: beside `salePrice`
/// or `finalPrice`, `price` is the one before it.
const PRICE: [&str; 11] = [
    "saleprice",
    "specialprice",
    "finalprice",
    "currentprice",
    "offerprice",
    "sellingprice",
    "price",
    "priceamount",
    "priceincents",
    "pricecents",
    "amountincents",
];

/// Inside a price object, its parts: the current amount first.
const AMOUNT: [&str; 13] = [
    "sale",
    "special",
    "now",
    "final",
    "amount",
    "value",
    "centamount",
    "current",
    "raw",
    "minvariantprice",
    "min",
    "formatted",
    "display",
];

/// Inside a price object, the amount before a discount.
const BEFORE: [&str; 6] = ["regular", "was", "list", "original", "compareat", "old"];
const REGULAR: [&str; 9] = [
    "compareatprice",
    "listprice",
    "originalprice",
    "regularprice",
    "wasprice",
    "oldprice",
    "msrp",
    "rrp",
    "strikethroughprice",
];
const CURRENCY: [&str; 4] = ["currency", "currencycode", "pricecurrency", "currencyiso"];
const CENTS: [&str; 5] = [
    "centamount",
    "priceincents",
    "pricecents",
    "amountincents",
    "cents",
];

/// A money value as apps write it: a number, a string, or an object with
/// an amount (`{amount, currencyCode}`, commercetools' `centAmount`,
/// Shopify's `minVariantPrice`), with its currency.
fn money(key: &str, v: &Value, depth: usize) -> PriceFields {
    if depth > 4 {
        return PriceFields::default();
    }
    let cents = CENTS.iter().any(|c| key_is(key, c));
    let whole = |v: &Value| match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    let mut out = match v {
        Value::Number(_) | Value::String(_) if cents => PriceFields {
            price: whole(v).and_then(|c| scaled(c, 2)),
            ..Default::default()
        },
        Value::Number(_) | Value::String(_) => f::price_fields(v),
        Value::Object(o) => {
            let inner = get(o, &AMOUNT);
            let mut m = inner
                .map(|(k, x)| money(k, x, depth + 1))
                .unwrap_or_default();
            if let Some((k, x)) = inner
                && key_is(k, "centamount")
                && let Some(digits) = o.get("fractionDigits").and_then(Value::as_u64)
            {
                m.price = whole(x).and_then(|c| scaled(c, digits));
            }
            if let Some((_, c)) = get(o, &CURRENCY) {
                let c = f::currency(c);
                if c.currency_raw.is_some() {
                    m.currency = c.currency;
                    m.currency_raw = c.currency_raw;
                }
            }
            m
        }
        _ => PriceFields::default(),
    };
    if out
        .price
        .as_deref()
        .is_some_and(|p| p.parse::<f64>().is_err())
    {
        out.price = None;
    }
    out
}

/// `c` hundredths (or `digits`-ths) as a decimal string: 5900 at 2 is
/// `59.00`, -50 is `-0.50`. More than six places isn't money.
fn scaled(c: i64, digits: u64) -> Option<String> {
    if digits > 6 {
        return None;
    }
    if digits == 0 {
        return Some(c.to_string());
    }
    let scale = 10u64.pow(digits as u32);
    let n = c.unsigned_abs();
    let sign = if c < 0 { "-" } else { "" };
    Some(format!(
        "{sign}{}.{:0w$}",
        n / scale,
        n % scale,
        w = digits as usize
    ))
}

/// What a product object says, in the item's fields, each with the path
/// below the object it came from (`.price`, `.price.regular`).
fn product_fields(
    o: &Map<String, Value>,
    base: Option<&url::Url>,
) -> Vec<(&'static str, Value, String)> {
    let mut out: Vec<(&'static str, Value, String)> = Vec::new();
    // `key` is a key of the object, or a path below it already written.
    let mut put = |field: &'static str, value: Option<Value>, key: &str| {
        if let Some(v) = value.filter(|v| !v.is_null()) {
            let path = if key.starts_with(['.', '[']) {
                key.to_string()
            } else {
                key_part(key)
            };
            out.push((field, v, path));
        }
    };
    if let Some((k, v)) = get(o, &NAME) {
        put("name", f::text(v).map(Value::String), k);
    }
    let price = get(o, &PRICE).map(|(k, v)| (k, money(k, v, 0)));
    let mut currency = price
        .as_ref()
        .and_then(|(_, m)| m.currency.clone().zip(m.currency_raw.clone()));
    let currency_key = get(o, &CURRENCY);
    if currency.is_none()
        && let Some((_, c)) = currency_key
    {
        let c = f::currency(c);
        currency = c
            .currency
            .zip(c.currency_raw.clone())
            .or(c.currency_raw.map(|raw| (String::new(), raw)));
    }
    if let Some((k, m)) = &price {
        put("price", m.price.clone().map(Value::String), k);
        let n = |s: &Option<String>| s.as_deref().and_then(|x| x.parse::<f64>().ok());
        // The old price: a key that says so, `price` beside a sale price,
        // or the part of a price object that says so.
        let inside = o
            .get(*k)
            .and_then(Value::as_object)
            .and_then(|po| get(po, &BEFORE));
        let plain = (!key_is(k, "price")).then(|| get(o, &["price"])).flatten();
        let regular = [(get(o, &REGULAR), false), (plain, false), (inside, true)]
            .into_iter()
            .filter_map(|(found, nested)| Some((found?, nested)))
            .map(|((rk, rv), nested)| (rk, money(rk, rv, 0), nested))
            .find(|(_, r, _)| n(&r.price) > n(&m.price));
        if let Some((rk, r, nested)) = regular {
            let path = if nested {
                format!("{}{}", key_part(k), key_part(rk))
            } else {
                key_part(rk)
            };
            put("regular_price", r.price.map(Value::String), &path);
        }
    }
    let ckey = price
        .as_ref()
        .filter(|(_, m)| m.currency_raw.is_some())
        .map(|(k, _)| *k)
        .or(currency_key.map(|(k, _)| k))
        .unwrap_or("currency");
    if let Some((code, raw)) = currency {
        put(
            "currency",
            (!code.is_empty()).then_some(Value::String(code)),
            ckey,
        );
        put("currency_raw", Some(Value::String(raw)), ckey);
    }
    if let Some((k, v)) = get(
        o,
        &[
            "availability",
            "stockstatus",
            "instock",
            "isinstock",
            "available",
            "availableforsale",
            "isavailable",
        ],
    ) {
        let value = match v {
            Value::Bool(true) => Some("InStock".to_string()),
            Value::Bool(false) => Some("OutOfStock".to_string()),
            other => f::availability(&Value::String(
                f::text(other).unwrap_or_default().replace(['_', '-'], " "),
            ))
            .or_else(|| f::availability(other)),
        };
        put("availability", value.map(Value::String), k);
    }
    if let Some((k, v)) = get(o, &["sku", "skuid", "skucode"]) {
        put("sku", f::text(v).map(Value::String), k);
    }
    if let Some((k, v)) = get(o, &["mpn"]) {
        put("mpn", f::text(v).map(Value::String), k);
    }
    if let Some((k, v)) = get(o, &["gtin", "gtin13", "gtin12", "ean", "upc", "barcode"]) {
        let mut one = Map::new();
        one.insert("gtin".into(), Value::String(f::text(v).unwrap_or_default()));
        put("gtin", f::gtins(&one), k);
    }
    if let Some((k, v)) = get(o, &["id", "productid", "itemid"])
        && matches!(v, Value::String(_) | Value::Number(_))
    {
        put("product_id", f::text(v).map(Value::String), k);
    }
    if let Some((k, v)) = get(o, &["brand", "vendor", "manufacturer"]) {
        put("brand", f::brand(v), k);
    }
    if let Some((k, v)) = get(
        o,
        &[
            "images",
            "image",
            "imageurl",
            "featuredimage",
            "media",
            "thumbnail",
            "picture",
        ],
    ) {
        let urls = image_urls(v);
        let list = Value::Array(urls.into_iter().map(Value::String).collect());
        put("main_image", f::image(&list, base), k);
        put("images", f::images(&list, base), k);
    }
    if let Some((k, v)) = get(o, &["description", "shortdescription"]) {
        put("description", f::text(v).map(Value::String), k);
        if let Some(html) = v.as_str().filter(|d| d.contains('<')) {
            let cleaned = crate::content::clean_html_fragment(html);
            put(
                "description_html",
                (!cleaned.is_empty()).then_some(Value::String(cleaned)),
                k,
            );
        }
    }
    if let Some((k, v)) = get(o, &["descriptionhtml", "bodyhtml"])
        && let Some(html) = v.as_str()
    {
        let cleaned = crate::content::clean_html_fragment(html);
        put(
            "description_html",
            (!cleaned.is_empty()).then_some(Value::String(cleaned)),
            k,
        );
    }
    if let Some((k, v)) = get(o, &["url", "href", "link", "permalink", "canonicalurl"])
        && let Some(u) = v
            .as_str()
            .filter(|u| u.starts_with('/') || u.starts_with("http"))
    {
        put("url", f::absolute(u, base).map(Value::String), k);
    }
    out
}

/// Image URLs from a URL, a list, or objects with `url`, `src` or
/// `originalSrc`, Shopify's `edges[].node` included.
fn image_urls(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(v, 0)];
    while let Some((v, depth)) = stack.pop() {
        if depth > 4 || out.len() >= 50 {
            continue;
        }
        match v {
            Value::String(s) if !s.trim().is_empty() => out.push(s.trim().to_string()),
            Value::Array(items) => stack.extend(items.iter().rev().map(|x| (x, depth + 1))),
            Value::Object(o) => {
                if let Some((_, u)) = get(o, &["url", "src", "originalsrc", "href", "large"]) {
                    stack.push((u, depth + 1));
                } else if let Some((_, inner)) = get(o, &["edges", "node", "nodes", "image"]) {
                    stack.push((inner, depth + 1));
                }
            }
            _ => {}
        }
    }
    out
}

/// How much an object looks like a product: it needs a name and a price;
/// each code, image, brand, currency or description adds to it, as does
/// a type name that says Product.
fn product_score(o: &Map<String, Value>) -> u32 {
    let has = |keys: &[&str]| get(o, keys).is_some();
    let named = get(o, &NAME).is_some_and(|(_, v)| v.is_string());
    let priced = get(o, &PRICE).is_some_and(|(k, v)| money(k, v, 0).price.is_some());
    if !named || !priced {
        return 0;
    }
    let mut score = 2;
    for keys in [
        &[
            "sku",
            "skuid",
            "id",
            "productid",
            "gtin",
            "ean",
            "upc",
            "mpn",
        ][..],
        &[
            "images",
            "image",
            "imageurl",
            "featuredimage",
            "media",
            "thumbnail",
        ],
        &["brand", "vendor", "manufacturer"],
        &CURRENCY,
        &["description", "descriptionhtml"],
        &REGULAR,
    ] {
        if has(keys) {
            score += 1;
        }
    }
    if o.get("__typename")
        .or_else(|| o.get("@type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t.to_ascii_lowercase().contains("product"))
    {
        score += 3;
    }
    score
}

/// An object found in the state: where it is, how well it fits, and
/// whether it sits in a list (at any level above it).
struct Found<'v> {
    step: usize,
    object: &'v Map<String, Value>,
    score: u32,
    listed: bool,
}

/// One step of a path into the state. Paths are built only for the
/// values that are kept, so a long key over many children costs nothing.
enum Seg<'v> {
    Root(&'v str),
    Key(&'v str),
    Index(usize),
}

/// Every step taken, each with the index of the step it hangs off.
struct Steps<'v>(Vec<(usize, Seg<'v>)>);

/// `.key`, or `["a.b"]` for a key a dot or bracket would make ambiguous.
fn key_part(k: &str) -> String {
    if !k.is_empty() && !k.contains(['.', '[', ']', '"']) {
        format!(".{k}")
    } else {
        format!("[{}]", Value::String(k.to_string()))
    }
}

/// A source label for key `k` of the object at `path`.
fn key_label(path: &str, k: &str) -> String {
    format!("{path}{}", key_part(k))
}

impl Steps<'_> {
    /// The path to step `at` as sources write it:
    /// `__NEXT_DATA__.props.items[2]`.
    fn label(&self, mut at: usize) -> String {
        let mut parts = Vec::new();
        loop {
            let (parent, seg) = &self.0[at];
            parts.push(match seg {
                Seg::Root(name) => name.to_string(),
                Seg::Key(k) => key_part(k),
                Seg::Index(i) => format!("[{i}]"),
            });
            if *parent == usize::MAX {
                break;
            }
            at = *parent;
        }
        parts.reverse();
        parts.concat()
    }

    /// The keys on the way to step `at`, nearest first.
    fn keys(&self, mut at: usize) -> impl Iterator<Item = &str> {
        std::iter::from_fn(move || {
            while at != usize::MAX {
                let (parent, seg) = &self.0[at];
                at = *parent;
                if let Seg::Key(k) = seg {
                    return Some(*k);
                }
            }
            None
        })
    }
}

/// A list of objects in the state: the step it's at, and its entries.
type Listed<'v> = (usize, Vec<&'v Map<String, Value>>);

/// Every object in the state that `score` gives a positive score, in the
/// order met, and the lists of three or more such objects.
fn search<'v>(
    roots: &'v [(String, Value)],
    score: impl Fn(&Map<String, Value>) -> u32,
) -> (Steps<'v>, Vec<Found<'v>>, Vec<Listed<'v>>) {
    let mut steps = Steps(Vec::new());
    let mut found = Vec::new();
    let mut lists = Vec::new();
    let mut budget = BUDGET;
    for (name, value) in roots {
        steps.0.push((usize::MAX, Seg::Root(name)));
        let mut stack: Vec<(&Value, usize, bool, usize)> =
            vec![(value, steps.0.len() - 1, false, 0)];
        while let Some((v, step, listed, depth)) = stack.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            if depth > MAX_DEPTH {
                continue;
            }
            match v {
                Value::Object(o) => {
                    let s = score(o);
                    if s > 0 {
                        found.push(Found {
                            step,
                            object: o,
                            score: s,
                            listed,
                        });
                    }
                    for (k, x) in o.iter().rev() {
                        if matches!(x, Value::Object(_) | Value::Array(_)) {
                            steps.0.push((step, Seg::Key(k)));
                            stack.push((x, steps.0.len() - 1, listed, depth + 1));
                        }
                    }
                }
                Value::Array(items) => {
                    // A list is judged by its first entries; each one
                    // weighed counts against the budget.
                    let looked = items.len().min(LIST_SAMPLE).min(budget);
                    budget -= looked;
                    let fits = items[..looked]
                        .iter()
                        .filter_map(Value::as_object)
                        .filter(|o| score(o) > 0)
                        .count();
                    if looked >= 3 && fits >= 3 && fits * 2 >= looked {
                        let all: Vec<&Map<String, Value>> = items
                            .iter()
                            .take(MAX_LISTED)
                            .filter_map(Value::as_object)
                            .collect();
                        lists.push((step, all));
                    }
                    // Past the first thousand, a list's entries are more
                    // of the same.
                    for (i, x) in items.iter().take(MAX_LISTED).enumerate().rev() {
                        if matches!(x, Value::Object(_) | Value::Array(_)) {
                            steps.0.push((step, Seg::Index(i)));
                            // A list of one is a wrapper (Nuxt 2's `data`),
                            // not one of many.
                            let many = listed || items.len() > 1;
                            stack.push((x, steps.0.len() - 1, many, depth + 1));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    (steps, found, lists)
}

/// Words in the keys above an object that say it isn't the page's
/// product or job: a basket's lines, a delivery option, a recommendation.
const ASIDE: [&str; 17] = [
    "cart",
    "basket",
    "bag",
    "wishlist",
    "recent",
    "recommend",
    "related",
    "upsell",
    "crosssell",
    "similar",
    "shipping",
    "delivery",
    "giftwrap",
    "wrap",
    "plan",
    "subscription",
    "option",
];

fn aside(steps: &Steps<'_>, at: usize) -> bool {
    steps.keys(at).any(|k| {
        let k: String = k
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        ASIDE.iter().any(|w| k.contains(w))
    })
}

/// The page's own product in the state: the best fit standing alone
/// (in no list, under no basket or recommendations, and more than a
/// name and a price), else one in a list whose URL is the page's.
fn main_product<'f, 'v>(
    page: &Page<'_, '_>,
    steps: &Steps<'v>,
    found: &'f [Found<'v>],
) -> Option<&'f Found<'v>> {
    let path = page.page.map(|p| p.path().trim_matches('/').to_string());
    let here = |o: &Map<String, Value>| {
        get(o, &["url", "href", "slug", "handle", "permalink"])
            .and_then(|(_, v)| v.as_str())
            .zip(path.as_deref())
            .is_some_and(|(u, p)| {
                let u = u.trim();
                let u = url::Url::parse(u)
                    .map(|x| x.path().to_string())
                    .unwrap_or_else(|_| u.to_string());
                let u = u.trim_matches('/');
                // Whole path segments: `slug: "g"` isn't `/p/thing`.
                !u.is_empty() && (p == u || p.ends_with(&format!("/{u}")))
            })
    };
    let weigh = |f: &&Found<'_>| f.score + if here(f.object) { 3 } else { 0 };
    let usable = |f: &&Found<'_>| !aside(steps, f.step);
    first_best(
        found
            .iter()
            .filter(|f| !f.listed && f.score >= 3)
            .filter(usable),
        weigh,
    )
    .or_else(|| {
        first_best(
            found.iter().filter(|f| here(f.object)).filter(usable),
            weigh,
        )
    })
}

/// The highest-weighed, the first met among equals.
fn first_best<'f, 'v>(
    found: impl Iterator<Item = &'f Found<'v>>,
    weigh: impl Fn(&&'f Found<'v>) -> u32,
) -> Option<&'f Found<'v>>
where
    'v: 'f,
{
    let mut best: Option<(&Found<'_>, u32)> = None;
    for f in found {
        let w = weigh(&f);
        if best.is_none_or(|(_, b)| w > b) {
            best = Some((f, w));
        }
    }
    best.map(|(f, _)| f)
}

/// The product in the page's app state, offered after the structured
/// sources and before meta tags. Returns the probability it gives.
pub(crate) fn product(page: &Page<'_, '_>, item: &mut Builder) -> Option<f64> {
    let roots = roots(page.root);
    if roots.is_empty() {
        return None;
    }
    let (steps, found, _) = search(&roots, product_score);
    let best = main_product(page, &steps, &found)?;
    let path = steps.label(best.step);
    for (field, value, key) in product_fields(best.object, page.base) {
        if field != "url" {
            item.offer(field, Some(value), format!("js:{path}{key}"));
        }
    }
    Some(if best.score >= 5 { 0.85 } else { 0.7 })
}

/// The products the page's app state lists: its longest list of priced
/// product objects, each as a list shows it. None on a product's own
/// page, whose lists are its related products and variants.
pub(crate) fn listing(page: &Page<'_, '_>) -> Option<(Vec<Map<String, Value>>, String)> {
    let roots = roots(page.root);
    if roots.is_empty() {
        return None;
    }
    let (steps, found, lists) = search(&roots, product_score);
    if found
        .iter()
        .any(|f| !f.listed && f.score >= 3 && !aside(&steps, f.step))
    {
        return None;
    }
    let (at, objects) = lists
        .into_iter()
        .filter(|(at, _)| !aside(&steps, *at))
        .max_by_key(|(_, l)| l.len())?;
    let order = [
        "name",
        "price",
        "regular_price",
        "currency",
        "currency_raw",
        "url",
        "main_image",
        "product_id",
    ];
    let products = objects
        .into_iter()
        .map(|o| {
            let fields = product_fields(o, page.base);
            order
                .iter()
                .filter_map(|k| {
                    let (_, v, _) = fields.iter().find(|(f, _, _)| f == k)?;
                    Some((k.to_string(), v.clone()))
                })
                .collect::<Map<String, Value>>()
        })
        .filter(|p| !p.is_empty())
        .collect();
    Some((products, format!("js:{}", steps.label(at))))
}

/// How much an object looks like a job: a title and two of the things
/// adverts give.
fn job_score(o: &Map<String, Value>) -> u32 {
    let titled =
        get(o, &["title", "jobtitle", "name", "position"]).is_some_and(|(_, v)| v.is_string());
    if !titled {
        return 0;
    }
    let facts = [
        &["salary", "basesalary", "compensation", "pay"][..],
        &["location", "joblocation", "city", "address"],
        &["employmenttype", "jobtype", "contracttype"],
        &[
            "company",
            "companyname",
            "employer",
            "hiringorganization",
            "organization",
        ],
        &["dateposted", "postedat", "publishedat", "postingdate"],
        &[
            "validthrough",
            "closingdate",
            "expiresat",
            "deadline",
            "applyby",
        ],
    ]
    .iter()
    .filter(|keys| get(o, keys).is_some())
    .count() as u32;
    let typed = o
        .get("__typename")
        .or_else(|| o.get("@type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t.to_ascii_lowercase().contains("job"));
    // A place, a company and a date are on stories and reviews too; a
    // salary, a job type or an apply link are only on adverts.
    let only_jobs = get(
        o,
        &[
            "salary",
            "basesalary",
            "compensation",
            "employmenttype",
            "jobtype",
            "contracttype",
            "applyurl",
            "applicationurl",
            "applylink",
        ],
    )
    .is_some();
    if (facts >= 2 && only_jobs) || (typed && facts >= 1) {
        facts + if typed { 3 } else { 0 }
    } else {
        0
    }
}

/// The job in the page's app state. Returns the probability it gives.
pub(crate) fn job(page: &Page<'_, '_>, item: &mut Builder) -> Option<f64> {
    let roots = roots(page.root);
    if roots.is_empty() {
        return None;
    }
    let (steps, found, _) = search(&roots, job_score);
    let best = first_best(
        found.iter().filter(|f| !f.listed && !aside(&steps, f.step)),
        |f| f.score,
    )?;
    let o = best.object;
    let path = steps.label(best.step);
    let src = |k: &str| format!("js:{}", key_label(&path, k));
    if let Some((k, v)) = get(o, &["title", "jobtitle", "name", "position"]) {
        item.offer("job_title", f::text(v).map(Value::String), src(k));
    }
    if let Some((k, v)) = get(o, &["id", "jobid", "jobpostingid", "reference"])
        && matches!(v, Value::String(_) | Value::Number(_))
    {
        item.offer("job_posting_id", f::text(v).map(Value::String), src(k));
    }
    if let Some((k, v)) = get(
        o,
        &[
            "company",
            "companyname",
            "employer",
            "hiringorganization",
            "organization",
        ],
    ) {
        let name = match v {
            Value::Object(c) => {
                get(c, &["name", "displayname", "title"]).and_then(|(_, n)| f::text(n))
            }
            other => f::text(other),
        };
        item.offer(
            "hiring_organization",
            name.map(|n| json!({"name": n, "name_raw": n})),
            src(k),
        );
    }
    if let Some((k, v)) = get(o, &["location", "joblocation", "city", "address"]) {
        let raw = match v {
            Value::Object(l) => {
                let parts: Vec<String> = [
                    &["name", "displayname", "formatted"][..],
                    &["street", "streetaddress"],
                    &["city", "locality", "addresslocality"],
                    &["region", "state", "addressregion"],
                    &["postcode", "postalcode", "zip"],
                    &["country", "addresscountry", "countrycode"],
                ]
                .iter()
                .filter_map(|keys| get(l, keys).and_then(|(_, x)| f::text(x)))
                .collect();
                (!parts.is_empty()).then(|| parts.join(", "))
            }
            Value::Array(list) => list.first().and_then(f::text),
            other => f::text(other),
        };
        item.offer("job_location", raw.map(|r| json!({"raw": r})), src(k));
    }
    if let Some((k, v)) = get(o, &["employmenttype", "jobtype", "contracttype"]) {
        item.offer(
            "employment_type",
            f::employment_type(v).map(Value::String),
            src(k),
        );
    }
    if let Some((k, v)) = get(o, &["salary", "basesalary", "compensation", "pay"]) {
        item.offer("base_salary", salary(v), src(k));
    }
    if let Some((k, v)) = get(o, &["description", "jobdescription", "descriptionhtml"]) {
        item.offer("description", f::text(v).map(Value::String), src(k));
        if let Some(html) = v.as_str().filter(|d| d.contains('<')) {
            let cleaned = crate::content::clean_html_fragment(html);
            item.offer(
                "description_html",
                (!cleaned.is_empty()).then_some(Value::String(cleaned)),
                src(k),
            );
        }
    }
    for (field, raw_field, keys) in [
        (
            "date_published",
            "date_published_raw",
            &["dateposted", "postedat", "publishedat", "postingdate"][..],
        ),
        (
            "valid_through",
            "valid_through_raw",
            &[
                "validthrough",
                "closingdate",
                "expiresat",
                "deadline",
                "applyby",
            ],
        ),
        (
            "job_start_date",
            "job_start_date_raw",
            &["startdate", "jobstartdate"],
        ),
    ] {
        if let Some((k, v)) = get(o, keys) {
            item.offer(field, f::iso_date(v, page.now).map(Value::String), src(k));
            item.offer(raw_field, f::text(v).map(Value::String), src(k));
        }
    }
    if let Some((k, v)) = get(o, &["remote", "isremote", "remoteallowed"])
        && v == &Value::Bool(true)
    {
        item.offer("remote_status", Some(json!("Remote")), src(k));
    }
    Some(if best.score >= 4 { 0.8 } else { 0.7 })
}

/// A salary as apps write it: text, a schema.org MonetaryAmount, or
/// `{min, max, currency, period}`.
fn salary(v: &Value) -> Option<Value> {
    let Value::Object(o) = v else {
        return f::salary(v);
    };
    if o.contains_key("value")
        && (o.contains_key("currency") || o.contains_key("@type"))
        && let Some(s) = f::salary(v)
    {
        return Some(s);
    }
    let amount = |keys: &[&str]| {
        get(o, keys).and_then(|(_, x)| match x {
            Value::Number(n) => f::decimal(n),
            other => f::price_fields(other).price,
        })
    };
    let min = amount(&["min", "minimum", "minvalue", "from", "low"]);
    let max = amount(&["max", "maximum", "maxvalue", "to", "high"]);
    min.as_ref().or(max.as_ref())?;
    let c = get(o, &CURRENCY)
        .map(|(_, c)| f::currency(c))
        .unwrap_or_default();
    let period = get(
        o,
        &["period", "unit", "unittext", "interval", "ratetype", "per"],
    )
    .and_then(|(_, p)| p.as_str())
    .and_then(rate);
    Some(json!({
        "raw": null,
        "value_min": min.clone().or(max.clone()),
        "value_max": max.or(min),
        "rate_type": period,
        "currency": c.currency,
        "currency_raw": c.currency_raw,
    }))
}

fn rate(p: &str) -> Option<&'static str> {
    let p = p.to_ascii_lowercase();
    let has = |w: &[&str]| w.iter().any(|x| p.contains(x));
    if has(&["hour"]) {
        Some("hourly")
    } else if has(&["day", "daily"]) {
        Some("daily")
    } else if has(&["week"]) {
        Some("weekly")
    } else if has(&["month"]) {
        Some("monthly")
    } else if has(&["year", "annum", "annual"]) {
        Some("annual")
    } else {
        None
    }
}
