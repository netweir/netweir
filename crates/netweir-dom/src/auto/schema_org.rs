//! schema.org data from JSON-LD and microdata, read into one shape so the
//! item mappers don't care which the page used.

use serde_json::{Map, Value};
use url::Url;

use crate::{Node, NodeId};

/// Where a value was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    JsonLd,
    Microdata,
    Meta,
    Dom,
}

/// One typed schema.org object on the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Thing {
    /// Short type names: `Product`, not `https://schema.org/Product`.
    pub types: Vec<String>,
    /// Properties in JSON-LD form. A nested object keeps its `@type`
    /// (shortened); a reference by `@id` is replaced by what it names.
    pub props: Map<String, Value>,
    pub source: Source,
    /// Microdata: the element with `itemscope`.
    pub element: Option<NodeId>,
    /// Microdata: the first element of each property, by name.
    pub prop_elements: Vec<(String, NodeId)>,
}

impl Thing {
    pub fn is(&self, ty: &str) -> bool {
        self.types.iter().any(|t| is_a(t, ty))
    }

    /// The element a property came from, if it came from microdata.
    pub fn element_of(&self, prop: &str) -> Option<NodeId> {
        self.prop_elements
            .iter()
            .find(|(name, _)| name == prop)
            .map(|(_, id)| *id)
    }
}

/// How deep references are expanded and microdata items read. Real
/// pages nest a handful of levels; anything past this is a page built to
/// hurt a parser.
const MAX_DEPTH: usize = 64;

/// How many values JSON-LD's references may expand to, and how many
/// microdata items may be read, on one page. Expanding a reference copies
/// what it names, so things that each name the next twice double the work
/// at every level without ever forming a cycle; past this, references
/// stay references. Real pages use a few hundred; every typed object is
/// also copied out as a thing of its own, so the work is a multiple of
/// this.
const BUDGET: usize = 10_000;

/// Every typed object on the page, from JSON-LD and microdata, nested
/// ones included, in the order they appear (JSON-LD first). In JSON-LD,
/// `@graph` is flattened and an object that is only `{"@id": ...}` is
/// replaced by the object with that id. Microdata URLs are made absolute
/// against `base`.
pub fn things(root: Node<'_>, base: Option<&Url>) -> Vec<Thing> {
    let elements: Vec<Node<'_>> = std::iter::once(root)
        .chain(root.descendants())
        .filter(|n| n.kind() == crate::NodeKind::Element)
        .collect();
    let mut out = Vec::new();
    json_ld_things(json_ld(&elements), &mut out);
    microdata_things(&elements, base, &mut out);
    out
}

/// The things of type `ty` or one of its subtypes.
pub fn of_type<'t>(things: &'t [Thing], ty: &str) -> Vec<&'t Thing> {
    things.iter().filter(|t| t.is(ty)).collect()
}

/// Subtypes the mappers treat as their parent type.
const SUBTYPES: &[(&str, &[&str])] = &[
    (
        "Product",
        &[
            "IndividualProduct",
            "ProductModel",
            "SomeProducts",
            "ProductGroup",
            "Vehicle",
            "Car",
            "Motorcycle",
        ],
    ),
    (
        "Article",
        &[
            "NewsArticle",
            "BlogPosting",
            "Report",
            "ScholarlyArticle",
            "TechArticle",
            "SocialMediaPosting",
            "LiveBlogPosting",
            "DiscussionForumPosting",
            "AnalysisNewsArticle",
            "OpinionNewsArticle",
            "ReportageNewsArticle",
            "ReviewNewsArticle",
            "BackgroundNewsArticle",
            "SatiricalArticle",
        ],
    ),
    ("Offer", &["AggregateOffer"]),
    ("ItemList", &["OfferCatalog", "BreadcrumbList"]),
    (
        "Organization",
        &["Corporation", "LocalBusiness", "OnlineStore", "Store"],
    ),
];

