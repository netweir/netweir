//! Generic records: the page's main repeated region (table rows, cards,
//! list entries) as rows with named columns, for pages no schema type
//! fits: public records, catalogues, directories, search results.
//!
//! Candidates are tables (rows of `td`, columns by position) and runs of
//! three or more siblings sharing a tag and stable classes. Navigation,
//! headers, footers and asides are passed over. A candidate needs two
//! columns of text or images in at least half its rows, so a menu (a
//! link's text and URL, one field) and prose (one paragraph per row)
//! don't count; of the rest, the most rows times columns wins.
//!
//! In a repeat, a field is an element with text of its own (read whole,
//! children included), a link's `href`, or an image's source. Fields line
//! up across rows by their tag-and-class path from the row. A column is
//! named by its table header, by a label beside it (`<dt>Phone</dt>`,
//! `Area:`), by its itemprop or class, or else by what it is.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use url::Url;

use crate::auto::fields::absolute;
use crate::css_path::{css_path, generated, segment};
use crate::document::{Node, NodeKind};
use crate::query::Query;

/// Rows read at most; a longer region is cut there.
pub const MAX_ROWS: usize = 5000;
const MIN_ROWS: usize = 3;
/// Rows of each candidate read to score it.
const SAMPLE: usize = 5;
/// Elements in a row at most: anything bigger is a page section, not a row.
const ROW_SIZE: usize = 2000;
/// The share of rows a column needs to be kept.
const KEEP: f64 = 0.25;

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// Finds the column's element in a row; `::attr()` for links and
    /// images.
    pub selector: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Records {
    /// Finds the rows on the page.
    pub selector: String,
    pub columns: Vec<Column>,
    /// One map per row, with every column: a string, or null when the row
    /// lacks it.
    pub rows: Vec<Map<String, Value>>,
}

/// The page's main repeated region, or None when nothing repeats.
pub fn records(root: Node<'_>, base: Option<&Url>) -> Option<Records> {
    let mut best: Option<(Candidate<'_>, (usize, usize))> = None;
    for c in candidates(root) {
        let Some(score) = c.score() else { continue };
        if best.as_ref().is_none_or(|(_, b)| score > *b) {
            best = Some((c, score));
        }
    }
    let (c, _) = best?;
    match c {
        Candidate::Table(t, rows) => Some(read_table(t, &rows, base)),
        Candidate::Repeat(parent, rows) => read_repeat(parent, &rows, base),
    }
}

enum Candidate<'a> {
    /// A table and its data rows.
    Table(Node<'a>, Vec<Node<'a>>),
    /// A parent and its look-alike children.
    Repeat(Node<'a>, Vec<Node<'a>>),
}

impl Candidate<'_> {
    /// Rows times columns, then text length; None when it isn't records.
    fn score(&self) -> Option<(usize, usize)> {
        let rows = match self {
            Candidate::Table(_, r) | Candidate::Repeat(_, r) => r,
        };
        let sample = &rows[..rows.len().min(SAMPLE)];
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut text = 0;
        for &row in sample {
            let fields = match self {
                Candidate::Table(..) => cells(row)
                    .into_iter()
                    .filter(|(_, c)| !squash(&c.text()).is_empty() || has_img(*c))
                    .map(|(i, _)| i.to_string())
                    .collect::<HashSet<_>>(),
                Candidate::Repeat(..) => fields(row)?
                    .into_iter()
                    .filter(|f| f.kind != Kind::Url)
                    .map(|f| f.key)
                    .collect(),
            };
            for k in fields {
                *seen.entry(k).or_default() += 1;
            }
            text += row.text().len().min(1000);
        }
        let columns = seen.values().filter(|&&n| n * 2 >= sample.len()).count();
        (columns >= 2).then_some((rows.len().min(MAX_ROWS) * columns, text))
    }
}

/// Elements whose subtree never holds records.
fn passed_over(n: Node<'_>) -> bool {
    matches!(
        n.tag(),
        Some(
            "nav"
                | "header"
                | "footer"
                | "aside"
                | "script"
                | "style"
                | "template"
                | "noscript"
                | "svg"
                | "select"
                | "head"
        )
    ) || matches!(
        n.attr("role"),
        Some("navigation" | "menu" | "menubar" | "banner" | "contentinfo")
    )
}

