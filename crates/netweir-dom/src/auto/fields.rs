//! Field values in the common item schema's shapes, from what schema.org
//! data or page text holds: prices as decimal strings, ISO 4217 codes,
//! ISO 8601 dates, and small objects for brands, ratings, breadcrumbs,
//! images and salaries. A value that can't be read is None, never a guess.

use serde_json::{Map, Value, json};
use url::Url;

use crate::Node;
use crate::date::Civil;

/// A price and its currency, as the item fields hold them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriceFields {
    /// Decimal digits, as written: `"1299.00"`, never a float.
    pub price: Option<String>,
    /// ISO 4217, only when the page says which currency.
    pub currency: Option<String>,
    /// What the page wrote: `$`, `€`, `USD`.
    pub currency_raw: Option<String>,
}

/// A price from a number (`1299`, written as it is) or text
/// (`"1.299,00 €"`, read by [`crate::price::parse_price`]).
pub fn price_fields(v: &Value) -> PriceFields {
    match v {
        Value::Number(n) => PriceFields {
            price: Some(n.to_string()),
            ..PriceFields::default()
        },
        Value::String(s) => match crate::price::parse_price(s) {
            Some(p) => PriceFields {
                price: Some(p.amount),
                currency: p
                    .currency
                    .as_deref()
                    .and_then(crate::price::currency_code)
                    .map(str::to_string),
                currency_raw: p.currency,
            },
            None => PriceFields::default(),
        },
        _ => PriceFields::default(),
    }
}

/// A currency as written (`"EUR"`, `"£"`): its ISO code when it names
/// only one, and the text itself.
pub fn currency(v: &Value) -> PriceFields {
    let Some(raw) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
        return PriceFields::default();
    };
    PriceFields {
        price: None,
        currency: crate::price::currency_code(raw).map(str::to_string),
        currency_raw: Some(raw.to_string()),
    }
}

/// `InStock` or `OutOfStock`, from a schema.org availability (any of its
/// URL forms) or from English text that says so.
pub fn availability(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    let word = s.rsplit('/').next().unwrap_or(s).to_ascii_lowercase();
    const IN: [&str; 7] = [
        "instock",
        "limitedavailability",
        "preorder",
        "presale",
        "backorder",
        "onlineonly",
        "instoreonly",
    ];
    const OUT: [&str; 3] = ["outofstock", "soldout", "discontinued"];
    let found = if IN.contains(&word.as_str()) {
        "InStock"
    } else if OUT.contains(&word.as_str()) {
        "OutOfStock"
    } else {
        let lower = s.to_lowercase();
        if ["out of stock", "sold out", "unavailable", "not available"]
            .iter()
            .any(|p| lower.contains(p))
        {
            "OutOfStock"
        } else if lower.contains("in stock") {
            "InStock"
        } else {
            return None;
        }
    };
    Some(found.to_string())
}

/// Every valid product code in a Product's code properties, once each.
pub fn gtins(props: &Map<String, Value>) -> Option<Value> {
    const KEYS: [&str; 8] = [
        "gtin13", "gtin12", "gtin14", "gtin8", "gtin", "isbn", "issn", "upc",
    ];
    let mut out: Vec<Value> = Vec::new();
    let mut seen = Vec::new();
    for key in KEYS {
        for value in each(props.get(key)) {
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => continue,
            };
            let Some(mut g) = crate::gtin::parse(&text) else {
                continue;
            };
            // The schema's codes are digits, and an ISBN-10 can end in X.
            if g.value.ends_with('X') {
                let Some(isbn13) = crate::gtin::isbn13_of(&g.value) else {
                    continue;
                };
                g = crate::gtin::Gtin {
                    kind: "isbn13",
                    value: isbn13,
                };
            }
            if !seen.contains(&g.value) {
                seen.push(g.value.clone());
                out.push(json!({"type": g.kind, "value": g.value}));
            }
        }
    }
    (!out.is_empty()).then_some(Value::Array(out))
}

/// `{name}` from a name, or a Brand or Organization.
pub fn brand(v: &Value) -> Option<Value> {
    each(Some(v))
        .find_map(|b| match b {
            Value::Object(o) => o.get("name").and_then(text),
            other => text(other),
        })
        .map(|name| json!({"name": name}))
}

/// `{rating_value, best_rating, review_count}` from an AggregateRating
/// or text such as `3.8 (7 reviews)` or `4.5 out of 5`.
pub fn rating(v: &Value) -> Option<Value> {
    let (value, best, count) = match v {
        Value::Object(o) => (
            o.get("ratingValue").and_then(number)?,
            o.get("bestRating").and_then(number),
            o.get("reviewCount")
                .or_else(|| o.get("ratingCount"))
                .and_then(count),
        ),
        Value::String(s) => rating_text(s)?,
        _ => return None,
    };
    Some(json!({"rating_value": value, "best_rating": best, "review_count": count}))
}

