//! Auto-extraction: typed items (products, job postings, articles and
//! more) read from a page without selectors.
//!
//! Each item type has fixed fields, named as in the common item schema
//! (in snake_case). A field takes the first value found in the page's
//! JSON-LD, then its microdata, then its meta tags, and remembers where
//! that was. Where JSON-LD and microdata disagree on a key field, the
//! item says so in its validation messages.

mod article;
mod dom;
mod feeds;
pub mod fields;
mod job;
mod js_state;
mod list;
mod product;
pub mod schema_org;

use serde_json::{Map, Value};
use url::Url;

use crate::Node;
use crate::date::Civil;
use schema_org::{Source, Thing};

/// The item types auto-extraction knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Schema {
    Product,
    ProductList,
    ProductNavigation,
    JobPosting,
    Article,
    ArticleList,
    ArticleNavigation,
    JobPostingNavigation,
    ForumThread,
}

impl Schema {
    /// The type's fields, in the order items list them.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            Schema::Product => product::FIELDS,
            Schema::ProductList => list::LIST_FIELDS,
            Schema::ProductNavigation => list::NAVIGATION_FIELDS,
            Schema::JobPosting => job::FIELDS,
            Schema::Article => article::FIELDS,
            Schema::ArticleList => feeds::ARTICLE_LIST_FIELDS,
            Schema::ArticleNavigation | Schema::JobPostingNavigation => feeds::NAVIGATION_FIELDS,
            Schema::ForumThread => feeds::THREAD_FIELDS,
        }
    }

    /// The type's name, as `from_name` reads it.
    pub fn name(self) -> &'static str {
        match self {
            Schema::Product => "product",
            Schema::ProductList => "product-list",
            Schema::ProductNavigation => "product-navigation",
            Schema::JobPosting => "job-posting",
            Schema::Article => "article",
            Schema::ArticleList => "article-list",
            Schema::ArticleNavigation => "article-navigation",
            Schema::JobPostingNavigation => "job-posting-navigation",
            Schema::ForumThread => "forum-thread",
        }
    }

    /// `product`, `product-list`, `product-navigation`, `job-posting`,
    /// `job-posting-navigation`, `article`, `article-list`,
    /// `article-navigation` or `forum-thread` (underscores work too).
    pub fn from_name(name: &str) -> Option<Schema> {
        Some(match name.replace('_', "-").as_str() {
            "product" => Schema::Product,
            "product-list" => Schema::ProductList,
            "product-navigation" => Schema::ProductNavigation,
            "job-posting" => Schema::JobPosting,
            "article" => Schema::Article,
            "article-list" => Schema::ArticleList,
            "article-navigation" => Schema::ArticleNavigation,
            "job-posting-navigation" => Schema::JobPostingNavigation,
            "forum-thread" => Schema::ForumThread,
            _ => return None,
        })
    }

    /// The kind of value a field holds, which decides how an element a
    /// person picked for it is read; None for fields no one element
    /// holds (a list's products, a group's variants), which can't be
    /// pinned.
    pub fn kind_of(self, field: &str) -> Option<fields::FieldKind> {
        use fields::FieldKind as K;
        Some(match field {
            "products"
            | "items"
            | "sub_categories"
            | "variants"
            | "additional_properties"
            | "articles"
            | "posts" => {
                return None;
            }
            "price" | "regular_price" => K::Price,
            "currency" => K::Currency,
            "currency_raw" => K::CurrencyRaw,
            "availability" => K::Availability,
            "gtin" => K::Gtin,
            "brand" | "topic" => K::Brand,
            "aggregate_rating" => K::Rating,
            "breadcrumbs" => K::Breadcrumbs,
            "main_image" => K::Image,
            "images" => K::Images,
            "features" | "requirements" => K::Texts,
            "url" | "canonical_url" => K::Url,
            "description_html" | "article_body_html" => K::Html,
            "base_salary" => K::Salary,
            "employment_type" => K::EmploymentType,
            "hiring_organization" => K::Organization,
            "job_location" => K::Location,
            "authors" => K::Authors,
            "pagination_next" => K::Link,
            "next_page" => K::Request,
            "page_number" => K::Int,
            "videos" | "audios" => K::Media,
            f if f.ends_with("_raw") => K::Text,
            f if f.starts_with("date_") || f == "valid_through" || f == "job_start_date" => K::Date,
            _ => K::Text,
        })
    }
}