pub(crate) fn is_a(ty: &str, wanted: &str) -> bool {
    ty == wanted
        || SUBTYPES
            .iter()
            .any(|(parent, subs)| *parent == wanted && subs.contains(&ty))
}

/// `Product` from `Product`, `https://schema.org/Product` or
/// `schema:Product`.
fn short(ty: &str) -> String {
    let ty = ty.trim();
    ty.rsplit(['/', '#', ':']).next().unwrap_or(ty).to_string()
}

fn types_of(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(t)) => vec![short(t)],
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).map(short).collect(),
        _ => Vec::new(),
    }
}

fn json_ld(elements: &[Node<'_>]) -> Vec<Value> {
    let mut top = Vec::new();
    for value in crate::metadata::json_ld(elements) {
        match value {
            Value::Object(mut o) if o.contains_key("@graph") => match o.remove("@graph") {
                Some(Value::Array(members)) => top.extend(members),
                Some(one) => top.push(one),
                None => {}
            },
            other => top.push(other),
        }
    }
    top
}

fn json_ld_things(top: Vec<Value>, out: &mut Vec<Thing>) {
    let mut by_id: std::collections::HashMap<String, &Map<String, Value>> = Default::default();
    for v in &top {
        index(v, &mut by_id, 0);
    }
    let mut expanding = Vec::new();
    let mut budget = BUDGET;
    let resolved: Vec<Value> = top
        .iter()
        .map(|v| resolve(v, &by_id, &mut expanding, &mut budget, 0))
        .collect();
    let mut seen = std::collections::HashSet::new();
    for v in &resolved {
        collect(v, out, &mut seen, 0);
    }
}

/// Every object with an `@id` and something besides, by id.
fn index<'v>(
    v: &'v Value,
    by_id: &mut std::collections::HashMap<String, &'v Map<String, Value>>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match v {
        Value::Object(o) => {
            if let Some(id) = o.get("@id").and_then(Value::as_str)
                && o.len() > 1
            {
                by_id.entry(id.to_string()).or_insert(o);
            }
            for child in o.values() {
                index(child, by_id, depth + 1);
            }
        }
        Value::Array(list) => list.iter().for_each(|c| index(c, by_id, depth + 1)),
        _ => {}
    }
}