fn rating_text(s: &str) -> Option<(f64, Option<f64>, Option<i64>)> {
    static VALUE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(\d+(?:[.,]\d+)?)(?:\s*(?:/|out of|of)\s*(\d+(?:[.,]\d+)?))?").unwrap()
    });
    static COUNT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(\d[\d,.]*)\s*(?:reviews?|ratings?|votes?)").unwrap()
    });
    let caps = VALUE.captures(s)?;
    let float = |m: regex::Match<'_>| m.as_str().replace(',', ".").parse::<f64>().ok();
    let value = float(caps.get(1)?)?;
    // "7 reviews" alone is a count, not a rating.
    if COUNT
        .captures(s)
        .is_some_and(|c| c.get(1).map(|m| m.start()) == caps.get(1).map(|m| m.start()))
    {
        return None;
    }
    let best = caps.get(2).and_then(float);
    let count = COUNT
        .captures(s)
        .and_then(|c| count(&Value::String(c[1].to_string())));
    Some((value, best, count))
}

/// `[{name, url}]` from a BreadcrumbList (or its list of ListItems), in
/// position order, URLs absolute.
pub fn breadcrumbs(v: &Value, base: Option<&Url>) -> Option<Value> {
    let list = match v {
        Value::Object(o) => o.get("itemListElement")?,
        other => other,
    };
    let mut items: Vec<(f64, usize, Value)> = Vec::new();
    for (i, item) in each(Some(list)).enumerate() {
        let Value::Object(o) = item else { continue };
        let inner = o.get("item");
        let name = o
            .get("name")
            .and_then(text)
            .or_else(|| inner.and_then(|i| i.get("name")).and_then(text));
        let Some(name) = name else { continue };
        let url = match inner {
            Some(Value::String(u)) => Some(u.as_str()),
            Some(Value::Object(i)) => i
                .get("@id")
                .or_else(|| i.get("url"))
                .and_then(Value::as_str),
            _ => o.get("url").and_then(Value::as_str),
        }
        .and_then(|u| absolute(u, base));
        let position = o.get("position").and_then(number).unwrap_or(f64::MAX);
        items.push((position, i, json!({"name": name, "url": url})));
    }
    items.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    (!items.is_empty()).then(|| Value::Array(items.into_iter().map(|(_, _, v)| v).collect()))
}

/// An ISO 8601 date (`2026-10-01`), or date and time with the zone the
/// text gave (`2026-10-01T09:30:00+02:00`), from any date
/// [`crate::date::parse_date`] reads. Relative dates count from `now`.
pub fn iso_date(v: &Value, now: Civil) -> Option<String> {
    let s = v.as_str()?;
    let parsed = crate::date::parse_date(s, None, now)?;
    let c = parsed.naive()?;
    let timed = s.contains(':') || (c.hour, c.minute, c.second, c.micro) != (0, 0, 0, 0);
    let mut out = format!("{:04}-{:02}-{:02}", c.year, c.month, c.day);
    if timed {
        out.push_str(&format!("T{:02}:{:02}:{:02}", c.hour, c.minute, c.second));
        if c.micro != 0 {
            out.push_str(&format!(".{:06}", c.micro));
        }
        if let crate::date::Zone::Offset(o) = parsed.zone {
            if o == 0 {
                out.push('Z');
            } else {
                let sign = if o < 0 { '-' } else { '+' };
                let o = o.unsigned_abs();
                out.push_str(&format!("{sign}{:02}:{:02}", o / 3600, o % 3600 / 60));
            }
        }
    }
    Some(out)
}

/// `{raw, value_min, value_max, rate_type, currency, currency_raw}` from
/// a MonetaryAmount or text such as `£30,000 - £40,000 a year`.
pub fn salary(v: &Value) -> Option<Value> {
    match v {
        Value::Object(o) => {
            let money = o.get("currency").map(currency).unwrap_or_default();
            let value = o.get("value")?;
            let (min, max, unit) = match value {
                Value::Object(q) => {
                    let one = q.get("value").and_then(amount);
                    (
                        q.get("minValue").and_then(amount).or_else(|| one.clone()),
                        q.get("maxValue").and_then(amount).or(one),
                        q.get("unitText").and_then(Value::as_str),
                    )
                }
                other => (
                    amount(other),
                    amount(other),
                    o.get("unitText").and_then(Value::as_str),
                ),
            };
            min.as_ref().or(max.as_ref())?;
            Some(json!({
                "raw": null, "value_min": min, "value_max": max,
                "rate_type": unit.and_then(rate_type),
                "currency": money.currency, "currency_raw": money.currency_raw,
            }))
        }
        Value::String(s) => salary_text(s),
        _ => None,
    }
}

