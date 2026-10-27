//! Social media posts: a single post's page, from its schema.org
//! SocialMediaPosting, or from the Open Graph tags a network's post page
//! carries. A posting with comments is a thread (see `feeds`).

use serde_json::{Map, Value, json};

use super::fields as f;
use super::schema_org::Thing;
use super::{Builder, Page};

pub(crate) const FIELDS: &[&str] = &[
    "url",
    "post_id",
    "text",
    "date_published",
    "hashtags",
    "media_urls",
    "author",
    "reactions",
];

/// Whether a posting has comments, which make it a thread.
pub(crate) fn has_comments(t: &Thing) -> bool {
    match t.props.get("comment") {
        Some(Value::Array(list)) => !list.is_empty(),
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

pub(crate) fn read(page: &Page<'_, '_>, item: &mut Builder) {
    let here = |t: &&Thing| {
        t.props
            .get("url")
            .and_then(Value::as_str)
            .is_none_or(|u| page.is_page_url(u))
    };
    let posting = page
        .things
        .iter()
        .filter(|t| t.types.iter().any(|ty| ty == "SocialMediaPosting"))
        .find(here);
    if let Some(t) = posting {
        if has_comments(t) {
            item.probability = 0.05;
            return;
        }
        from_thing(page, t, item);
        item.probability = if item.has("text") { 0.9 } else { 0.7 };
        return;
    }
    item.probability = if from_meta(page, item) { 0.5 } else { 0.05 };
}

fn from_thing(page: &Page<'_, '_>, t: &Thing, item: &mut Builder) {
    let p = &t.props;
    let get = |k: &str| p.get(k).unwrap_or(&Value::Null);
    let src = |k: &str| page.source(t, k);
    let id = match get("identifier") {
        Value::Object(o) => o.get("value").and_then(f::text),
        other => f::text(other),
    }
    .or_else(|| page.page.and_then(id_in_url));
    item.offer("post_id", id.map(Value::String), src("identifier"));
    let body = if p.contains_key("articleBody") {
        "articleBody"
    } else {
        "text"
    };
    let text = f::text(get(body));
    item.offer("hashtags", text.as_deref().and_then(hashtags), src(body));
    item.offer("text", text.map(Value::String), src(body));
    item.offer(
        "date_published",
        f::iso_date(get("datePublished"), page.now).map(Value::String),
        src("datePublished"),
    );
    let mut media: Vec<Value> = f::images(get("image"), page.base)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    for video in each(get("video")) {
        let url = match video {
            Value::Object(o) => ["contentUrl", "embedUrl", "url"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str)),
            other => other.as_str(),
        };
        if let Some(u) = url.and_then(|u| f::absolute(u, page.base)) {
            let one = json!({"url": u});
            if !media.contains(&one) {
                media.push(one);
            }
        }
    }
    item.offer(
        "media_urls",
        (!media.is_empty()).then_some(Value::Array(media)),
        src("image"),
    );
    let author = match get("author") {
        Value::Array(list) => list.first().and_then(Value::as_object),
        Value::Object(o) => Some(o),
        _ => None,
    };
    item.offer(
        "author",
        author.and_then(|a| author_of(a, page)),
        src("author"),
    );
    item.offer(
        "reactions",
        reactions(get("interactionStatistic")),
        src("interactionStatistic"),
    );
}

/// `{number_of_followers, ...}` from the post's author, None when the
/// page says none of it.
fn author_of(a: &Map<String, Value>, page: &Page<'_, '_>) -> Option<Value> {
    let follows = |k: &str| {
        each(a.get(k).unwrap_or(&Value::Null))
            .filter_map(Value::as_object)
            .find(|c| action(c) == Some("FollowAction"))
            .and_then(|c| c.get("userInteractionCount"))
            .and_then(f::whole)
    };
    let location = match a.get("homeLocation") {
        Some(Value::Object(o)) => o.get("name").and_then(f::text),
        Some(other) => f::text(other),
        None => None,
    };
    let out = json!({
        "number_of_followers": follows("interactionStatistic"),
        "number_of_following": follows("agentInteractionStatistic"),
        "date_account_created": a.get("dateCreated").and_then(|d| f::iso_date(d, page.now)),
        "location": location,
        "is_verified": null,
    });
    out.as_object()?
        .values()
        .any(|v| !v.is_null())
        .then_some(out)
}

/// The action an InteractionCounter counts: `LikeAction` from
/// `https://schema.org/LikeAction` or `LikeAction`.
fn action(counter: &Map<String, Value>) -> Option<&str> {
    let ty = match counter.get("interactionType")? {
        Value::Object(o) => o.get("@type")?.as_str()?,
        other => other.as_str()?,
    };
    ty.rsplit(['/', ':']).next()
}

/// `{replies, reposts, likes, dislikes}` from interaction counters.
fn reactions(stats: &Value) -> Option<Value> {
    let mut out = json!({"replies": null, "reposts": null, "likes": null, "dislikes": null});
    let mut any = false;
    for c in each(stats).filter_map(Value::as_object) {
        let key = match action(c) {
            Some("LikeAction") => "likes",
            Some("DislikeAction") => "dislikes",
            Some("ShareAction") => "reposts",
            Some("CommentAction" | "ReplyAction") => "replies",
            _ => continue,
        };
        if let Some(n) = c.get("userInteractionCount").and_then(f::whole) {
            out[key] = json!(n);
            any = true;
        }
    }
    any.then_some(out)
}

/// A network's post page without structured data: its address names a
/// post (`/@ada/113456789`, `/status/123`, `/posts/abc`), and its Open
/// Graph tags carry the text and media.
fn from_meta(page: &Page<'_, '_>, item: &mut Builder) -> bool {
    let Some(id) = page.page.and_then(id_in_url) else {
        return false;
    };
    let text = ["og:description", "twitter:description"]
        .iter()
        .find_map(|k| Some((*k, page.meta(k).and_then(f::text)?)));
    let Some((key, text)) = text else {
        return false;
    };
    item.offer("post_id", Some(Value::String(id)), "page".into());
    item.offer("hashtags", hashtags(&text), format!("meta:{key}"));
    item.offer("text", Some(Value::String(text)), format!("meta:{key}"));
    let mut media: Vec<Value> = Vec::new();
    for key in ["og:image", "og:video", "twitter:image"] {
        let Some(Value::Array(list)) = page.meta.get(key).map(|v| match v {
            Value::Array(_) => v.clone(),
            one => Value::Array(vec![one.clone()]),
        }) else {
            continue;
        };
        for u in list.iter().filter_map(Value::as_str) {
            let lower = u.to_ascii_lowercase();
            // A network's logo or the poster's avatar isn't the post's.
            if lower.contains("logo") || lower.contains("avatar") {
                continue;
            }
            if let Some(abs) = f::absolute(u, page.base) {
                let one = json!({"url": abs});
                if !media.contains(&one) {
                    media.push(one);
                }
            }
        }
    }
    item.offer(
        "media_urls",
        (!media.is_empty()).then_some(Value::Array(media)),
        "meta:og:image".into(),
    );
    true
}

/// The post's id in its address: `/@ada/113456789`, `/status/123`,
/// `/statuses/123`, `/posts/abc-123`.
fn id_in_url(url: &url::Url) -> Option<String> {
    static POST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"/(?:@[^/]+|status(?:es)?|posts?)/([A-Za-z0-9_-]+)/?$").unwrap()
    });
    let c = POST.captures(url.path())?;
    let id = &c[1];
    // Networks number their posts; `/posts/2026-rust-tips` is a blog's.
    id.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| id.to_string())
}

/// The post's hashtags, without `#`, once each, in order; a `#` in a
/// link is a fragment, not a tag.
fn hashtags(text: &str) -> Option<Value> {
    let mut tags: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if word.contains("://") {
            continue;
        }
        let Some(rest) = word.strip_prefix('#') else {
            continue;
        };
        let tag: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !tag.is_empty() && !tag.chars().all(|c| c.is_ascii_digit()) && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    (!tags.is_empty()).then(|| json!(tags))
}

fn each(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    match v {
        Value::Array(list) => Box::new(list.iter()),
        Value::Null => Box::new(std::iter::empty()),
        one => Box::new(std::iter::once(one)),
    }
}
