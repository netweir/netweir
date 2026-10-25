//! Declarative extraction: an item's fields and a crawl's link rules,
//! compiled once and run on every page without leaving Rust.

use regex::Regex;

use std::sync::LazyLock;

use url::Url;

use crate::document::{Node, NodeKind};
use crate::query::{Hit, Query};
use crate::track::{Fingerprint, best, still_repeated};
use crate::xpath::XPath;

/// A compiled CSS (with `::text`/`::attr()`) or XPath query.
pub enum Selector {
    Css(Query),
    XPath(XPath),
}

impl Selector {
    pub fn css(query: &str) -> Result<Selector, String> {
        Query::new(query)
            .map(Selector::Css)
            .map_err(|e| e.to_string())
    }

    pub fn xpath(query: &str) -> Result<Selector, String> {
        XPath::new(query)
            .map(Selector::XPath)
            .map_err(|e| e.to_string())
    }

    pub fn hits<'a>(&self, scope: Node<'a>) -> Result<Vec<Hit<'a>>, String> {
        match self {
            Selector::Css(q) => Ok(q.run(scope)),
            Selector::XPath(x) => x.run(scope).map_err(|e| e.to_string()),
        }
    }
}

/// What a field's text is turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convert {
    Text,
    Int,
    Float,
    /// `true`/`false`, `yes`/`no`, `on`/`off` or `1`/`0`, in any case.
    Bool,
    /// An element as Markdown, links absolute against the page; with
    /// `main_only`, its main content, missing when it has none.
    Markdown {
        main_only: bool,
    },
    /// An element's text as a reader sees it (see
    /// [`crate::markdown::clean_text`]); `main_only` as for Markdown.
    CleanText {
        main_only: bool,
    },
}

impl Convert {
    /// Whether this works on the element a query found rather than on
    /// its text.
    pub fn takes_element(self) -> bool {
        matches!(self, Convert::Markdown { .. } | Convert::CleanText { .. })
    }
}

/// One field's value on one page.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Nothing matched (or the pattern found nothing).
    Missing,
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// With `all`, one value per result.
    List(Vec<Value>),
    /// A structured value, such as an auto-extracted field.
    Json(serde_json::Value),
    /// The text that would not convert.
    Invalid(String),
    /// Why the query failed on this page (an XPath type error, say).
    Error(String),
}

/// A named query and what to do with what it finds.
pub struct Field {
    name: String,
    selector: Selector,
    tracker: Option<Tracker>,
    re: Option<Regex>,
    all: bool,
    strip: bool,
    convert: Convert,
}

impl Field {
    pub fn new(name: &str, selector: Selector) -> Field {
        Field {
            name: name.to_string(),
            selector,
            tracker: None,
            re: None,
            all: false,
            strip: false,
            convert: Convert::Text,
        }
    }

    /// Keeps only what `pattern` matches in each result: its first group if
    /// it has groups (empty when that group took no part in the match, as
    /// in parsel), otherwise the whole match.
    pub fn re(mut self, pattern: &str) -> Result<Field, String> {
        let re = Regex::new(pattern).map_err(|e| {
            // The regex crate's message spans lines; the last one says why.
            let reason = e.to_string();
            let reason = reason.lines().last().unwrap_or("").trim().to_string();
            format!("bad regular expression {pattern:?}: {reason}")
        })?;
        self.re = Some(re);
        Ok(self)
    }

    /// Every result as a list, instead of the first.
    pub fn all(mut self, all: bool) -> Field {
        self.all = all;
        self
    }

    /// Trims surrounding whitespace from each result.
    pub fn strip(mut self, strip: bool) -> Field {
        self.strip = strip;
        self
    }

    pub fn convert(mut self, convert: Convert) -> Field {
        self.convert = convert;
        self
    }

    /// Follows the element through redesigns as `name` (see Tracker).
    /// Without a TrackContext when extracting, the field works as usual.
    pub fn track(mut self, tracker: Tracker) -> Field {
        self.tracker = Some(tracker);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn texts(
        &self,
        scope: Node<'_>,
        base: Option<&Url>,
        ctx: Option<&TrackContext<'_>>,
    ) -> Result<(Vec<Value>, Option<Tracking>), String> {
        let (hits, tracking) = match (&self.tracker, ctx) {
            (Some(t), Some(ctx)) => {
                let (hits, tracking) = t.run(scope, ctx)?;
                (hits, Some(tracking))
            }
            _ => (self.selector.hits(scope)?, None),
        };
        let mut out = Vec::new();
        if self.convert.takes_element() {
            for hit in hits {
                out.push(self.element_value(hit, base));
                if !self.all {
                    break;
                }
            }
            return Ok((out, tracking));
        }
        for hit in hits {
            let text = hit.to_string_value();
            match &self.re {
                Some(re) => {
                    let grouped = re.captures_len() > 1;
                    for caps in re.captures_iter(&text) {
                        let m = if grouped { caps.get(1) } else { caps.get(0) };
                        out.push(self.value(m.map_or("", |m| m.as_str()).to_string()));
                        if !self.all {
                            return Ok((out, tracking));
                        }
                    }
                }
                None => {
                    out.push(self.value(text));
                    if !self.all {
                        return Ok((out, tracking));
                    }
                }
            }
        }
        Ok((out, tracking))
    }

    fn value(&self, text: String) -> Value {
        let text = if self.strip {
            text.trim().to_string()
        } else {
            text
        };
        let trimmed = text.trim();
        match self.convert {
            Convert::Text => Value::Text(text),
            Convert::Int => trimmed.parse().map_or(Value::Invalid(text), Value::Int),
            Convert::Float => trimmed.parse().map_or(Value::Invalid(text), Value::Float),
            Convert::Bool => match trimmed.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Value::Bool(true),
                "false" | "no" | "off" | "0" => Value::Bool(false),
                _ => Value::Invalid(text),
            },
            Convert::Markdown { .. } | Convert::CleanText { .. } => Value::Invalid(text),
        }
    }