fn salary_text(s: &str) -> Option<Value> {
    static RANGE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s*(?:-|–|—|\bto\b)\s*").unwrap());
    let mut parts = RANGE
        .split(s)
        .filter(|p| p.chars().any(|c| c.is_ascii_digit()));
    let first = parts.next()?;
    let low = thousands(first)?;
    let high = parts
        .next()
        .and_then(thousands)
        .unwrap_or_else(|| low.clone());
    let raw = low.currency.clone().or(high.currency.clone());
    Some(json!({
        "raw": s, "value_min": low.amount, "value_max": high.amount,
        "rate_type": rate_type(s),
        "currency": raw.as_deref().and_then(crate::price::currency_code),
        "currency_raw": raw,
    }))
}

/// A price where `30k` means 30000.
fn thousands(part: &str) -> Option<crate::price::Price> {
    let mut p = crate::price::parse_price(part)?;
    let after_digits = part
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ',');
    if after_digits.starts_with(['k', 'K']) {
        let n: f64 = p.amount.parse().ok()?;
        p.amount = format!("{}", (n * 1000.0).round() as i64);
    }
    Some(p)
}

/// `hourly`, `daily`, `weekly`, `monthly` or `annual`, from schema.org's
/// unitText (`HOUR`) or the way a salary is written (`a year`, `/hr`).
fn rate_type(s: &str) -> Option<&'static str> {
    let lower = s.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if has(&["hour", "/hr", "p/h", "per hr"]) {
        Some("hourly")
    } else if has(&["day", "daily", "/d"]) && !has(&["days ago"]) {
        Some("daily")
    } else if has(&["week", "/wk"]) {
        Some("weekly")
    } else if has(&["month", "/mo", "pcm"]) {
        Some("monthly")
    } else if has(&["year", "annum", "annual", "/yr", " pa", "p.a."]) {
        Some("annual")
    } else {
        None
    }
}

/// `full-time`, `part-time` and so on from schema.org's codes, as the
/// common item schema writes them; other words as written. Several are
/// joined with commas.
pub fn employment_type(v: &Value) -> Option<String> {
    let words: Vec<String> = each(Some(v))
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(|w| {
            match w.to_ascii_uppercase().replace(['-', ' '], "_").as_str() {
                "FULL_TIME" => "full-time",
                "PART_TIME" => "part-time",
                "CONTRACTOR" | "CONTRACT" => "contract",
                "TEMPORARY" => "temporary",
                "INTERN" | "INTERNSHIP" => "internship",
                "VOLUNTEER" => "volunteer",
                "PER_DIEM" => "per diem",
                _ => w,
            }
            .to_string()
        })
        .collect();
    (!words.is_empty()).then(|| words.join(", "))
}

/// `{url}` for the first image in a URL, an ImageObject or a list.
pub fn image(v: &Value, base: Option<&Url>) -> Option<Value> {
    each(Some(v)).find_map(|i| image_url(i, base).map(|url| json!({"url": url})))
}

/// `[{url}]` for every image, once each.
pub fn images(v: &Value, base: Option<&Url>) -> Option<Value> {
    let mut urls: Vec<String> = Vec::new();
    for url in each(Some(v)).filter_map(|i| image_url(i, base)) {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    (!urls.is_empty())
        .then(|| Value::Array(urls.into_iter().map(|url| json!({"url": url})).collect()))
}

fn image_url(v: &Value, base: Option<&Url>) -> Option<String> {
    let url = match v {
        Value::String(s) => s.as_str(),
        Value::Object(o) => ["url", "contentUrl", "@id"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str))?,
        _ => return None,
    };
    absolute(url, base)
}

/// Text as a reader would see it: tags dropped, entities decoded,
/// whitespace collapsed. A number is its digits; a list, its first item.
pub fn text(v: &Value) -> Option<String> {
    let s = match v {
        // Blocks kept apart, so "<p>a</p><p>b</p>" is "a b", not "ab".
        Value::String(s) if s.contains(['<', '&']) => {
            crate::markdown::clean_text(crate::Document::parse(s).root())
        }
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Array(list) => return list.iter().find_map(text),
        _ => return None,
    };
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

/// What kind of value a field holds, which decides how an element a
/// person picked for it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Clean text.
    Text,
    /// The element's HTML.
    Html,
    /// A link's `href` (or an image's source), absolute.
    Url,
    Price,
    Currency,
    CurrencyRaw,
    Availability,
    Gtin,
    Brand,
    Rating,
    Breadcrumbs,
    Date,
    Salary,
    EmploymentType,
    Image,
    Images,
    /// The text of each item (`li`, or each child) as a list.
    Texts,
    /// `{name, name_raw}`, from the organisation's name.
    Organization,
    /// `{raw}`, the place as written.
    Location,
    /// `[{name, name_raw, url}]`: each link's text and page, or the text.
    Authors,
    /// `{url, text}` for a link.
    Link,
    /// `{url, name}` for a page to request.
    Request,
    /// The first whole number in the text.
    Int,
    /// `[{url}]` for videos or audio: their sources, or links.
    Media,
}

