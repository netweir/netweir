//! Articles in the common schema's shape: the schema.org Article (or
//! NewsArticle, BlogPosting...) first, then what `content::article` reads
//! from the page itself.

use serde_json::{Map, Value, json};

use super::fields as f;
use super::schema_org::{Source, Thing, of_type};
use super::{Builder, Page};

pub(crate) const FIELDS: &[&str] = &[
    "url",
    "canonical_url",
    "headline",
    "date_published",
    "date_published_raw",
    "date_modified",
    "date_modified_raw",
    "authors",
    "breadcrumbs",
    "in_language",
    "main_image",
    "images",
    "description",
    "article_body",
    "article_body_html",
    "videos",
    "audios",
];

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    let articles = of_type(&page.things, "Article");
    let mut chosen: Vec<&Thing> = Vec::new();
    for source in [Source::JsonLd, Source::Microdata] {
        if let Some(first) = articles.iter().find(|t| t.source == source) {
            chosen.push(first);
        }
    }
    for a in &chosen {
        from_thing(page, a, item);
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
    from_meta(page, item);
    let found = from_content(page, item);
    item.probability = if !chosen.is_empty() {
        0.9
    } else if found {
        0.6
    } else {
        0.05
    };
}

fn from_thing(page: &Page<'_, '_>, a: &Thing, item: &mut Builder) {
    let p = &a.props;
    let get = |k: &str| p.get(k).unwrap_or(&Value::Null);
    let src = |k: &str| page.source(a, k);
    let headline = f::text(get("headline")).or_else(|| f::text(get("name")));
    item.offer("headline", headline.map(Value::String), src("headline"));
    for (field, raw_field, prop) in [
        ("date_published", "date_published_raw", "datePublished"),
        ("date_modified", "date_modified_raw", "dateModified"),
    ] {
        item.offer(
            field,
            f::iso_date(get(prop), page.now).map(Value::String),
            src(prop),
        );
        item.offer(raw_field, f::text(get(prop)).map(Value::String), src(prop));
    }
    item.offer("authors", authors(get("author"), page), src("author"));
    item.offer(
        "in_language",
        language(get("inLanguage")).map(Value::String),
        src("inLanguage"),
    );
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
    item.offer("videos", media(get("video"), page), src("video"));
    item.offer("audios", media(get("audio"), page), src("audio"));
}

fn from_meta(page: &Page<'_, '_>, item: &mut Builder) {
    let m = |k: &str| page.meta(k).cloned();
    if let Some((key, c)) = m("canonical")
        .map(|c| ("canonical", c))
        .or_else(|| m("og:url").map(|c| ("og:url", c)))
    {
        item.offer(
            "canonical_url",
            c.as_str()
                .and_then(|u| f::absolute(u, page.base))
                .map(Value::String),
            format!("meta:{key}"),
        );
    }
    let description = m("og:description")
        .as_ref()
        .and_then(f::text)
        .map(Value::String);
    item.offer("description", description, "meta:og:description".into());
}

/// What `content::article` reads from the page. Says whether the page
/// has main content.
fn from_content(page: &Page<'_, '_>, item: &mut Builder) -> bool {
    let a = crate::content::article(page.root, page.base);
    let main = crate::content::main_content(page.root)
        .and_then(|m| crate::css_path::css_path(m.node))
        .map_or_else(|| "dom".to_string(), |p| format!("dom:{p}"));
    if let Some(title) = &a.title {
        // Credit the heading that says it, when one does.
        let heading = page
            .root
            .descendants()
            .filter(|n| n.tag() == Some("h1"))
            .find(|h| f::text(&Value::String(h.text())).as_deref() == Some(title.as_str()))
            .and_then(crate::css_path::css_path)
            .map_or_else(|| "dom".to_string(), |p| format!("dom:{p}"));
        item.offer("headline", Some(Value::String(title.clone())), heading);
    }
    if let Some(byline) = &a.byline {
        item.offer(
            "authors",
            Some(json!([{"name": byline, "name_raw": byline}])),
            "dom".into(),
        );
    }
    if let Some(published) = &a.published {
        let raw = Value::String(published.clone());
        item.offer(
            "date_published",
            f::iso_date(&raw, page.now).map(Value::String),
            "dom".into(),
        );
        item.offer("date_published_raw", Some(raw), "dom".into());
    }
    if let Some(image) = &a.image {
        let url = Value::String(image.clone());
        item.offer("main_image", f::image(&url, page.base), "dom".into());
        item.offer("images", f::images(&url, page.base), "dom".into());
    }
    item.offer(
        "in_language",
        a.language.clone().map(Value::String),
        "dom:html".into(),
    );
    item.offer(
        "article_body",
        a.text.clone().map(Value::String),
        main.clone(),
    );
    item.offer("article_body_html", a.html.clone().map(Value::String), main);
    a.text.is_some()
}

/// `[{name, name_raw, url}]` from Person or Organization things, names,
/// or a list of either.
pub(super) fn authors(v: &Value, page: &Page<'_, '_>) -> Option<Value> {
    let list: Vec<Value> = each(v)
        .filter_map(|a| {
            let mut out = Map::new();
            let name = match a {
                Value::Object(o) => {
                    if let Some(u) = o
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(|u| f::absolute(u, page.base))
                    {
                        out.insert("url".into(), Value::String(u));
                    }
                    o.get("name").and_then(f::text)
                }
                other => f::text(other),
            }?;
            out.insert("name".into(), Value::String(name.clone()));
            out.insert("name_raw".into(), Value::String(name));
            Some(Value::Object(out))
        })
        .collect();
    (!list.is_empty()).then_some(Value::Array(list))
}

/// A language tag: `en-GB`, or a Language thing's name.
pub(super) fn language(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => o
            .get("alternateName")
            .or_else(|| o.get("name"))
            .and_then(f::text),
        other => f::text(other),
    }
}

/// `[{url}]` for VideoObjects or AudioObjects (their file, else their
/// embed), or URLs.
fn media(v: &Value, page: &Page<'_, '_>) -> Option<Value> {
    let list: Vec<Value> = each(v)
        .filter_map(|m| {
            let url = match m {
                Value::Object(o) => ["contentUrl", "embedUrl", "url"]
                    .iter()
                    .find_map(|k| o.get(*k).and_then(Value::as_str))?,
                Value::String(s) => s.as_str(),
                _ => return None,
            };
            f::absolute(url, page.base).map(|u| json!({ "url": u }))
        })
        .collect();
    (!list.is_empty()).then_some(Value::Array(list))
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