fn elements<'a>(n: Node<'a>) -> impl Iterator<Item = Node<'a>> {
    n.children().filter(|c| c.kind() == NodeKind::Element)
}

fn classes(n: Node<'_>) -> Vec<&str> {
    let mut out: Vec<&str> = n
        .attr("class")
        .unwrap_or_default()
        .split_ascii_whitespace()
        .filter(|c| !generated(c))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn candidates(root: Node<'_>) -> Vec<Candidate<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if passed_over(n) {
            continue;
        }
        stack.extend(elements(n));
        match n.tag() {
            Some("table") => {
                if let Some(rows) = table_rows(n) {
                    out.push(Candidate::Table(n, rows));
                }
            }
            Some("tbody" | "thead" | "tfoot" | "tr" | "dl") => {}
            _ => {
                let mut groups: HashMap<(&str, Vec<&str>), Vec<Node<'_>>> = HashMap::new();
                for c in elements(n) {
                    if let Some(tag) = c.tag() {
                        groups.entry((tag, classes(c))).or_default().push(c);
                    }
                }
                for (_, rows) in groups {
                    if rows.len() >= MIN_ROWS {
                        out.push(Candidate::Repeat(n, rows));
                    }
                }
            }
        }
    }
    out
}

/// The table's own rows (not a nested table's), header rows aside: those
/// with a `td`. None for a layout table (one column, or tables in its
/// cells) and for fewer than three rows.
fn table_rows(table: Node<'_>) -> Option<Vec<Node<'_>>> {
    let rows: Vec<Node<'_>> = own_rows(table)
        .filter(|r| elements(*r).any(|c| c.tag() == Some("td")))
        .collect();
    if rows.len() < MIN_ROWS {
        return None;
    }
    let wide = rows.iter().any(|r| cells(*r).len() >= 2);
    let holds_tables = rows
        .iter()
        .any(|r| r.descendants().any(|d| d.tag() == Some("table")));
    (wide && !holds_tables).then_some(rows)
}

fn own_rows<'a>(table: Node<'a>) -> impl Iterator<Item = Node<'a>> {
    elements(table).flat_map(|c| match c.tag() {
        Some("tr") => vec![c],
        Some("thead" | "tbody" | "tfoot") => {
            elements(c).filter(|r| r.tag() == Some("tr")).collect()
        }
        _ => vec![],
    })
}

/// A row's cells with the column each starts in, colspans counted.
fn cells(row: Node<'_>) -> Vec<(usize, Node<'_>)> {
    let mut at = 0;
    let mut out = Vec::new();
    for c in elements(row).filter(|c| matches!(c.tag(), Some("td" | "th"))) {
        out.push((at, c));
        let span: usize = c
            .attr("colspan")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);
        at += span.clamp(1, 1000);
    }
    out
}

