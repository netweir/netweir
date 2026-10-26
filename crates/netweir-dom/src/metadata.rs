//! The structured data a page carries about itself: JSON-LD, microdata,
//! Open Graph, Twitter cards and Dublin Core.

use serde_json::{Map, Value};

use crate::{Node, NodeKind};

/// Everything a page says about itself in structured data, as a JSON
/// object with "json_ld", "microdata", "opengraph", "twitter" and
/// "dublin_core". Relative URLs in microdata are resolved against `base`.
pub fn metadata(root: Node<'_>, base: Option<&url::Url>) -> Value {
    let elements: Vec<Node<'_>> = std::iter::once(root)
        .chain(root.descendants())
        .filter(|n| n.kind() == NodeKind::Element)
        .collect();
    let mut out = Map::new();
    out.insert("json_ld".into(), Value::Array(json_ld(&elements)));
    out.insert(
        "microdata".into(),
        Value::Array(Microdata::new(&elements, base).items()),
    );
    let (opengraph, twitter, dublin_core) = meta_tags(&elements);
    out.insert("opengraph".into(), Value::Object(opengraph));
    out.insert("twitter".into(), Value::Object(twitter));
    out.insert("dublin_core".into(), Value::Object(dublin_core));
    Value::Object(out)
}

/// Every `<script type="application/ld+json">`'s JSON, a top-level list's
/// members one by one. A block wrapped in an HTML comment or CDATA, as
/// some pages hide them, is unwrapped; one that still isn't JSON is
/// skipped.
pub(crate) fn json_ld(elements: &[Node<'_>]) -> Vec<Value> {
    let mut out = Vec::new();
    for script in elements.iter().filter(|n| n.tag() == Some("script")) {
        let kind = script.attr("type").unwrap_or_default();
        let kind = kind.split(';').next().unwrap_or_default().trim();
        if !kind.eq_ignore_ascii_case("application/ld+json") {
            continue;
        }
        let text = script.text();
        let mut text = text.trim();
        for (open, close) in [("<!--", "-->"), ("<![CDATA[", "]]>")] {
            if let Some(inner) = text.strip_prefix(open).and_then(|t| t.strip_suffix(close)) {
                text = inner.trim();
            }
        }
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(items)) => out.extend(items),
            Ok(value) => out.push(value),
            Err(_) => {}
        }
    }
    out
}

/// Open Graph (and its `article:`, `product:` and other namespaces),
/// Twitter card and Dublin Core `<meta>` tags. A key that appears more
/// than once becomes a list, in order.
pub(crate) fn meta_tags(
    elements: &[Node<'_>],
) -> (Map<String, Value>, Map<String, Value>, Map<String, Value>) {
    const OPEN_GRAPH: [&str; 8] = [
        "og:", "article:", "product:", "book:", "profile:", "music:", "video:", "fb:",
    ];
    let (mut og, mut twitter, mut dc) = (Map::new(), Map::new(), Map::new());
    for meta in elements.iter().filter(|n| n.tag() == Some("meta")) {
        let Some(content) = meta.attr("content") else {
            continue;
        };
        let key = meta
            .attr("property")
            .or_else(|| meta.attr("name"))
            .unwrap_or_default()
            .trim();
        let lower = key.to_ascii_lowercase();
        let target = if OPEN_GRAPH.iter().any(|p| lower.starts_with(p)) {
            &mut og
        } else if lower.starts_with("twitter:") {
            &mut twitter
        } else if lower.starts_with("dc.") || lower.starts_with("dcterms.") {
            &mut dc
        } else {
            continue;
        };
        add(target, &lower, Value::String(content.to_string()));
    }
    (og, twitter, dc)
}

/// Adds `value` under `key`, making a list when the key is already there.
/// Values are strings and items, never lists, so a list is one this made.
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

/// Microdata, read as the HTML standard's "microdata" section describes.
pub(crate) struct Microdata<'d, 'a> {
    elements: &'d [Node<'a>],
    base: Option<&'d url::Url>,
    /// Document order, for sorting an item's properties.
    order: std::collections::HashMap<crate::NodeId, usize>,
    /// Elements by id, for itemref.
    by_id: std::collections::HashMap<&'a str, Node<'a>>,
}