/// A field's value from an element a person chose for it.
pub fn from_element(
    kind: FieldKind,
    node: Node<'_>,
    base: Option<&Url>,
    now: Civil,
) -> Option<Value> {
    let words = || {
        let t = crate::markdown::clean_text(node);
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        (!t.is_empty()).then_some(t)
    };
    let string = |t: Option<String>| t.map(Value::String);
    match kind {
        FieldKind::Text => string(words()),
        FieldKind::Html => Some(Value::String(node.html())),
        FieldKind::Url => {
            let link = node
                .attr("href")
                .or_else(|| crate::markdown::image_source(node));
            string(link.and_then(|u| absolute(u, base)))
        }
        FieldKind::Price => string(price_fields(&Value::String(words()?)).price),
        FieldKind::Currency => {
            let t = Value::String(words()?);
            string(price_fields(&t).currency.or_else(|| currency(&t).currency))
        }
        FieldKind::CurrencyRaw => {
            let t = Value::String(words()?);
            string(
                price_fields(&t)
                    .currency_raw
                    .or_else(|| currency(&t).currency_raw),
            )
        }
        FieldKind::Availability => {
            let link = node.attr("href").map(|h| Value::String(h.to_string()));
            link.and_then(|l| availability(&l))
                .or_else(|| availability(&Value::String(words()?)))
                .map(Value::String)
        }
        FieldKind::Gtin => {
            let mut one = Map::new();
            one.insert("gtin".into(), Value::String(words()?));
            gtins(&one)
        }
        FieldKind::Brand => {
            let alt = || {
                std::iter::once(node)
                    .chain(node.descendants())
                    .find(|n| n.tag() == Some("img"))
                    .and_then(|img| img.attr("alt"))
                    .map(str::to_string)
            };
            brand(&Value::String(words().or_else(alt)?))
        }
        FieldKind::Rating => {
            let label = ["aria-label", "title", "data-rating", "content"]
                .iter()
                .find_map(|a| node.attr(a));
            label
                .and_then(|l| rating(&Value::String(l.to_string())))
                .or_else(|| rating(&Value::String(words()?)))
        }
        FieldKind::Breadcrumbs => {
            let items: Vec<Node<'_>> = node
                .descendants()
                .filter(|n| n.tag() == Some("li"))
                .collect();
            let items = if items.is_empty() {
                node.descendants()
                    .filter(|n| n.tag() == Some("a"))
                    .collect()
            } else {
                items
            };
            let list: Vec<Value> = items
                .into_iter()
                .filter_map(|item| {
                    let name = crate::markdown::clean_text(item);
                    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
                    if name.is_empty() {
                        return None;
                    }
                    let link = std::iter::once(item)
                        .chain(item.descendants())
                        .find_map(|n| n.attr("href"))
                        .and_then(|u| absolute(u, base));
                    Some(json!({"name": name, "url": link}))
                })
                .collect();
            (!list.is_empty()).then_some(Value::Array(list))
        }
        FieldKind::Date => {
            let attr = node.attr("datetime").or_else(|| node.attr("content"));
            attr.and_then(|a| iso_date(&Value::String(a.to_string()), now))
                .or_else(|| iso_date(&Value::String(words()?), now))
                .map(Value::String)
        }
        FieldKind::Salary => salary(&Value::String(words()?)),
        FieldKind::EmploymentType => employment_type(&Value::String(words()?)).map(Value::String),
        FieldKind::Image | FieldKind::Images => {
            let srcs: Vec<Value> = std::iter::once(node)
                .chain(node.descendants())
                .filter(|n| matches!(n.tag(), Some("img" | "source")))
                .filter_map(crate::markdown::image_source)
                .map(|s| Value::String(s.to_string()))
                .collect();
            match kind {
                FieldKind::Image => image(&Value::Array(srcs), base),
                _ => images(&Value::Array(srcs), base),
            }
        }
        FieldKind::Organization => {
            let name = words()?;
            Some(json!({"name": name, "name_raw": name}))
        }
        FieldKind::Location => Some(json!({"raw": words()?})),
        FieldKind::Authors => {
            let links: Vec<Value> = std::iter::once(node)
                .chain(node.descendants())
                .filter(|n| n.tag() == Some("a"))
                .filter_map(|a| {
                    let name = self::text(&Value::String(a.text()))?;
                    let mut one = json!({"name": name, "name_raw": name});
                    if let Some(u) = a.attr("href").and_then(|h| absolute(h, base)) {
                        one["url"] = Value::String(u);
                    }
                    Some(one)
                })
                .collect();
            if links.is_empty() {
                from_text(kind, &words()?, base, now)
            } else {
                Some(Value::Array(links))
            }
        }
        FieldKind::Link | FieldKind::Request => {
            let link = std::iter::once(node)
                .chain(node.descendants())
                .find_map(|n| n.attr("href"))
                .and_then(|h| absolute(h, base))?;
            let label = if kind == FieldKind::Link {
                "text"
            } else {
                "name"
            };
            let mut out = Map::new();
            out.insert("url".into(), Value::String(link));
            if let Some(t) = words() {
                out.insert(label.into(), Value::String(t));
            }
            Some(Value::Object(out))
        }
        FieldKind::Int => from_text(kind, &words()?, base, now),
        FieldKind::Media => {
            let mut urls: Vec<Value> = Vec::new();
            for n in std::iter::once(node).chain(node.descendants()) {
                let src = match n.tag() {
                    Some("video" | "audio" | "source" | "track") => n.attr("src"),
                    Some("a") => n.attr("href"),
                    _ => None,
                };
                if let Some(u) = src.and_then(|s| absolute(s, base)) {
                    let one = json!({"url": u});
                    if !urls.contains(&one) {
                        urls.push(one);
                    }
                }
            }
            (!urls.is_empty()).then_some(Value::Array(urls))
        }
        FieldKind::Texts => {
            let lis: Vec<Node<'_>> = node
                .descendants()
                .filter(|n| n.tag() == Some("li"))
                .collect();
            let parts: Vec<Node<'_>> = if lis.is_empty() {
                node.children()
                    .filter(|c| c.kind() == crate::NodeKind::Element)
                    .collect()
            } else {
                lis
            };
            let list: Vec<Value> = parts
                .into_iter()
                .filter_map(|p| {
                    let t = crate::markdown::clean_text(p);
                    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
                    (!t.is_empty()).then_some(Value::String(t))
                })
                .collect();
            (!list.is_empty()).then_some(Value::Array(list))
        }
    }
}