/// An item read from a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted {
    /// Every field of the type, in order; Null when the page doesn't say.
    pub fields: Vec<(&'static str, Value)>,
    /// Where each field with a value came from: `json-ld`,
    /// `microdata:<css path>`, `meta:<property>`, `dom:<css path>`.
    pub sources: Vec<(&'static str, String)>,
    /// How sure it is that the page is one of these, from 0 to 1.
    pub probability: f64,
    /// Problems, by field (`item` for the item as a whole).
    pub validation: Vec<(&'static str, String)>,
}

/// The `schema` item on the page at `page`, its URL. `base` is what its
/// relative links are relative to; None works it out from `page` and the
/// page's `<base href>`. `now` is when the page was fetched, for relative
/// dates.
pub fn extract(
    schema: Schema,
    root: Node<'_>,
    page: Option<&Url>,
    base: Option<&Url>,
    now: Civil,
) -> Extracted {
    let found_base = match base {
        Some(b) => Some(b.clone()),
        None => page.map(|p| page_base(root, p)),
    };
    let ctx = Page::new(root, page, found_base.as_ref(), now);
    let mut out = read(schema, &ctx).finish();
    // Microdata sources name their element; its CSS path is worked out
    // only now, for the fields that kept a value, once per element.
    let elements = ctx.elements.into_inner();
    let mut paths: Vec<Option<Option<String>>> = vec![None; elements.len()];
    for (_, source) in &mut out.sources {
        if let Some(i) = element_index(source) {
            let path = paths[i].get_or_insert_with(|| {
                // SAFETY: the ids came from `things(root)`, nodes of this document.
                crate::css_path::css_path(unsafe { root.doc.node(elements[i]) })
            });
            *source = match path {
                Some(p) => format!("microdata:{p}"),
                None => "microdata".into(),
            };
        }
    }
    out
}

/// The type of item the page at `page` holds: a product, a listing of
/// products, a job advert, a board of jobs, a forum thread, a list of
/// articles or an article, or None when it looks like none of them.
/// With it, each type's probability, in the order tried. Arguments are
/// as for [`extract`].
pub fn page_type(
    root: Node<'_>,
    page: Option<&Url>,
    base: Option<&Url>,
    now: Civil,
) -> (Option<Schema>, Vec<(Schema, f64)>) {
    let found_base = match base {
        Some(b) => Some(b.clone()),
        None => page.map(|p| page_base(root, p)),
    };
    let ctx = Page::new(root, page, found_base.as_ref(), now);
    let mut all: Vec<(Schema, f64)> = Vec::new();
    let mut weights: Vec<f64> = Vec::new();
    for schema in [
        Schema::Product,
        Schema::JobPosting,
        Schema::ProductList,
        Schema::ForumThread,
        Schema::ArticleList,
        Schema::JobPostingNavigation,
        Schema::Article,
    ] {
        let item = read(schema, &ctx);
        // Any page with a few paragraphs reads as an article from its main
        // content alone (0.6): product pages with a long description, an
        // About page, a privacy policy. That counts for less than any other
        // type's evidence, and for nothing without a byline, a date or
        // `og:type` article.
        let weight = match schema {
            Schema::Article if item.probability < 0.7 => {
                let said = ctx
                    .meta("og:type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("article"));
                if said || item.has("authors") || item.has("date_published") {
                    item.probability.min(0.45)
                } else {
                    item.probability.min(0.3)
                }
            }
            _ => item.probability,
        };
        all.push((schema, item.probability));
        weights.push(weight);
    }
    let mut best: Option<(Schema, f64)> = None;
    for (one, w) in all.iter().zip(weights) {
        if w >= TYPED && best.is_none_or(|(_, b)| w > b) {
            best = Some((one.0, w));
        }
    }
    (best.map(|(s, _)| s), all)
}

/// The weighed probability a page needs to be typed at all.
const TYPED: f64 = 0.4;

/// Runs the reader for `schema` on the page.
fn read(schema: Schema, ctx: &Page<'_, '_>) -> Builder {
    let mut item = Builder::new(schema);
    if let Some(p) = ctx.page {
        item.set("url", Value::String(p.to_string()), "page".into());
    }
    match schema {
        Schema::Product => product::read(ctx, &mut item),
        Schema::JobPosting => job::read(ctx, &mut item),
        Schema::Article => article::read(ctx, &mut item),
        Schema::ProductList => list::read_list(ctx, &mut item),
        Schema::ProductNavigation => list::read_navigation(ctx, &mut item),
        Schema::ArticleList => feeds::read_article_list(ctx, &mut item),
        Schema::ArticleNavigation => feeds::read_navigation(ctx, &mut item, feeds::Leads::Articles),
        Schema::JobPostingNavigation => feeds::read_navigation(ctx, &mut item, feeds::Leads::Jobs),
        Schema::ForumThread => feeds::read_thread(ctx, &mut item),
    }
    item
}

/// A source that stands for a microdata element: the element's index in
/// `Page::elements`.
fn element_index(source: &str) -> Option<usize> {
    source.strip_prefix(ELEMENT)?.parse().ok()
}

/// Marks a source standing for a microdata element, until its CSS path
/// is worked out.
const ELEMENT: &str = "\u{1}microdata\u{1}";

/// What a source says it is, for messages: `json-ld`, `microdata`,
/// `meta`, `dom`, `css`.
fn kind_of_source(source: &str) -> &str {
    if source.starts_with(ELEMENT) {
        "microdata"
    } else {
        source.split(':').next().unwrap_or(source)
    }
}

/// What every item reader works from.
pub(crate) struct Page<'p, 'a> {
    pub root: Node<'a>,
    pub page: Option<&'p Url>,
    pub base: Option<&'p Url>,
    pub now: Civil,
    pub things: Vec<Thing>,
    /// Open Graph and other meta tags by property, lowercased, plus
    /// `canonical` for `<link rel=canonical>`.
    pub meta: Map<String, Value>,
    /// Microdata elements sources have named, by index.
    elements: std::cell::RefCell<Vec<crate::NodeId>>,
    /// The app state in the page's scripts, read the first time a reader
    /// asks.
    state: std::cell::OnceCell<Vec<(String, Value)>>,
}

impl<'p, 'a> Page<'p, 'a> {
    fn new(root: Node<'a>, page: Option<&'p Url>, base: Option<&'p Url>, now: Civil) -> Self {
        Page {
            root,
            page,
            base,
            now,
            things: schema_org::things(root, base),
            meta: meta(root),
            elements: Default::default(),
            state: Default::default(),
        }
    }

    /// Whether `url` is this page's address or its canonical one, written
    /// any of the ways sites write it: `http` or `https`, with or without
    /// `www.`, a trailing slash, a query string or a fragment.
    pub fn is_page_url(&self, url: &str) -> bool {
        let key = |u: &str| {
            let u = Url::parse(u).ok()?;
            let host = u.host_str()?.to_ascii_lowercase();
            let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
            Some((host, u.path().trim_end_matches('/').to_string()))
        };
        let Some(want) = fields::absolute(url, self.base).and_then(|u| key(&u)) else {
            return false;
        };
        let canonical = self
            .meta("canonical")
            .and_then(Value::as_str)
            .and_then(|c| fields::absolute(c, self.base));
        self.page.and_then(|p| key(p.as_str())) == Some(want.clone())
            || canonical.and_then(|c| key(&c)) == Some(want)
    }

    /// The app state in the page's scripts (see [`js_state`]).
    pub fn state(&self) -> &[(String, Value)] {
        self.state.get_or_init(|| js_state::roots(self.root))
    }

    /// Where a thing's property came from, as a source label.
    pub fn source(&self, thing: &Thing, prop: &str) -> String {
        match thing.source {
            Source::JsonLd => "json-ld".into(),
            // The element's CSS path costs searches of the page, so it's
            // left until `extract` knows the field kept this value.
            Source::Microdata => match thing.element_of(prop).or(thing.element) {
                Some(id) => {
                    let mut elements = self.elements.borrow_mut();
                    let i = elements.iter().position(|e| *e == id).unwrap_or_else(|| {
                        elements.push(id);
                        elements.len() - 1
                    });
                    format!("{ELEMENT}{i}")
                }
                None => "microdata".into(),
            },
            Source::Meta => "meta".into(),
            Source::Dom => "dom".into(),
        }
    }

    /// A meta tag's value (the first, if it appears more than once).
    pub fn meta(&self, key: &str) -> Option<&Value> {
        match self.meta.get(key)? {
            Value::Array(list) => list.first(),
            one => Some(one),
        }
    }
}

/// What the page's relative links are relative to: its `<base href>`
/// against its URL, or its URL.
fn page_base(root: Node<'_>, page: &Url) -> Url {
    std::iter::once(root)
        .chain(root.descendants())
        .find(|n| n.tag() == Some("base") && n.attr("href").is_some())
        .and_then(|b| page.join(b.attr("href").unwrap_or_default().trim()).ok())
        .unwrap_or_else(|| page.clone())
}

fn meta(root: Node<'_>) -> Map<String, Value> {
    let elements: Vec<Node<'_>> = std::iter::once(root)
        .chain(root.descendants())
        .filter(|n| matches!(n.tag(), Some("meta" | "link")))
        .collect();
    let (mut out, twitter, _) = crate::metadata::meta_tags(&elements);
    for (k, v) in twitter {
        out.entry(k).or_insert(v);
    }
    if let Some(href) = elements.iter().find_map(|n| {
        let rel = n.attr("rel")?;
        (n.tag() == Some("link")
            && rel
                .split_whitespace()
                .any(|r| r.eq_ignore_ascii_case("canonical")))
        .then(|| n.attr("href"))
        .flatten()
    }) {
        out.insert("canonical".into(), Value::String(href.trim().to_string()));
    }
    out
}

/// Fields compared between JSON-LD and microdata; a page that gives two
/// different answers for one of these has a problem worth reporting.
const COMPARED: [&str; 6] = [
    "name",
    "price",
    "currency",
    "availability",
    "sku",
    "job_title",
];

/// An item being filled in, first source first.
pub(crate) struct Builder {
    schema: Schema,
    values: Vec<Value>,
    sources: Vec<Option<String>>,
    pub probability: f64,
    validation: Vec<(&'static str, String)>,
}

impl Builder {
    fn new(schema: Schema) -> Builder {
        let n = schema.fields().len();
        Builder {
            schema,
            values: vec![Value::Null; n],
            sources: vec![None; n],
            probability: 0.0,
            validation: Vec::new(),
        }
    }

    fn at(&self, field: &str) -> usize {
        self.schema
            .fields()
            .iter()
            .position(|f| *f == field)
            .unwrap_or_else(|| panic!("{field} isn't a field of {:?}", self.schema))
    }

    pub fn has(&self, field: &str) -> bool {
        !self.values[self.at(field)].is_null()
    }

    pub fn get(&self, field: &str) -> &Value {
        &self.values[self.at(field)]
    }

    /// Sets `field` unless an earlier source did. A different answer
    /// from a structured source on a compared field is noted.
    pub fn offer(&mut self, field: &'static str, value: Option<Value>, source: String) {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return;
        };
        let i = self.at(field);
        match &self.sources[i] {
            None => {
                self.values[i] = value;
                self.sources[i] = Some(source);
            }
            Some(first) => {
                let structured = |s: &str| matches!(kind_of_source(s), "json-ld" | "microdata");
                if COMPARED.contains(&field)
                    && structured(first)
                    && structured(&source)
                    && self.values[i] != value
                {
                    let message = format!(
                        "{} gave {}, {} gave {}; kept the first",
                        kind_of_source(first),
                        self.values[i],
                        kind_of_source(&source),
                        value
                    );
                    self.note(field, message);
                }
            }
        }
    }

    /// Sets `field` whatever was there.
    pub fn set(&mut self, field: &'static str, value: Value, source: String) {
        let i = self.at(field);
        self.values[i] = value;
        self.sources[i] = Some(source);
    }

    pub fn note(&mut self, field: &'static str, message: String) {
        self.validation.push((field, message));
    }

    fn finish(self) -> Extracted {
        let names = self.schema.fields();
        Extracted {
            sources: names
                .iter()
                .zip(&self.sources)
                .filter_map(|(f, s)| s.clone().map(|s| (*f, s)))
                .collect(),
            fields: names.iter().copied().zip(self.values).collect(),
            probability: self.probability,
            validation: self.validation,
        }
    }
}