/// `v` with references expanded and types shortened; `expanding` holds
/// the ids being expanded above, so a cycle stays a reference, and
/// `budget` the values still to be made (see [`BUDGET`]).
fn resolve(
    v: &Value,
    by_id: &std::collections::HashMap<String, &Map<String, Value>>,
    expanding: &mut Vec<String>,
    budget: &mut usize,
    depth: usize,
) -> Value {
    if depth > MAX_DEPTH {
        return Value::Null;
    }
    *budget = budget.saturating_sub(1);
    match v {
        Value::Object(o) => {
            let id = o.get("@id").and_then(Value::as_str);
            let target = match id {
                Some(id) if o.len() == 1 && *budget > 0 && !expanding.iter().any(|e| e == id) => {
                    by_id.get(id).copied().unwrap_or(o)
                }
                _ => o,
            };
            let pushed = match target.get("@id").and_then(Value::as_str) {
                Some(id) if target.len() > 1 => {
                    expanding.push(id.to_string());
                    true
                }
                _ => false,
            };
            let mut map = Map::new();
            for (k, child) in target {
                match k.as_str() {
                    "@context" => {}
                    "@type" => {
                        let types = types_of(Some(child));
                        let value = match types.as_slice() {
                            [one] => Value::String(one.clone()),
                            _ => Value::Array(types.into_iter().map(Value::String).collect()),
                        };
                        map.insert(k.clone(), value);
                    }
                    _ => {
                        map.insert(
                            k.clone(),
                            resolve(child, by_id, expanding, budget, depth + 1),
                        );
                    }
                }
            }
            if pushed {
                expanding.pop();
            }
            Value::Object(map)
        }
        Value::Array(list) => Value::Array(
            list.iter()
                .map(|c| resolve(c, by_id, expanding, budget, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A Thing for every typed object in `v`, outermost first. An object
/// with an `@id` already in `seen` was collected where it first appeared
/// (expanded references repeat it).
fn collect(
    v: &Value,
    out: &mut Vec<Thing>,
    seen: &mut std::collections::HashSet<String>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match v {
        Value::Object(o) => {
            if let Some(id) = o.get("@id").and_then(Value::as_str)
                && o.len() > 1
                && !seen.insert(id.to_string())
            {
                return;
            }
            let types = types_of(o.get("@type"));
            if !types.is_empty() {
                let props = o
                    .iter()
                    .filter(|(k, _)| !k.starts_with('@'))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                out.push(Thing {
                    types,
                    props,
                    source: Source::JsonLd,
                    element: None,
                    prop_elements: Vec::new(),
                });
            }
            for child in o.values() {
                collect(child, out, seen, depth + 1);
            }
        }
        Value::Array(list) => list.iter().for_each(|c| collect(c, out, seen, depth + 1)),
        _ => {}
    }
}

fn microdata_things(elements: &[Node<'_>], base: Option<&Url>, out: &mut Vec<Thing>) {
    let micro = crate::metadata::Microdata::new(elements, base);
    let mut budget = BUDGET;
    for item in elements
        .iter()
        .filter(|n| n.attr("itemscope").is_some() && n.attr("itemprop").is_none())
    {
        let mut read = std::collections::HashSet::new();
        microdata_item(&micro, *item, out, &mut Vec::new(), &mut read, &mut budget);
    }
}

/// Reads one microdata item into `out` (it, then the items inside it)
/// and returns its value in JSON-LD form. `within` holds the items being
/// read above, so depth stays bounded; `read` the items already read for
/// this top-level item, so itemref loops end and shared items aren't
/// copied over and over.
fn microdata_item<'a>(
    micro: &crate::metadata::Microdata<'_, 'a>,
    node: Node<'a>,
    out: &mut Vec<Thing>,
    within: &mut Vec<NodeId>,
    read: &mut std::collections::HashSet<NodeId>,
    budget: &mut usize,
) -> Value {
    *budget = budget.saturating_sub(1);
    read.insert(node.id());
    within.push(node.id());
    let types = node
        .attr("itemtype")
        .unwrap_or_default()
        .split_whitespace()
        .map(short)
        .collect::<Vec<_>>();
    let at = out.len();
    out.push(Thing {
        types: types.clone(),
        props: Map::new(),
        source: Source::Microdata,
        element: Some(node.id()),
        prop_elements: Vec::new(),
    });
    let mut props = Map::new();
    let mut prop_elements = Vec::new();
    for prop in micro.properties(node) {
        let value = if prop.attr("itemscope").is_some() {
            // An item several itemrefs name is read once per top-level
            // item: reading it again for each would copy it into every
            // parent, doubling at each level without a cycle.
            if read.contains(&prop.id()) || within.len() >= MAX_DEPTH || *budget == 0 {
                continue;
            }
            microdata_item(micro, prop, out, within, read, budget)
        } else {
            // Search engines read `content` on any element, and pages
            // rely on it (`<span itemprop="priceCurrency" content="USD">$`),
            // though the standard reads it only on <meta>.
            let value = match prop.attr("content") {
                Some(c) if prop.tag() != Some("meta") => c.trim().to_string(),
                _ => micro.value(prop),
            };
            Value::String(value)
        };
        for name in prop.attr("itemprop").unwrap_or_default().split_whitespace() {
            if !prop_elements
                .iter()
                .any(|(n, _): &(String, NodeId)| n == name)
            {
                prop_elements.push((name.to_string(), prop.id()));
            }
            add(&mut props, name, value.clone());
        }
    }
    within.pop();
    out[at].props = props.clone();
    out[at].prop_elements = prop_elements;
    if types.is_empty() {
        // Untyped items hold data but aren't things to map.
        out.remove(at);
    } else {
        let ty = match types.as_slice() {
            [one] => Value::String(one.clone()),
            many => Value::Array(many.iter().cloned().map(Value::String).collect()),
        };
        props.insert("@type".into(), ty);
    }
    Value::Object(props)
}

/// Adds `value` under `key`, making a list when the key is already there.
fn add(map: &mut Map<String, Value>, key: &str, value: Value) {
    match map.get_mut(key) {
        None => {
            map.insert(key.to_string(), value);
        }
        Some(Value::Array(list)) => list.push(value),
        Some(existing) => {
            let first = existing.take();
            *existing = Value::Array(vec![first, value]);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::Document;

    fn read(html: &str) -> Vec<Thing> {
        let doc = Document::parse(html);
        let base = Url::parse("https://shop.example/p/1").unwrap();
        things(doc.root(), Some(&base))
    }

    fn ld(json: &str) -> String {
        format!(r#"<script type="application/ld+json">{json}</script>"#)
    }

    #[test]
    fn graph_and_id_references_are_resolved() {
        let found = read(&ld(r##"{"@context": "https://schema.org", "@graph": [
            {"@type": "Product", "@id": "#p", "name": "Lamp",
             "brand": {"@id": "#b"}, "offers": [{"@id": "#o"}]},
            {"@type": "Brand", "@id": "#b", "name": "Lumo"},
            {"@type": "Offer", "@id": "#o", "price": "19.99", "priceCurrency": "EUR"}
        ]}"##));
        let products = of_type(&found, "Product");
        assert_eq!(products.len(), 1);
        let p = &products[0].props;
        assert_eq!(p["name"], "Lamp");
        assert_eq!(p["brand"]["name"], "Lumo");
        assert_eq!(p["brand"]["@type"], "Brand");
        assert_eq!(p["offers"][0]["price"], "19.99");
        assert_eq!(products[0].source, Source::JsonLd);
        assert_eq!(
            of_type(&found, "Offer").len(),
            1,
            "nested things are things too"
        );
    }

    #[test]
    fn type_lists_and_type_urls_match() {
        let found = read(&format!(
            "{}{}{}",
            ld(r#"{"@type": ["Product", "Thing"], "name": "a"}"#),
            ld(r#"{"@type": "http://schema.org/Product", "name": "b"}"#),
            ld(r#"{"@type": "schema:IndividualProduct", "name": "c"}"#),
        ));
        let names: Vec<_> = of_type(&found, "Product")
            .iter()
            .map(|t| t.props["name"].clone())
            .collect();
        assert_eq!(names, [json!("a"), json!("b"), json!("c")]);
        assert_eq!(found[1].types, ["Product"]);
    }

    #[test]
    fn microdata_and_json_ld_give_the_same_shape() {
        let micro = read(
            r#"<div itemscope itemtype="https://schema.org/Product">
                 <h1 itemprop="name">Lamp</h1>
                 <img itemprop="image" src="/l.jpg">
                 <div itemprop="offers" itemscope itemtype="https://schema.org/Offer">
                   <data itemprop="price" value="19.99">$19.99</data>
                 </div>
               </div>"#,
        );
        let json = read(&ld(r#"{"@type": "Product", "name": "Lamp",
            "image": "https://shop.example/l.jpg",
            "offers": {"@type": "Offer", "price": "19.99"}}"#));
        let (m, j) = (of_type(&micro, "Product")[0], of_type(&json, "Product")[0]);
        assert_eq!(m.props, j.props);
        assert_eq!(m.source, Source::Microdata);
        assert!(m.element.is_some() && m.element_of("name").is_some());
        assert_eq!(m.element_of("missing"), None);
    }

    #[test]
    fn microdata_content_attributes_win_over_text() {
        let found = read(
            r#"<div itemscope itemtype="https://schema.org/Offer">
                 <span itemprop="priceCurrency" content="USD">$</span>
                 <span itemprop="price" content="28.00">28</span></div>"#,
        );
        assert_eq!(found[0].props["priceCurrency"], "USD");
        assert_eq!(found[0].props["price"], "28.00");
    }

    #[test]
    fn cycles_end() {
        let found = read(&ld(r##"{"@graph": [
            {"@type": "Person", "@id": "#a", "knows": {"@id": "#b"}},
            {"@type": "Person", "@id": "#b", "knows": {"@id": "#a"}}
        ]}"##));
        assert_eq!(of_type(&found, "Person").len(), 2);
        // A reference back to an object being expanded stays a reference.
        let a = &found[0].props;
        assert_eq!(a["knows"]["knows"], json!({"@id": "#a"}));
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let deep = format!(
            "{}{}",
            r#"{"@type": "Product", "name": "x", "a": "#.repeat(10_000),
            "}".repeat(10_001)
        );
        let start = std::time::Instant::now();
        let found = read(&ld(&deep));
        assert!(start.elapsed().as_secs_f64() < 1.0);
        // Too deep to be real data: dropped, not a crash.
        assert!(of_type(&found, "Product").is_empty());
        let mut html = String::new();
        for _ in 0..10_000 {
            html.push_str(r#"<div itemprop="x" itemscope itemtype="https://schema.org/Thing">"#);
        }
        let start = std::time::Instant::now();
        let found = read(&format!(
            r#"<div itemscope itemtype="https://schema.org/Product">{html}</div>"#
        ));
        assert!(start.elapsed().as_secs_f64() < 1.0);
        assert_eq!(of_type(&found, "Product").len(), 1);
    }

    #[test]
    fn broken_json_is_skipped_not_fatal() {
        let found = read(&format!(
            "{}{}",
            ld(r#"{"@type": "Product", "name": "#),
            ld(r#"{"@type": "Product", "name": "ok"}"#)
        ));
        assert_eq!(of_type(&found, "Product").len(), 1);
    }

    #[test]
    fn shared_references_are_bounded() {
        // Each thing names the next twice: no cycle, but expanding every
        // reference in place doubles the work at each level.
        let mut graph = vec![
            r##"{"@type": "Product", "@id": "#p", "name": "Lamp", "isRelatedTo": {"@id": "#n0"}}"##
                .to_string(),
        ];
        for i in 0..40 {
            graph.push(format!(
                r##"{{"@type": "Thing", "@id": "#n{i}", "name": "n{i}", "a": {{"@id": "#n{n}"}}, "b": {{"@id": "#n{n}"}}}}"##,
                n = i + 1
            ));
        }
        let start = std::time::Instant::now();
        let found = read(&ld(&format!(r#"{{"@graph": [{}]}}"#, graph.join(","))));
        // Seconds per level doubling without the budget; well under one
        // with it, even unoptimised on a busy runner.
        assert!(start.elapsed().as_secs_f64() < 1.0, "{:?}", start.elapsed());
        assert_eq!(of_type(&found, "Product")[0].props["name"], "Lamp");

        let mut html = String::from(
            r#"<div itemscope itemtype="https://schema.org/Product" itemref="l0 m0"><span itemprop="name">Lamp</span></div>"#,
        );
        for i in 0..40 {
            let next = format!(r#"itemref="l{n} m{n}""#, n = i + 1);
            for id in ["l", "m"] {
                html.push_str(&format!(
                    r#"<div id="{id}{i}" itemprop="isRelatedTo" itemscope itemtype="https://schema.org/Thing" {next}><span itemprop="name">t</span></div>"#
                ));
            }
        }
        let start = std::time::Instant::now();
        let found = read(&html);
        assert!(start.elapsed().as_secs_f64() < 1.0, "{:?}", start.elapsed());
        assert_eq!(of_type(&found, "Product")[0].props["name"], "Lamp");
    }
}