/// A field's value from a string a query found (`::text`, `::attr()`).
pub fn from_text(kind: FieldKind, text: &str, base: Option<&Url>, now: Civil) -> Option<Value> {
    let v = Value::String(text.to_string());
    let string = |t: Option<String>| t.map(Value::String);
    match kind {
        FieldKind::Text | FieldKind::Html => string(self::text(&v)),
        FieldKind::Url => string(absolute(text, base)),
        FieldKind::Price => string(price_fields(&v).price),
        FieldKind::Currency => string(price_fields(&v).currency.or_else(|| currency(&v).currency)),
        FieldKind::CurrencyRaw => string(
            price_fields(&v)
                .currency_raw
                .or_else(|| currency(&v).currency_raw),
        ),
        FieldKind::Availability => string(availability(&v)),
        FieldKind::Gtin => {
            let mut one = Map::new();
            one.insert("gtin".into(), v);
            gtins(&one)
        }
        FieldKind::Brand => brand(&v),
        FieldKind::Rating => rating(&v),
        FieldKind::Breadcrumbs => None,
        FieldKind::Date => string(iso_date(&v, now)),
        FieldKind::Salary => salary(&v),
        FieldKind::EmploymentType => string(employment_type(&v)),
        FieldKind::Image => image(&v, base),
        FieldKind::Images => images(&v, base),
        FieldKind::Organization => {
            let name = self::text(&v)?;
            Some(json!({"name": name, "name_raw": name}))
        }
        FieldKind::Location => Some(json!({"raw": self::text(&v)?})),
        FieldKind::Authors => {
            let name = self::text(&v)?;
            Some(json!([{"name": name, "name_raw": name}]))
        }
        FieldKind::Link => Some(json!({"url": absolute(text, base)?})),
        FieldKind::Request => Some(json!({"url": absolute(text, base)?})),
        FieldKind::Int => {
            static INT: std::sync::LazyLock<regex::Regex> =
                std::sync::LazyLock::new(|| regex::Regex::new(r"\d+").unwrap());
            INT.find(text)?
                .as_str()
                .parse::<i64>()
                .ok()
                .map(Value::from)
        }
        FieldKind::Media => Some(json!([{"url": absolute(text, base)?}])),
        FieldKind::Texts => self::text(&v).map(|t| Value::Array(vec![Value::String(t)])),
    }
}

