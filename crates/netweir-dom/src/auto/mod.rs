//! Auto-extraction: typed items (products, job postings, articles and
//! more) read from a page without selectors.
//!
//! Each item type has fixed fields, named as in the common item schema
//! (in snake_case). A field takes the first value found in the page's
//! JSON-LD, then its microdata, then its meta tags, and remembers where
//! that was. Where JSON-LD and microdata disagree on a key field, the
//! item says so in its validation messages.

mod article;
pub mod fields;
mod job;
mod list;
mod product;
pub mod schema_org;

use serde_json::{Map, Value};
use url::Url;

use crate::Node;
use crate::date::Civil;
use schema_org::{Source, Thing};

/// The item types auto-extraction knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    Product,
    ProductList,
    ProductNavigation,
    JobPosting,
    Article,
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
        }
    }

    /// `product`, `product-list`, `product-navigation`, `job-posting` or
    /// `article` (underscores work too).
    pub fn from_name(name: &str) -> Option<Schema> {
        Some(match name.replace('_', "-").as_str() {
            "product" => Schema::Product,
            "product-list" => Schema::ProductList,
            "product-navigation" => Schema::ProductNavigation,
            "job-posting" => Schema::JobPosting,
            "article" => Schema::Article,
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
            "products" | "items" | "sub_categories" | "variants" | "additional_properties" => {
                return None;
            }
            "price" | "regular_price" => K::Price,
            "currency" => K::Currency,
            "currency_raw" => K::CurrencyRaw,
            "availability" => K::Availability,
            "gtin" => K::Gtin,
            "brand" => K::Brand,
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
    let ctx = Page {
        root,
        page,
        base: found_base.as_ref(),
        now,
        things: schema_org::things(root, found_base.as_ref()),
        meta: meta(root),
        elements: Default::default(),
    };
    let mut item = Builder::new(schema);
    if let Some(p) = page {
        item.set("url", Value::String(p.to_string()), "page".into());
    }
    match schema {
        Schema::Product => product::read(&ctx, &mut item),
        Schema::JobPosting => job::read(&ctx, &mut item),
        Schema::Article => article::read(&ctx, &mut item),
        Schema::ProductList => list::read_list(&ctx, &mut item),
        Schema::ProductNavigation => list::read_navigation(&ctx, &mut item),
    }
    let mut out = item.finish();
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
}

impl<'a> Page<'_, 'a> {
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