fn has_img(n: Node<'_>) -> bool {
    n.descendants().any(|d| d.tag() == Some("img"))
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_value(n: Node<'_>) -> Value {
    match squash(&n.text()) {
        t if t.is_empty() => Value::Null,
        t => Value::String(t),
    }
}

fn link(a: Node<'_>, base: Option<&Url>) -> Option<String> {
    let href = a.attr("href")?.trim();
    if href.is_empty()
        || href.starts_with('#')
        || href.to_ascii_lowercase().starts_with("javascript:")
    {
        return None;
    }
    absolute(href, base)
}

/// An image's source, and the attribute it came from: a lazy loader's
/// attribute when `src` is missing or a placeholder.
fn image(img: Node<'_>) -> Option<(&str, &'static str)> {
    let real = |v: &&str| !v.trim().is_empty() && !v.trim_start().starts_with("data:");
    if let Some(src) = img.attr("src").filter(real) {
        return Some((src, "src"));
    }
    ["data-src", "data-lazy-src", "data-original"]
        .into_iter()
        .find_map(|a| img.attr(a).filter(real).map(|v| (v, a)))
}

/// The name for a header or label: lower snake case.
fn snake(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_end_matches('_').chars().take(40).collect()
}

/// Names made distinct in order: the second "text" becomes "text_2".
fn distinct(names: &mut [String]) {
    let mut used: HashMap<String, usize> = HashMap::new();
    for name in names.iter_mut() {
        if name.is_empty() {
            *name = "column".into();
        }
        let n = used.entry(name.clone()).or_default();
        *n += 1;
        if *n > 1 {
            let mut k = *n;
            while used.contains_key(&format!("{name}_{k}")) {
                k += 1;
            }
            *name = format!("{name}_{k}");
            used.insert(name.clone(), 1);
        }
    }
}

fn read_table(table: Node<'_>, rows: &[Node<'_>], base: Option<&Url>) -> Records {
    let rows = &rows[..rows.len().min(MAX_ROWS)];
    let header = own_rows(table)
        .filter(|r| {
            let c = cells(*r);
            !c.is_empty() && c.iter().all(|(_, c)| c.tag() == Some("th"))
        })
        .take_while(|r| r.order() < rows[0].order())
        .last();
    let heads: HashMap<usize, String> = header
        .map(|h| {
            cells(h)
                .into_iter()
                .map(|(i, c)| (i, snake(&squash(&c.text()))))
                .collect()
        })
        .unwrap_or_default();
    let read: Vec<HashMap<usize, Node<'_>>> = rows
        .iter()
        .map(|r| cells(*r).into_iter().collect())
        .collect();
    let width = read
        .iter()
        .flat_map(|r| r.keys())
        .max()
        .map_or(0, |m| m + 1);
    let keep = |n: usize| n as f64 >= KEEP * rows.len() as f64;
    // (column, what, name, selector, reader)
    let mut columns: Vec<(usize, u8, String, String)> = Vec::new();
    for i in 0..width {
        let in_rows: Vec<Node<'_>> = read.iter().filter_map(|r| r.get(&i).copied()).collect();
        let Some(&first) = in_rows.first() else {
            continue;
        };
        let name = heads
            .get(&i)
            .filter(|h| !h.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("column_{}", i + 1));
        let position = elements(first.parent().unwrap_or(first))
            .position(|c| c == first)
            .unwrap_or(0)
            + 1;
        let cell = format!("{}:nth-child({position})", first.tag().unwrap_or("td"));
        if keep(
            in_rows
                .iter()
                .filter(|c| !squash(&c.text()).is_empty())
                .count(),
        ) {
            columns.push((i, 0, name.clone(), cell.clone()));
        }
        if keep(
            in_rows
                .iter()
                .filter(|c| first_link(**c, base).is_some())
                .count(),
        ) {
            columns.push((i, 1, format!("{name}_url"), format!("{cell} a::attr(href)")));
        }
        if keep(
            in_rows
                .iter()
                .filter(|c| first_image(**c, base).is_some())
                .count(),
        ) {
            columns.push((
                i,
                2,
                format!("{name}_image"),
                format!("{cell} img::attr(src)"),
            ));
        }
    }
    let mut names: Vec<String> = columns.iter().map(|c| c.2.clone()).collect();
    distinct(&mut names);
    let out_rows = read
        .iter()
        .map(|r| {
            columns
                .iter()
                .zip(&names)
                .map(|((i, what, ..), name)| {
                    let v = r.get(i).map_or(Value::Null, |c| match what {
                        0 => text_value(*c),
                        1 => first_link(*c, base).map_or(Value::Null, Value::String),
                        _ => first_image(*c, base).map_or(Value::Null, Value::String),
                    });
                    (name.clone(), v)
                })
                .collect()
        })
        .collect();
    let at = css_path(table).unwrap_or_else(|| "table".into());
    Records {
        selector: format!("{at} > * > tr:has(> td), {at} > tr:has(> td)"),
        columns: columns
            .into_iter()
            .zip(names)
            .map(|(c, name)| Column {
                name,
                selector: c.3,
            })
            .collect(),
        rows: out_rows,
    }
}

fn first_link(cell: Node<'_>, base: Option<&Url>) -> Option<String> {
    cell.descendants()
        .filter(|d| d.tag() == Some("a"))
        .find_map(|a| link(a, base))
}