    /// An element's Markdown or clean text; a string (from `::attr()`
    /// or `::text`) has no element to convert.
    fn element_value(&self, hit: Hit<'_>, base: Option<&Url>) -> Value {
        let node = match hit {
            Hit::Node(n) if n.kind() == NodeKind::Element => n,
            other => return Value::Invalid(other.to_string_value()),
        };
        let text = match self.convert {
            Convert::Markdown { main_only: false } => Some(crate::markdown::markdown(node, base)),
            Convert::Markdown { main_only: true } => {
                crate::content::main_content(node).map(|m| m.markdown(base))
            }
            Convert::CleanText { main_only: false } => Some(crate::markdown::clean_text(node)),
            Convert::CleanText { main_only: true } => {
                crate::content::main_content(node).map(|m| m.clean_text())
            }
            _ => unreachable!("only element conversions come here"),
        };
        text.map_or(Value::Missing, Value::Text)
    }

    /// The field's value below `scope`; `base` is the page's URL (after
    /// any `<base href>`), which Markdown resolves links against.
    pub fn extract(&self, scope: Node<'_>, base: Option<&Url>) -> Value {
        self.extract_with(scope, base, None).0
    }

    /// `extract`, tracking the field's element if it's tracked and `ctx`
    /// says where.
    pub fn extract_with(
        &self,
        scope: Node<'_>,
        base: Option<&Url>,
        ctx: Option<&TrackContext<'_>>,
    ) -> (Value, Option<Tracking>) {
        let (values, tracking) = match self.texts(scope, base, ctx) {
            Ok(t) => t,
            Err(e) => return (Value::Error(e), None),
        };
        let value = if self.all {
            Value::List(values)
        } else {
            values.into_iter().next().unwrap_or(Value::Missing)
        };
        (value, tracking)
    }
}

/// Where tracked fingerprints are kept, by site and name.
pub trait Tracks: Sync {
    fn get(&self, site: &str, name: &str) -> Option<String>;
    fn put(&self, site: &str, name: &str, fingerprint: &str);
}

/// What tracking a query did on one page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tracking {
    /// The selector matched; its element's fingerprint was saved.
    Matched,
    /// The selector found nothing, and the element was found by
    /// similarity, with this score.
    Relocated(f64),
    /// The selector found nothing, and nothing scored well enough; the
    /// best candidate's score (0 if there was none).
    Lost(f64),
    /// The selector found nothing, and there is no fingerprint to look
    /// for: an element that has never been seen.
    Unknown,
}

/// Where tracking reads and writes, for one page.
pub struct TrackContext<'t> {
    pub tracks: &'t dyn Tracks,
    /// The page's host, so one name can mean different elements on
    /// different sites.
    pub site: &'t str,
    pub threshold: f64,
}

/// A tracked query, split into the part that finds the element and the
/// ending (`::text`, `::attr(x)`, `/text()`, `/@x`) that takes a value
/// from it.
pub struct Tracker {
    name: String,
    full: Selector,
    element: Selector,
    ending: Option<Selector>,
}

static CSS_ENDING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?s)(.*?)(\s*::(?:text|attr\([^)]*\)))\s*$").expect("a valid pattern")
});
static XPATH_ENDING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?s)(.*?)(/{1,2})(text\(\)|@[\w:.*-]+)\s*$").expect("a valid pattern")
});