impl<'d, 'a> Microdata<'d, 'a> {
    pub(crate) fn new(elements: &'d [Node<'a>], base: Option<&'d url::Url>) -> Self {
        let order = elements
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id(), i))
            .collect();
        let mut by_id = std::collections::HashMap::new();
        for n in elements {
            if let Some(id) = n.attr("id") {
                by_id.entry(id).or_insert(*n);
            }
        }
        Microdata {
            elements,
            base,
            order,
            by_id,
        }
    }

    /// The top-level items: elements with itemscope and no itemprop.
    fn items(&self) -> Vec<Value> {
        self.elements
            .iter()
            .filter(|n| n.attr("itemscope").is_some() && n.attr("itemprop").is_none())
            .map(|n| self.item(*n, &mut std::collections::HashSet::new(), 0))
            .collect()
    }

    /// One item; `read` holds the items already read for this top-level
    /// item, so one that contains itself through itemref ends instead of
    /// looping, and one several itemrefs name is read once rather than
    /// copied into every parent (which doubles at each level). Items
    /// nested deeper than 64 are left out.
    fn item(
        &self,
        node: Node<'a>,
        read: &mut std::collections::HashSet<crate::NodeId>,
        depth: usize,
    ) -> Value {
        read.insert(node.id());
        let mut item = Map::new();
        let types: Vec<&str> = node
            .attr("itemtype")
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        match types.as_slice() {
            [] => {}
            [one] => {
                item.insert("type".into(), Value::String((*one).to_string()));
            }
            many => {
                item.insert(
                    "type".into(),
                    Value::Array(
                        many.iter()
                            .map(|t| Value::String((*t).to_string()))
                            .collect(),
                    ),
                );
            }
        }
        if let Some(id) = node.attr("itemid") {
            item.insert("id".into(), Value::String(self.absolute(id.trim())));
        }
        let mut properties = Map::new();
        for prop in self.properties(node) {
            let value = if prop.attr("itemscope").is_some() {
                if read.contains(&prop.id()) || depth >= 64 {
                    continue;
                }
                self.item(prop, read, depth + 1)
            } else {
                Value::String(self.value(prop))
            };
            for name in prop.attr("itemprop").unwrap_or_default().split_whitespace() {
                add(&mut properties, name, value.clone());
            }
        }
        item.insert("properties".into(), Value::Object(properties));
        Value::Object(item)
    }

    /// The elements whose itemprop belongs to `item`, in document order:
    /// its descendants and those its itemref names, not looking inside
    /// nested items.
    pub(crate) fn properties(&self, item: Node<'a>) -> Vec<Node<'a>> {
        let mut pending: Vec<Node<'a>> = item.children().collect();
        for id in item.attr("itemref").unwrap_or_default().split_whitespace() {
            if let Some(n) = self.by_id.get(id) {
                pending.push(*n);
            }
        }
        let mut seen = std::collections::HashSet::new();
        let mut found = Vec::new();
        while let Some(n) = pending.pop() {
            if n == item || !seen.insert(n.id()) || n.kind() != NodeKind::Element {
                continue;
            }
            if n.attr("itemscope").is_none() {
                pending.extend(n.children());
            }
            if n.attr("itemprop").is_some() {
                found.push(n);
            }
        }
        found.sort_by_key(|n| self.order.get(&n.id()).copied().unwrap_or(usize::MAX));
        found
    }

    /// A property's value, by its element, as the standard gives it.
    pub(crate) fn value(&self, n: Node<'a>) -> String {
        let attr = |name: &str| n.attr(name).unwrap_or_default().to_string();
        match n.tag().unwrap_or_default() {
            "meta" => attr("content"),
            "audio" | "embed" | "iframe" | "img" | "source" | "track" | "video" => {
                self.absolute(n.attr("src").unwrap_or_default())
            }
            "a" | "area" | "link" => self.absolute(n.attr("href").unwrap_or_default()),
            "object" => self.absolute(n.attr("data").unwrap_or_default()),
            "data" | "meter" => attr("value"),
            "time" => n
                .attr("datetime")
                .map(str::to_string)
                .unwrap_or_else(|| n.text()),
            _ => n.text().trim().to_string(),
        }
    }

    fn absolute(&self, url: &str) -> String {
        let url = url.trim();
        match self.base.and_then(|b| b.join(url).ok()) {
            Some(u) if !url.is_empty() => u.to_string(),
            _ => url.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Document;

    fn meta(html: &str) -> serde_json::Value {
        let doc = Document::parse(html);
        let base = url::Url::parse("https://shop.example/p/1").unwrap();
        super::metadata(doc.root(), Some(&base))
    }

    #[test]
    fn json_ld_blocks_are_read_and_top_level_lists_flattened() {
        let m = meta(
            r##"<script type="application/ld+json">{"@type": "Product", "name": "Lamp"}</script>
               <script type="application/ld+json">[{"@type": "A"}, {"@type": "B"}]</script>
               <script type="Application/LD+JSON; charset=utf-8">
                 <!-- {"@type": "Wrapped"} -->
               </script>
               <script type="application/ld+json">{"broken": </script>
               <script type="text/javascript">{"@type": "NotMe"}</script>"##,
        );
        assert_eq!(
            m["json_ld"],
            json!([
                {"@type": "Product", "name": "Lamp"},
                {"@type": "A"},
                {"@type": "B"},
                {"@type": "Wrapped"}
            ])
        );
    }

    #[test]
    fn microdata_follows_the_html_standard() {
        let m = meta(
            r##"<div itemscope itemtype="https://schema.org/Product" itemid="#lamp" itemref="extra">
                 <h1 itemprop="name">Desk lamp</h1>
                 <img itemprop="image" src="/img/lamp.jpg">
                 <a itemprop="url" href="lamp">Link</a>
                 <meta itemprop="sku" content="L-1">
                 <span itemprop="category tag">Lighting</span>
                 <div itemprop="offers" itemscope itemtype="https://schema.org/Offer">
                   <data itemprop="price" value="19.99">$19.99</data>
                   <time itemprop="validFrom" datetime="2026-10-01">Oct 1</time>
                 </div>
                 <span itemprop="colour">Red</span><span itemprop="colour">Blue</span>
               </div>
               <p id="extra"><span itemprop="brand">Lumo</span></p>"##,
        );
        assert_eq!(
            m["microdata"],
            json!([{
                "type": "https://schema.org/Product",
                "id": "https://shop.example/p/1#lamp",
                "properties": {
                    "name": "Desk lamp",
                    "image": "https://shop.example/img/lamp.jpg",
                    "url": "https://shop.example/p/lamp",
                    "sku": "L-1",
                    "category": "Lighting",
                    "tag": "Lighting",
                    "offers": {
                        "type": "https://schema.org/Offer",
                        "properties": {"price": "19.99", "validFrom": "2026-10-01"}
                    },
                    "colour": ["Red", "Blue"],
                    "brand": "Lumo"
                }
            }])
        );
    }

    #[test]
    fn open_graph_twitter_and_dublin_core_come_from_meta_tags() {
        let m = meta(
            r##"<meta property="og:title" content="Desk lamp">
               <meta property="og:image" content="https://cdn.example/1.jpg">
               <meta property="og:image" content="https://cdn.example/2.jpg">
               <meta property="product:price:amount" content="19.99">
               <meta name="og:type" content="product">
               <meta name="twitter:card" content="summary_large_image">
               <meta property="twitter:site" content="@shop">
               <meta name="DC.title" content="Desk lamp">
               <meta name="dcterms.creator" content="Lumo">
               <meta name="description" content="not structured">"##,
        );
        assert_eq!(
            m["opengraph"],
            json!({
                "og:title": "Desk lamp",
                "og:image": ["https://cdn.example/1.jpg", "https://cdn.example/2.jpg"],
                "product:price:amount": "19.99",
                "og:type": "product"
            })
        );
        assert_eq!(
            m["twitter"],
            json!({"twitter:card": "summary_large_image", "twitter:site": "@shop"})
        );
        assert_eq!(
            m["dublin_core"],
            json!({"dc.title": "Desk lamp", "dcterms.creator": "Lumo"})
        );
    }

    #[test]
    fn a_page_without_any_has_empty_sections() {
        let m = meta("<p>plain</p>");
        assert_eq!(
            m,
            json!({"json_ld": [], "microdata": [], "opengraph": {}, "twitter": {}, "dublin_core": {}})
        );
    }

    #[test]
    fn an_item_that_refers_to_itself_does_not_loop() {
        let m = meta(r##"<div id="a" itemscope itemref="a"><span itemprop="x">1</span></div>"##);
        assert_eq!(m["microdata"][0]["properties"]["x"], "1");
    }

    #[test]
    fn items_shared_by_itemrefs_are_read_once() {
        let mut html = String::from(
            r#"<div itemscope itemtype="https://schema.org/Product" itemref="l0 m0"><span itemprop="name">Lamp</span></div>"#,
        );
        for i in 0..40 {
            let next = format!(r#"itemref="l{n} m{n}""#, n = i + 1);
            for id in ["l", "m"] {
                html.push_str(&format!(
                    r#"<div id="{id}{i}" itemprop="related" itemscope {next}><span itemprop="name">t</span></div>"#
                ));
            }
        }
        let start = std::time::Instant::now();
        let m = meta(&html);
        assert!(start.elapsed().as_secs_f64() < 1.0, "{:?}", start.elapsed());
        assert_eq!(m["microdata"][0]["properties"]["name"], "Lamp");
    }
}