fn first_image(cell: Node<'_>, base: Option<&Url>) -> Option<String> {
    cell.descendants()
        .filter(|d| d.tag() == Some("img"))
        .find_map(|i| image(i).and_then(|(s, _)| absolute(s, base)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Url,
    Image,
}

struct Field<'a> {
    /// Tag-and-class path from the row, kind and occurrence: what lines
    /// fields up across rows.
    key: String,
    kind: Kind,
    node: Node<'a>,
    /// For a link, the text field it is, holds, or sits in.
    text_of: Option<Node<'a>>,
}

/// The fields of a row in document order, or None for a row too big to be
/// one.
fn fields(row: Node<'_>) -> Option<Vec<Field<'_>>> {
    let mut out: Vec<Field<'_>> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut size = 0;
    // (node, path to it, the text field it sits in)
    let mut stack: Vec<(Node<'_>, String, Option<Node<'_>>)> = vec![(row, String::new(), None)];
    while let Some((n, path, inside)) = stack.pop() {
        size += 1;
        if size > ROW_SIZE {
            return None;
        }
        let mut push = |key: String, kind: Kind, node, text_of| {
            let k = seen.entry(key.clone()).or_default();
            *k += 1;
            out.push(Field {
                key: format!("{key}#{k}"),
                kind,
                node,
                text_of,
            });
        };
        let mut inside = inside;
        if n != row && inside.is_none() && own_text(n) {
            push(format!("{path}:t"), Kind::Text, n, None);
            inside = Some(n);
        }
        match n.tag() {
            Some("a") if n.attr("href").is_some() => {
                let text = inside.or_else(|| {
                    n.descendants()
                        .find(|d| d.kind() == NodeKind::Element && own_text(*d))
                });
                push(format!("{path}:u"), Kind::Url, n, text);
            }
            Some("img") if image(n).is_some() => {
                push(format!("{path}:i"), Kind::Image, n, None);
            }
            _ => {}
        }
        if passed_over(n) && n != row {
            continue;
        }
        let kids: Vec<Node<'_>> = elements(n).collect();
        for c in kids.into_iter().rev() {
            let step = format!(
                "{}/{}.{}",
                path,
                c.tag().unwrap_or("*"),
                classes(c).join(".")
            );
            stack.push((c, step, inside));
        }
    }
    // The walk pushes children in reverse, so popping visits them in
    // order; the fields are in document order already.
    Some(out)
}

/// Whether the element has text directly inside it.
fn own_text(n: Node<'_>) -> bool {
    n.children()
        .any(|c| c.kind() == NodeKind::Text && c.data().is_some_and(|d| !d.trim().is_empty()))
}

struct RepeatColumn<'a> {
    key: String,
    kind: Kind,
    /// The column's field in the first row that has it.
    example: Node<'a>,
    text_of: Option<Node<'a>>,
    name: String,
}

fn read_repeat<'a>(parent: Node<'a>, rows: &[Node<'a>], base: Option<&Url>) -> Option<Records> {
    let rows = &rows[..rows.len().min(MAX_ROWS)];
    let mut read: Vec<HashMap<String, Field<'a>>> = Vec::new();
    let mut order: Vec<String> = Vec::new();
    let mut count: HashMap<String, usize> = HashMap::new();
    for &row in rows {
        let fields = fields(row).unwrap_or_default();
        let mut after: Option<usize> = None;
        for f in &fields {
            match order.iter().position(|k| *k == f.key) {
                Some(p) => after = Some(p),
                None => {
                    let at = after.map_or(0, |p| p + 1);
                    order.insert(at, f.key.clone());
                    after = Some(at);
                }
            }
            *count.entry(f.key.clone()).or_default() += 1;
        }
        read.push(fields.into_iter().map(|f| (f.key.clone(), f)).collect());
    }
    let keep = |k: &str| count[k] as f64 >= KEEP * rows.len() as f64;
    let mut columns: Vec<RepeatColumn<'a>> = order
        .iter()
        .filter(|k| keep(k))
        .filter_map(|k| {
            let f = read.iter().find_map(|r| r.get(k))?;
            Some(RepeatColumn {
                key: k.clone(),
                kind: f.kind,
                example: f.node,
                text_of: f.text_of,
                name: String::new(),
            })
        })
        .collect();
    let rows_with = |r: &HashMap<String, Field<'a>>, k: &str| r.get(k).map(|f| f.node);
    let texts: Vec<Vec<Option<String>>> = columns
        .iter()
        .map(|c| {
            read.iter()
                .map(|r| rows_with(r, &c.key).map(|n| squash(&n.text())))
                .collect()
        })
        .collect();
    // Names, first from what the field is.
    for c in &mut columns {
        c.name = match c.kind {
            Kind::Image => {
                named(c.example, rows_of(c.example, rows)).unwrap_or_else(|| "image".into())
            }
            Kind::Url => String::new(),
            Kind::Text => named(c.example, rows_of(c.example, rows)).unwrap_or_else(|| {
                match c.example.tag() {
                    Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6") => "heading",
                    Some("time") => "date",
                    Some("a") => "link",
                    _ => "text",
                }
                .into()
            }),
        };
    }
    // Labels: a label column names the next text column and goes.
    let mut drop: HashSet<usize> = HashSet::new();
    let mut strip: HashMap<usize, String> = HashMap::new();
    for i in 0..columns.len() {
        if columns[i].kind != Kind::Text {
            continue;
        }
        if let Some(label) = label(columns[i].example, &texts[i], rows.len())
            && let Some(next) = (i + 1..columns.len()).find(|&j| columns[j].kind == Kind::Text)
            && label_of(&texts[next], rows.len()).is_none()
        {
            columns[next].name = snake(&label);
            drop.insert(i);
            continue;
        }
        if let Some(prefix) = label_of(&texts[i], rows.len()) {
            columns[i].name = snake(&prefix);
            strip.insert(i, prefix);
        }
    }
    // Links are named after their text.
    for i in 0..columns.len() {
        if columns[i].kind == Kind::Url {
            let text = columns[i].text_of.and_then(|t| {
                columns
                    .iter()
                    .enumerate()
                    .find(|(j, c)| c.kind == Kind::Text && c.example == t && !drop.contains(j))
                    .map(|(_, c)| c.name.clone())
            });
            columns[i].name = match text {
                Some(t) => format!("{t}_url"),
                None => "link".into(),
            };
        }
    }
    let kept: Vec<usize> = (0..columns.len()).filter(|i| !drop.contains(i)).collect();
    let content = kept
        .iter()
        .filter(|&&i| columns[i].kind != Kind::Url)
        .count();
    if content < 2 {
        return None;
    }
    let mut names: Vec<String> = kept.iter().map(|&i| columns[i].name.clone()).collect();
    distinct(&mut names);
    let out_rows = read
        .iter()
        .map(|r| {
            kept.iter()
                .zip(&names)
                .map(|(&i, name)| {
                    let c = &columns[i];
                    let v = match r.get(&c.key) {
                        None => Value::Null,
                        Some(f) => match c.kind {
                            Kind::Text => {
                                let t = squash(&f.node.text());
                                let t = match strip.get(&i) {
                                    Some(p) => without_label(&t, p).unwrap_or(&t).to_string(),
                                    None => t,
                                };
                                if t.is_empty() {
                                    Value::Null
                                } else {
                                    Value::String(t)
                                }
                            }
                            Kind::Url => link(f.node, base).map_or(Value::Null, Value::String),
                            Kind::Image => image(f.node)
                                .and_then(|(s, _)| absolute(s, base))
                                .map_or(Value::Null, Value::String),
                        },
                    };
                    (name.clone(), v)
                })
                .collect()
        })
        .collect();
    let row_selector = {
        let first = rows[0];
        let mut s = first.tag().unwrap_or("*").to_string();
        for c in classes(first) {
            s.push('.');
            s.push_str(&crate::css_path::escape(c));
        }
        format!("{} > {s}", css_path(parent)?)
    };
    let columns = kept
        .iter()
        .zip(names)
        .map(|(&i, name)| {
            let c = &columns[i];
            let row = rows_of(c.example, rows).unwrap_or(rows[0]);
            let mut selector = relative(row, c.example);
            match c.kind {
                Kind::Url => selector.push_str("::attr(href)"),
                Kind::Image => {
                    let attr = image(c.example).map_or("src", |(_, a)| a);
                    selector.push_str(&format!("::attr({attr})"));
                }
                Kind::Text => {}
            }
            Column { name, selector }
        })
        .collect();
    Some(Records {
        selector: row_selector,
        columns,
        rows: out_rows,
    })
}

/// The row holding `n`.
fn rows_of<'a>(n: Node<'a>, rows: &[Node<'a>]) -> Option<Node<'a>> {
    std::iter::successors(n.parent(), |p| p.parent()).find(|p| rows.contains(p))
}

/// A field's name from its itemprop or classes, its own or an ancestor's
/// below the row; the row's class is taken off the front (`hit__title` in
/// `hit` is `title`).
fn named(n: Node<'_>, row: Option<Node<'_>>) -> Option<String> {
    let row_classes: Vec<&str> = row.map(classes).unwrap_or_default();
    std::iter::successors(Some(n), |p| p.parent())
        .take_while(|p| Some(*p) != row)
        .find_map(|p| {
            if let Some(prop) = p.attr("itemprop").map(snake).filter(|s| !s.is_empty()) {
                return Some(prop);
            }
            let class = p
                .attr("class")?
                .split_ascii_whitespace()
                .find(|c| !generated(c))?;
            let bare = row_classes
                .iter()
                .find_map(|r| {
                    class
                        .strip_prefix(r)
                        .map(|s| s.trim_start_matches(['-', '_']))
                })
                .filter(|s| !s.is_empty())
                .unwrap_or(class);
            Some(snake(bare)).filter(|s| !s.is_empty())
        })
}

/// The text of a label column: the same short text in most rows, written
/// as a label (`dt`, `th`, `label`, a class naming it, or ending in `:`).
fn label(example: Node<'_>, texts: &[Option<String>], rows: usize) -> Option<String> {
    let common = most_common(texts.iter().flatten().map(String::as_str))?;
    let (text, n) = common;
    if rows < MIN_ROWS || (n as f64) < 0.8 * rows as f64 || text.chars().count() > 40 {
        return None;
    }
    let looks = matches!(example.tag(), Some("dt" | "th" | "label"))
        || text.ends_with(':')
        || example
            .attr("class")
            .is_some_and(|c| c.to_ascii_lowercase().contains("label"));
    looks.then(|| text.trim_end_matches(':').trim().to_string())
}

/// The label most values start with (`Area:` in `Area: 120 m²`), when
/// most rows' values do.
fn label_of(texts: &[Option<String>], rows: usize) -> Option<String> {
    let labels = texts.iter().flatten().filter_map(|t| {
        let (l, rest) = t.split_once(':')?;
        let l = l.trim();
        (!l.is_empty() && l.chars().count() <= 30 && !rest.trim().is_empty() && !l.contains("//"))
            .then_some(l)
    });
    let (l, n) = most_common(labels)?;
    (rows >= MIN_ROWS && n as f64 >= 0.8 * texts.iter().flatten().count() as f64)
        .then(|| l.to_string())
}

fn without_label<'t>(text: &'t str, label: &str) -> Option<&'t str> {
    let (l, rest) = text.split_once(':')?;
    (l.trim() == label).then(|| rest.trim())
}

fn most_common<'t>(items: impl Iterator<Item = &'t str>) -> Option<(&'t str, usize)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut order = Vec::new();
    for i in items {
        let c = counts.entry(i).or_default();
        if *c == 0 {
            order.push(i);
        }
        *c += 1;
    }
    order
        .into_iter()
        .map(|i| (i, counts[i]))
        .max_by_key(|(_, n)| *n)
}

/// A selector for `n` that finds it, first among matches, inside `row`:
/// the shortest ending of its path from the row that does.
fn relative(row: Node<'_>, n: Node<'_>) -> String {
    let chain: Vec<Node<'_>> = std::iter::successors(Some(n), |p| p.parent())
        .take_while(|p| *p != row)
        .collect();
    let segments: Vec<String> = chain.iter().rev().map(|p| segment(*p)).collect();
    for len in 1..=segments.len() {
        let path = segments[segments.len() - len..].join(" > ");
        if Query::cached(&path).is_ok_and(|q| q.select(row).first() == Some(&n)) {
            return path;
        }
    }
    segments.join(" > ")
}