/// A value, or each member of a list.
fn each(v: Option<&Value>) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Some(Value::Array(list)) => Box::new(list.iter()),
        Some(Value::Null) | None => Box::new(std::iter::empty()),
        Some(one) => Box::new(std::iter::once(one)),
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', ".").parse().ok(),
        _ => None,
    }
}

fn count(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().replace([',', '.', ' '], "").parse().ok(),
        _ => None,
    }
}

/// A decimal string from a number or numeric text.
fn amount(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(_) => price_fields(v).price,
        _ => None,
    }
}

pub(crate) fn absolute(url: &str, base: Option<&Url>) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    match base {
        Some(b) => b.join(url).ok().map(String::from),
        None => Url::parse(url)
            .ok()
            .map(String::from)
            .or(Some(url.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Civil {
        Civil::date(2026, 10, 25)
    }

    fn base() -> Url {
        Url::parse("https://shop.example/c/lamps/").unwrap()
    }

    fn price(v: Value) -> (Option<String>, Option<String>, Option<String>) {
        let p = price_fields(&v);
        (p.price, p.currency, p.currency_raw)
    }

    fn s(x: &str) -> Option<String> {
        Some(x.to_string())
    }

    #[test]
    fn prices_keep_their_digits() {
        assert_eq!(price(json!("1.299,00 €")), (s("1299.00"), s("EUR"), s("€")));
        assert_eq!(price(json!(1299)), (s("1299"), None, None));
        assert_eq!(price(json!(12.5)), (s("12.5"), None, None));
        assert_eq!(price(json!("19.99")), (s("19.99"), None, None));
        assert_eq!(price(json!("$12")), (s("12"), None, s("$")));
        assert_eq!(price(json!("USD 12")), (s("12"), s("USD"), s("USD")));
        assert_eq!(price(json!(0)), (s("0"), None, None));
        assert_eq!(price(json!("Call for price")), (None, None, None));
        assert_eq!(price(json!(null)), (None, None, None));
    }

    #[test]
    fn currency_codes_and_symbols() {
        let c = |v| {
            let p = currency(&v);
            (p.currency, p.currency_raw)
        };
        assert_eq!(c(json!("EUR")), (s("EUR"), s("EUR")));
        assert_eq!(c(json!("eur")), (s("EUR"), s("eur")));
        assert_eq!(c(json!("£")), (s("GBP"), s("£")));
        assert_eq!(c(json!("$")), (None, s("$")));
        assert_eq!(c(json!("dollars")), (None, s("dollars")));
    }

    #[test]
    fn availability_is_in_or_out_of_stock() {
        for v in [
            "https://schema.org/InStock",
            "http://schema.org/InStock",
            "InStock",
            "LimitedAvailability",
            "PreOrder",
            "BackOrder",
            "In stock",
            "in stock (7 left)",
            "OnlineOnly",
            "InStoreOnly",
        ] {
            assert_eq!(availability(&json!(v)), s("InStock"), "{v}");
        }
        for v in [
            "https://schema.org/OutOfStock",
            "SoldOut",
            "Discontinued",
            "Out of stock",
            "Sold out",
            "Currently unavailable",
        ] {
            assert_eq!(availability(&json!(v)), s("OutOfStock"), "{v}");
        }
        assert_eq!(availability(&json!("ships in 3 days")), None);
        assert_eq!(availability(&json!(3)), None);
    }

    #[test]
    fn gtins_from_any_property_checked() {
        let props = json!({"gtin13": "4006381333931", "gtin": "0-306-40615-2",
                           "gtin12": "000000000000x", "isbn": "978-0-306-40615-7",
                           "issn": "080442957X"});
        assert_eq!(
            gtins(props.as_object().unwrap()),
            Some(json!([
                {"type": "gtin13", "value": "4006381333931"},
                {"type": "isbn10", "value": "0306406152"},
                {"type": "isbn13", "value": "9780306406157"},
                // An ISBN-10 ending in X, as its ISBN-13: the schema's codes are digits.
                {"type": "isbn13", "value": "9780804429573"}
            ]))
        );
        assert_eq!(gtins(json!({"gtin13": "123"}).as_object().unwrap()), None);
    }

    #[test]
    fn brands_from_names_and_things() {
        assert_eq!(brand(&json!("Lumo")), Some(json!({"name": "Lumo"})));
        assert_eq!(
            brand(&json!({"@type": "Brand", "name": " Lumo "})),
            Some(json!({"name": "Lumo"}))
        );
        assert_eq!(
            brand(&json!([{"name": "Lumo"}, "Other"])),
            Some(json!({"name": "Lumo"}))
        );
        assert_eq!(brand(&json!({"@type": "Brand"})), None);
        assert_eq!(brand(&json!("")), None);
    }

    #[test]
    fn ratings_from_things_and_text() {
        assert_eq!(
            rating(
                &json!({"@type": "AggregateRating", "ratingValue": "4.6", "bestRating": 5,
                           "reviewCount": "1,204"})
            ),
            Some(json!({"rating_value": 4.6, "best_rating": 5.0, "review_count": 1204}))
        );
        assert_eq!(
            rating(&json!({"ratingValue": 4, "ratingCount": 12})),
            Some(json!({"rating_value": 4.0, "best_rating": null, "review_count": 12}))
        );
        assert_eq!(
            rating(&json!("3.8 (7 reviews)")),
            Some(json!({"rating_value": 3.8, "best_rating": null, "review_count": 7}))
        );
        assert_eq!(
            rating(&json!("4.5 out of 5 stars")),
            Some(json!({"rating_value": 4.5, "best_rating": 5.0, "review_count": null}))
        );
        assert_eq!(
            rating(&json!("Rated 4,5/5")),
            Some(json!({"rating_value": 4.5, "best_rating": 5.0, "review_count": null}))
        );
        assert_eq!(rating(&json!("no reviews yet")), None);
    }

    #[test]
    fn breadcrumbs_in_position_order_with_absolute_urls() {
        let list = json!({"@type": "BreadcrumbList", "itemListElement": [
            {"@type": "ListItem", "position": 2, "name": "Lamps", "item": "/c/lamps/"},
            {"@type": "ListItem", "position": "1", "item": {"@id": "https://shop.example/", "name": "Home"}},
            {"@type": "ListItem", "position": 3, "name": "Desk lamp"}
        ]});
        assert_eq!(
            breadcrumbs(&list, Some(&base())),
            Some(json!([
                {"name": "Home", "url": "https://shop.example/"},
                {"name": "Lamps", "url": "https://shop.example/c/lamps/"},
                {"name": "Desk lamp", "url": null}
            ]))
        );
        assert_eq!(breadcrumbs(&json!({"itemListElement": []}), None), None);
    }

    #[test]
    fn dates_become_iso_8601() {
        assert_eq!(iso_date(&json!("2026-10-01"), now()), s("2026-10-01"));
        assert_eq!(
            iso_date(&json!("2026-10-01T09:30:00+02:00"), now()),
            s("2026-10-01T09:30:00+02:00")
        );
        assert_eq!(
            iso_date(&json!("2026-10-01T09:30:00Z"), now()),
            s("2026-10-01T09:30:00Z")
        );
        assert_eq!(iso_date(&json!("1 October 2026"), now()), s("2026-10-01"));
        assert_eq!(iso_date(&json!("3 days ago"), now()), s("2026-10-22"));
        assert_eq!(iso_date(&json!("soon"), now()), None);
    }

    #[test]
    fn salaries_from_things_and_text() {
        assert_eq!(
            salary(&json!({"@type": "MonetaryAmount", "currency": "USD",
                           "value": {"@type": "QuantitativeValue", "minValue": 50000,
                                     "maxValue": 70000, "unitText": "YEAR"}})),
            Some(
                json!({"raw": null, "value_min": "50000", "value_max": "70000",
                        "rate_type": "annual", "currency": "USD", "currency_raw": "USD"})
            )
        );
        assert_eq!(
            salary(&json!({"currency": "EUR", "value": {"value": 25, "unitText": "HOUR"}})),
            Some(json!({"raw": null, "value_min": "25", "value_max": "25",
                        "rate_type": "hourly", "currency": "EUR", "currency_raw": "EUR"}))
        );
        assert_eq!(
            salary(&json!("£30,000 - £40,000 a year")),
            Some(
                json!({"raw": "£30,000 - £40,000 a year", "value_min": "30000",
                        "value_max": "40000", "rate_type": "annual", "currency": "GBP",
                        "currency_raw": "£"})
            )
        );
        assert_eq!(
            salary(&json!("$25/hr")),
            Some(
                json!({"raw": "$25/hr", "value_min": "25", "value_max": "25",
                        "rate_type": "hourly", "currency": null, "currency_raw": "$"})
            )
        );
        assert_eq!(salary(&json!("competitive")), None);
    }

    #[test]
    fn employment_types_read_as_words() {
        assert_eq!(employment_type(&json!("FULL_TIME")), s("full-time"));
        assert_eq!(
            employment_type(&json!(["PART_TIME", "CONTRACTOR"])),
            s("part-time, contract")
        );
        assert_eq!(employment_type(&json!("Permanent")), s("Permanent"));
        assert_eq!(employment_type(&json!([])), None);
    }

    #[test]
    fn images_from_strings_and_image_objects() {
        assert_eq!(
            image(&json!("/i/1.jpg"), Some(&base())),
            Some(json!({"url": "https://shop.example/i/1.jpg"}))
        );
        assert_eq!(
            image(
                &json!({"@type": "ImageObject", "contentUrl": "https://cdn.example/2.jpg"}),
                None
            ),
            Some(json!({"url": "https://cdn.example/2.jpg"}))
        );
        assert_eq!(
            images(
                &json!(["/a.jpg", {"url": "/b.jpg"}, "/a.jpg"]),
                Some(&base())
            ),
            Some(
                json!([{"url": "https://shop.example/a.jpg"}, {"url": "https://shop.example/b.jpg"}])
            )
        );
        assert_eq!(image(&json!(""), None), None);
    }

    #[test]
    fn text_is_trimmed_and_html_free() {
        assert_eq!(text(&json!("  Desk   lamp\n")), s("Desk lamp"));
        assert_eq!(
            text(&json!("A <b>bold</b> lamp &amp; shade")),
            s("A bold lamp & shade")
        );
        assert_eq!(text(&json!(42)), s("42"));
        assert_eq!(text(&json!(["first", "second"])), s("first"));
        assert_eq!(text(&json!("  ")), None);
        assert_eq!(text(&json!({"name": "x"})), None);
    }

    fn el(html: &str, kind: FieldKind) -> Option<Value> {
        let doc = crate::Document::parse(html);
        let node = crate::Query::new("#x").unwrap().select(doc.root())[0];
        from_element(kind, node, Some(&base()), now())
    }

    #[test]
    fn fields_from_elements_a_person_picked() {
        assert_eq!(
            el(r#"<span id=x>$9.50 inc. VAT</span>"#, FieldKind::Price),
            Some(json!("9.50"))
        );
        assert_eq!(
            el(r#"<span id=x>9,50 €</span>"#, FieldKind::Currency),
            Some(json!("EUR"))
        );
        assert_eq!(
            el(r#"<span id=x>$9.50</span>"#, FieldKind::CurrencyRaw),
            Some(json!("$"))
        );
        assert_eq!(
            el(r#"<h1 id=x> Desk <b>lamp</b> </h1>"#, FieldKind::Text),
            Some(json!("Desk lamp"))
        );
        assert_eq!(
            el(r#"<a id=x href="/p/2">x</a>"#, FieldKind::Url),
            Some(json!("https://shop.example/p/2"))
        );
        assert_eq!(
            el(r#"<p id=x>Sold out</p>"#, FieldKind::Availability),
            Some(json!("OutOfStock"))
        );
        assert_eq!(
            el(
                r#"<link id=x href="https://schema.org/InStock">"#,
                FieldKind::Availability
            ),
            Some(json!("InStock"))
        );
        assert_eq!(
            el(
                r#"<table><tr><td id=x>EAN 4006381333931</td></tr></table>"#,
                FieldKind::Gtin
            ),
            Some(json!([{"type": "gtin13", "value": "4006381333931"}]))
        );
        assert_eq!(
            el(
                r#"<table><tr><td id=x>4006381333931</td></tr></table>"#,
                FieldKind::Gtin
            ),
            Some(json!([{"type": "gtin13", "value": "4006381333931"}]))
        );
        assert_eq!(
            el(r#"<a id=x><img alt="Lumo"></a>"#, FieldKind::Brand),
            Some(json!({"name": "Lumo"}))
        );
        assert_eq!(
            el(
                r#"<div id=x aria-label="Rated 4.5 out of 5"><i></i></div>"#,
                FieldKind::Rating
            ),
            Some(json!({"rating_value": 4.5, "best_rating": 5.0, "review_count": null}))
        );
        assert_eq!(
            el(
                r#"<ol id=x><li><a href="/">Home</a></li><li><a href="/c/">Lamps</a></li><li>Desk lamp</li></ol>"#,
                FieldKind::Breadcrumbs
            ),
            Some(json!([
                {"name": "Home", "url": "https://shop.example/"},
                {"name": "Lamps", "url": "https://shop.example/c/"},
                {"name": "Desk lamp", "url": null}
            ]))
        );
        assert_eq!(
            el(
                r#"<time id=x datetime="2026-10-01">Oct 1</time>"#,
                FieldKind::Date
            ),
            Some(json!("2026-10-01"))
        );
        assert_eq!(
            el(
                r#"<div id=x><img src="/a.jpg"><img data-src="/b.jpg" src="data:,"></div>"#,
                FieldKind::Images
            ),
            Some(
                json!([{"url": "https://shop.example/a.jpg"}, {"url": "https://shop.example/b.jpg"}])
            )
        );
        assert_eq!(
            el(
                r#"<ul id=x><li>Dimmable</li><li> USB-C </li></ul>"#,
                FieldKind::Texts
            ),
            Some(json!(["Dimmable", "USB-C"]))
        );
        assert_eq!(el(r#"<span id=x></span>"#, FieldKind::Text), None);
    }
}