/// Whether `query` has a comma outside brackets and quotes: a list.
fn is_list(query: &str) -> bool {
    let (mut depth, mut quote) = (0i32, None);
    for c in query.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

impl Tracker {
    /// `kind` is "css" or "xpath".
    pub fn new(name: &str, kind: &str, query: &str) -> Result<Tracker, String> {
        let compile = |q: &str| match kind {
            "css" => Selector::css(q),
            _ => Selector::xpath(q),
        };
        if kind == "css" && is_list(query) {
            return Err(format!(
                "track={name:?} follows one selector, not a list: {query:?}"
            ));
        }
        let (element, ending) = match kind {
            "css" => match CSS_ENDING.captures(query) {
                Some(c) => (c[1].to_string(), Some(c[2].trim().to_string())),
                None => (query.to_string(), None),
            },
            _ => match XPATH_ENDING.captures(query) {
                Some(c) => {
                    let relative = if &c[2] == "//" {
                        format!(".//{}", &c[3])
                    } else {
                        c[3].to_string()
                    };
                    (c[1].to_string(), Some(relative))
                }
                None => (query.to_string(), None),
            },
        };
        if element.trim().is_empty() {
            return Err(format!(
                "track={name:?} needs an element to follow, not just {query:?}"
            ));
        }
        Ok(Tracker {
            name: name.to_string(),
            full: compile(query)?,
            element: compile(&element)?,
            ending: ending.map(|e| compile(&e)).transpose()?,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The first element the query's element part finds below `scope`. A
    /// query that finds things but no element (`//p/text()[1]`) can't be
    /// followed, and says so.
    pub fn first_element<'a>(&self, scope: Node<'a>) -> Result<Option<Node<'a>>, String> {
        let hits = self.element.hits(scope)?;
        let found = !hits.is_empty();
        match hits.into_iter().find_map(|h| match h {
            Hit::Node(n) if n.kind() == NodeKind::Element => Some(n),
            _ => None,
        }) {
            None if found => Err(format!(
                "track={:?} needs a query that finds an element, with at most an ending \
                 such as ::text or /text() after it",
                self.name
            )),
            other => Ok(other),
        }
    }

    /// The query's results below `scope`. When the element is there, its
    /// fingerprint is saved; when it isn't, it's looked for by similarity.
    pub fn run<'a>(
        &self,
        scope: Node<'a>,
        ctx: &TrackContext<'_>,
    ) -> Result<(Vec<Hit<'a>>, Tracking), String> {
        if let Some(element) = self.first_element(scope)? {
            let mut fp = Fingerprint::of(element);
            if let Some(previous) = ctx
                .tracks
                .get(ctx.site, &self.name)
                .and_then(|json| Fingerprint::from_json(&json))
            {
                fp.learn_from(&previous);
            }
            ctx.tracks.put(ctx.site, &self.name, &fp.to_json());
            return Ok((self.full.hits(scope)?, Tracking::Matched));
        }
        let Some(fp) = ctx
            .tracks
            .get(ctx.site, &self.name)
            .and_then(|json| Fingerprint::from_json(&json))
        else {
            return Ok((Vec::new(), Tracking::Unknown));
        };
        if still_repeated(scope, &fp) {
            return Ok((Vec::new(), Tracking::Lost(0.0)));
        }
        match best(scope, &fp) {
            Some((node, score)) if score >= ctx.threshold => {
                let hits = match &self.ending {
                    Some(ending) => ending.hits(node)?,
                    None => vec![Hit::Node(node)],
                };
                Ok((hits, Tracking::Relocated(score)))
            }
            Some((_, score)) => Ok((Vec::new(), Tracking::Lost(score))),
            None => Ok((Vec::new(), Tracking::Lost(0.0))),
        }
    }
}

/// An item's fields, in order.
pub struct ItemSpec {
    fields: Vec<Field>,
}

impl ItemSpec {
    pub fn new(fields: Vec<Field>) -> ItemSpec {
        ItemSpec { fields }
    }

    pub fn names(&self) -> Vec<&str> {
        self.fields.iter().map(Field::name).collect()
    }

    /// One value per field, in the fields' order; `base` as for
    /// [`Field::extract`].
    pub fn extract(&self, scope: Node<'_>, base: Option<&Url>) -> Vec<Value> {
        self.fields.iter().map(|f| f.extract(scope, base)).collect()
    }

    /// `extract`, with tracked fields tracked. Also returns, by field
    /// number, each tracked field that had to be relocated or was lost.
    pub fn extract_with(
        &self,
        scope: Node<'_>,
        base: Option<&Url>,
        ctx: &TrackContext<'_>,
    ) -> (Vec<Value>, Vec<(usize, Tracking)>) {
        let mut notes = Vec::new();
        let values = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let (value, tracking) = f.extract_with(scope, base, Some(ctx));
                if let Some(t) =
                    tracking.filter(|t| matches!(t, Tracking::Relocated(_) | Tracking::Lost(_)))
                {
                    notes.push((i, t));
                }
                value
            })
            .collect();
        (values, notes)
    }
}

/// The links `selector` finds below `scope`, as written in the page: an
/// element's `href`, or the string itself when the query asks for strings
/// (`::attr(href)`, `@href`). Elements without an `href` are skipped.
pub fn links(selector: &Selector, scope: Node<'_>) -> Result<Vec<String>, String> {
    Ok(selector
        .hits(scope)?
        .into_iter()
        .filter_map(|hit| match hit {
            Hit::Node(n) if n.kind() == NodeKind::Element => n.attr("href").map(str::to_string),
            other => Some(other.to_string_value()),
        })
        .collect())
}
