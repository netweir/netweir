//! Generic records: the page's main repeated region (table rows, cards,
//! list entries) as rows with named columns, for pages no schema type
//! fits: public records, catalogues, directories, search results.
//!
//! Candidates are tables (rows of `td`, columns by position, spans
//! counted) and runs of three or more siblings with one tag that share
//! their classes (`card` and `card card--sale` are one run). Navigation,
//! headers, footers, asides, sidebars, widgets and menus are passed over.
//! A candidate needs two columns of text or images in at least half its
//! rows, so a menu (a link's text and URL, one field) and prose (one
//! paragraph per row) don't count. Of the rest, the most columns squared
//! times rows wins, so ten rich rows beat fifty category links with
//! their counts; on a tie, the first on the page.
//!
//! In a repeat, a field is an element with text of its own (read whole,
//! children included), a link's `href`, or an image's source. Fields line
//! up across rows by their tag-and-class path from the row, aligned the
//! way a diff lines up two files, so an element only some rows have
//! doesn't push the rest into the wrong column. A column is named by its
//! table header, by a label beside it (`<dt>Phone</dt>`, `Area:`), by its
//! itemprop or class, or else by what it is.
//!
//! A column's selector is written from one row, so in a row shaped
//! differently (a cell after a colspan, an element only some rows have)
//! it can find another element or nothing.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};
use url::Url;

use crate::auto::fields::absolute;
use crate::css_path::{css_path, escape, generated, segment};
use crate::document::{Node, NodeKind};
use crate::query::Query;

/// Rows read at most; a longer region is cut there.
pub const MAX_ROWS: usize = 5000;
const MIN_ROWS: usize = 3;
/// Rows of each candidate read to score it.
const SAMPLE: usize = 5;
/// Elements in a row, and levels below it, at most: anything bigger is a
/// page section, not a row.
const ROW_SIZE: usize = 2000;
const ROW_DEPTH: usize = 32;
/// Fields read from one row, and columns kept across rows, at most: what
/// keeps lining fields up quick.
const ROW_FIELDS: usize = 64;
const MAX_COLUMNS: usize = 256;
/// The share of rows a column needs to be kept (and two rows at least).
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
        // Candidates come in document order, so the first of equals stays.
        if best.as_ref().is_none_or(|(_, b)| score > *b) {
            best = Some((c, score));
        }
    }
    let (c, _) = best?;
    match c {
        Candidate::Table(t, rows) => Some(read_table(t, &rows, base)),
        Candidate::Repeat(rows, tag, shared) => {
            let mut row_selector = escape(tag);
            for c in &shared {
                row_selector.push('.');
                row_selector.push_str(&escape(c));
            }
            let parent = css_path(rows[0].parent()?)?;
            let selector = if rows.len() > MAX_ROWS {
                format!("{parent} > {row_selector}:nth-child(-n+{MAX_ROWS} of {row_selector})")
            } else {
                format!("{parent} > {row_selector}")
            };
            read_repeat(selector, &rows, base)
        }
    }
}

enum Candidate<'a> {
    /// A table and its data rows.
    Table(Node<'a>, Vec<Node<'a>>),
    /// A run of look-alike siblings, and the tag and classes that find it
    /// under its parent.
    Repeat(Vec<Node<'a>>, &'a str, Vec<&'a str>),
}

impl Candidate<'_> {
    fn rows(&self) -> &[Node<'_>] {
        match self {
            Candidate::Table(_, r) | Candidate::Repeat(r, ..) => r,
        }
    }

    /// Columns squared times rows, then text length; None when it isn't
    /// records.
    fn score(&self) -> Option<(usize, usize)> {
        let rows = self.rows();
        let sample = &rows[..rows.len().min(SAMPLE)];
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut text = 0;
        for &row in sample {
            let fields: HashSet<String> = match self {
                Candidate::Table(..) => cells(row)
                    .into_iter()
                    .filter(|(_, c)| !squash(&c.text()).is_empty() || has_img(*c))
                    .map(|(i, _)| i.to_string())
                    .collect(),
                Candidate::Repeat(..) => {
                    let mut n: HashMap<&str, usize> = HashMap::new();
                    let fields = fields(row)?;
                    fields
                        .iter()
                        .filter(|f| f.kind != Kind::Url)
                        .map(|f| {
                            let k = n.entry(&f.key).or_default();
                            *k += 1;
                            format!("{}#{k}", f.key)
                        })
                        .collect()
                }
            };
            for k in fields {
                *seen.entry(k).or_default() += 1;
            }
            text += row.text().len().min(1000);
        }
        let columns = seen.values().filter(|&&n| n * 2 >= sample.len()).count();
        (columns >= 2).then_some((rows.len().min(MAX_ROWS) * columns * columns, text))
    }
}

/// Elements whose subtree never holds records.
fn passed_over(n: Node<'_>) -> bool {
    if matches!(
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
    ) {
        return true;
    }
    // `sidebar`, `widget-area`, `menu-main`, `nav-links`; not
    // `with-sidebar` on the page's wrapper.
    let names = n.attr("class").unwrap_or_default().split_ascii_whitespace();
    names.chain(n.attr("id")).any(|name| {
        let name = name.to_ascii_lowercase();
        [
            "sidebar",
            "widget",
            "menu",
            "navbar",
            "breadcrumb",
            "footer",
        ]
        .iter()
        .any(|p| name.starts_with(p))
            || name == "nav"
            || name.starts_with("nav-")
            || name.starts_with("nav_")
    })
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

/// Every candidate, in document order.
fn candidates(root: Node<'_>) -> Vec<Candidate<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if passed_over(n) {
            continue;
        }
        let kids: Vec<Node<'_>> = elements(n).collect();
        stack.extend(kids.iter().rev());
        match n.tag() {
            Some("table") => {
                if let Some(rows) = table_rows(n) {
                    out.push(Candidate::Table(n, rows));
                }
            }
            Some("tbody" | "thead" | "tfoot" | "tr" | "dl") => {}
            _ => runs(&kids, &mut out),
        }
    }
    out
}

/// The runs among a parent's children: those with one tag, joined when
/// they share a class, each found again by the tag and the classes all of
/// it shares.
fn runs<'a>(kids: &[Node<'a>], out: &mut Vec<Candidate<'a>>) {
    let mut tags: Vec<&str> = Vec::new();
    for k in kids {
        if let Some(t) = k.tag()
            && !tags.contains(&t)
        {
            tags.push(t);
        }
    }
    let mut made: HashSet<Vec<usize>> = HashSet::new();
    for tag in tags {
        let same: Vec<Node<'a>> = kids
            .iter()
            .copied()
            .filter(|k| k.tag() == Some(tag))
            .collect();
        if same.len() < MIN_ROWS {
            continue;
        }
        // Union-find over the siblings: a shared class joins two, and
        // those without classes are one group.
        let mut group: Vec<usize> = (0..same.len()).collect();
        fn find(g: &mut [usize], i: usize) -> usize {
            let mut r = i;
            while g[r] != r {
                r = g[r];
            }
            g[i] = r;
            r
        }
        let mut first: HashMap<&str, usize> = HashMap::new();
        let mut bare: Option<usize> = None;
        for (i, k) in same.iter().enumerate() {
            let cs = classes(*k);
            if cs.is_empty() {
                let b = *bare.get_or_insert(i);
                let (a, b) = (find(&mut group, i), find(&mut group, b));
                group[a] = b;
            }
            for c in cs {
                let j = *first.entry(c).or_insert(i);
                let (a, b) = (find(&mut group, i), find(&mut group, j));
                group[a] = b;
            }
        }
        let mut members: Vec<(usize, Vec<usize>)> = Vec::new();
        for i in 0..same.len() {
            let r = find(&mut group, i);
            match members.iter_mut().find(|(g, _)| *g == r) {
                Some((_, m)) => m.push(i),
                None => members.push((r, vec![i])),
            }
        }
        for (_, m) in members {
            if m.len() < MIN_ROWS {
                continue;
            }
            let mut shared: Vec<&str> = classes(same[m[0]]);
            for &i in &m[1..] {
                let theirs = classes(same[i]);
                shared.retain(|c| theirs.contains(c));
            }
            // The rows are what the selector finds, so the two agree.
            let rows: Vec<usize> = (0..same.len())
                .filter(|&i| {
                    let theirs = classes(same[i]);
                    shared.iter().all(|c| theirs.contains(c))
                })
                .collect();
            if rows.len() < MIN_ROWS || !made.insert(rows.clone()) {
                continue;
            }
            out.push(Candidate::Repeat(
                rows.into_iter().map(|i| same[i]).collect(),
                tag,
                shared,
            ));
        }
    }
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

fn span(c: Node<'_>, attr: &str) -> usize {
    c.attr(attr)
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 1000)
}

/// A row's own cells with the column each starts in, colspans counted.
fn cells(row: Node<'_>) -> Vec<(usize, Node<'_>)> {
    let mut at = 0;
    let mut out = Vec::new();
    for c in elements(row).filter(|c| matches!(c.tag(), Some("td" | "th"))) {
        out.push((at, c));
        at += span(c, "colspan");
    }
    out
}

/// Each row's cells by the column they start in, rowspans carried into
/// the rows below as a browser lays them out. The flag marks a cell
/// carried down from a row above.
fn grid<'a>(rows: &[Node<'a>]) -> Vec<Vec<(usize, Node<'a>, bool)>> {
    // Per column: rows still to cover, and the cell (None past its first
    // column, so a wide cell's value is read once).
    let mut carry: Vec<(usize, Option<Node<'a>>)> = Vec::new();
    let mut out = Vec::new();
    for &row in rows {
        let mut placed = Vec::new();
        let mut at = 0;
        for c in elements(row).filter(|c| matches!(c.tag(), Some("td" | "th"))) {
            take_carried(&mut carry, &mut at, &mut placed);
            placed.push((at, c, false));
            let (wide, tall) = (span(c, "colspan"), span(c, "rowspan"));
            if carry.len() < at + wide {
                carry.resize(at + wide, (0, None));
            }
            if tall > 1 {
                for (k, slot) in carry[at..at + wide].iter_mut().enumerate() {
                    *slot = (tall - 1, (k == 0).then_some(c));
                }
            }
            at += wide;
        }
        // Carried cells after the row's last own one.
        while at < carry.len() {
            take_carried(&mut carry, &mut at, &mut placed);
            at += 1;
        }
        out.push(placed);
    }
    out
}

/// The cells carried down into this row from `at` on, up to the next
/// free column.
fn take_carried<'a>(
    carry: &mut [(usize, Option<Node<'a>>)],
    at: &mut usize,
    placed: &mut Vec<(usize, Node<'a>, bool)>,
) {
    while *at < carry.len() && carry[*at].0 > 0 {
        carry[*at].0 -= 1;
        if let Some(c) = carry[*at].1 {
            placed.push((*at, c, true));
        }
        *at += 1;
    }
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

/// The links `link` reads, as a selector.
const LINK: &str = r##"a[href]:not([href=""]):not([href^="#"]):not([href^="javascript:" i])"##;

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

/// The attribute most of these images take their source from.
fn image_attr<'a>(imgs: impl Iterator<Item = Node<'a>>) -> &'static str {
    most_common(imgs.filter_map(|i| image(i).map(|(_, a)| a))).map_or("src", |(a, _)| a)
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

/// Whether `n` of `rows` is enough to keep a column.
fn enough(n: usize, rows: usize) -> bool {
    n >= rows.min(2) && n as f64 >= KEEP * rows as f64
}

fn read_table<'a>(table: Node<'a>, rows: &[Node<'a>], base: Option<&Url>) -> Records {
    let all: Vec<Node<'_>> = own_rows(table).collect();
    let laid = grid(&all);
    let header = all
        .iter()
        .zip(&laid)
        .take_while(|(r, _)| r.order() < rows[0].order())
        .filter(|(_, cells)| {
            !cells.is_empty() && cells.iter().all(|(_, c, _)| c.tag() == Some("th"))
        })
        .last();
    let heads: HashMap<usize, String> = header
        .map(|(_, cells)| {
            cells
                .iter()
                .map(|(i, c, _)| (*i, snake(&squash(&c.text()))))
                .collect()
        })
        .unwrap_or_default();
    let wanted: HashSet<u32> = rows.iter().take(MAX_ROWS).map(|r| r.order()).collect();
    let read: Vec<HashMap<usize, (Node<'_>, bool)>> = all
        .iter()
        .zip(&laid)
        .filter(|(r, _)| wanted.contains(&r.order()))
        .map(|(_, cells)| {
            cells
                .iter()
                .map(|(i, c, carried)| (*i, (*c, *carried)))
                .collect()
        })
        .collect();
    let width = read
        .iter()
        .flat_map(|r| r.keys())
        .max()
        .map_or(0, |m| m + 1);
    let keep = |n: usize| enough(n, read.len());
    // (column, what: 0 text, 1 link, 2 image, name, selector)
    let mut columns: Vec<(usize, u8, String, String)> = Vec::new();
    for i in 0..width {
        let in_rows: Vec<Node<'_>> = read.iter().filter_map(|r| r.get(&i).map(|c| c.0)).collect();
        if in_rows.is_empty() {
            continue;
        }
        let name = heads
            .get(&i)
            .filter(|h| !h.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("column_{}", i + 1));
        // Written from a row whose cells line up with the columns up to
        // this one, so it finds this column in rows without spans.
        let own = |r: &HashMap<usize, (Node<'a>, bool)>| {
            let (cell, carried) = *r.get(&i)?;
            let row = cell.parent()?;
            let index = elements(row).position(|c| c == cell)?;
            let lined_up = !carried
                && elements(row)
                    .take(index + 1)
                    .all(|c| matches!(c.tag(), Some("td" | "th")) && span(c, "colspan") == 1)
                && (0..i).all(|k| r.get(&k).is_some_and(|(_, carried)| !carried));
            lined_up.then_some((cell, index))
        };
        let (example, index) = read
            .iter()
            .find_map(own)
            .or_else(|| {
                read.iter().find_map(|r| {
                    let (c, carried) = *r.get(&i)?;
                    let index = elements(c.parent()?).position(|x| x == c)?;
                    (!carried).then_some((c, index))
                })
            })
            .unwrap_or((in_rows[0], i));
        let cell = format!("{}:nth-child({})", example.tag().unwrap_or("td"), index + 1);
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
            columns.push((
                i,
                1,
                format!("{name}_url"),
                format!("{cell} {LINK}::attr(href)"),
            ));
        }
        if keep(
            in_rows
                .iter()
                .filter(|c| first_image(**c, base).is_some())
                .count(),
        ) {
            let attr = image_attr(in_rows.iter().filter_map(|c| {
                c.descendants()
                    .find(|d| d.tag() == Some("img") && image(*d).is_some())
            }));
            columns.push((
                i,
                2,
                format!("{name}_image"),
                format!("{cell} img[{attr}]::attr({attr})"),
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
                    let v = r.get(i).map_or(Value::Null, |(c, _)| match what {
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
    let row_selector = "tr:has(> td)";
    // ponytail: the cap applies per row parent, so tables split across multiple tbody elements can exceed MAX_ROWS.
    let selector = if rows.len() > MAX_ROWS {
        format!(
            "{at} > * > {row_selector}:nth-child(-n+{MAX_ROWS} of {row_selector}), {at} > {row_selector}:nth-child(-n+{MAX_ROWS} of {row_selector})"
        )
    } else {
        format!("{at} > * > {row_selector}, {at} > {row_selector}")
    };
    Records {
        selector,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Text,
    Url,
    Image,
}

struct Field<'a> {
    /// Tag-and-class path from the row, and kind: what lines fields up
    /// across rows.
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
    let mut size = 0;
    // (node, path to it, its depth, the text field it sits in)
    let mut stack: Vec<(Node<'_>, String, usize, Option<Node<'_>>)> =
        vec![(row, String::new(), 0, None)];
    while let Some((n, path, depth, inside)) = stack.pop() {
        size += 1;
        if size > ROW_SIZE || depth > ROW_DEPTH {
            return None;
        }
        let mut inside = inside;
        if n != row && inside.is_none() && own_text(n) {
            out.push(Field {
                key: format!("{path}:t"),
                kind: Kind::Text,
                node: n,
                text_of: None,
            });
            inside = Some(n);
        }
        match n.tag() {
            Some("a") if link(n, None).is_some() => {
                let text = inside.or_else(|| {
                    n.descendants()
                        .find(|d| d.kind() == NodeKind::Element && own_text(*d))
                });
                out.push(Field {
                    key: format!("{path}:u"),
                    kind: Kind::Url,
                    node: n,
                    text_of: text,
                });
            }
            Some("img") if image(n).is_some() => out.push(Field {
                key: format!("{path}:i"),
                kind: Kind::Image,
                node: n,
                text_of: None,
            }),
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
            stack.push((c, step, depth + 1, inside));
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

/// What a text looks like, to tell one plain `<span>` from the next:
/// whether it holds digits, and roughly how long it is.
fn shape(n: Node<'_>) -> (bool, u8) {
    let t = squash(&n.text());
    let len = t.chars().count();
    (
        t.chars().any(|c| c.is_ascii_digit()),
        match len {
            0..=3 => 0,
            4..=20 => 1,
            _ => 2,
        },
    )
}

struct RepeatColumn<'a> {
    key: String,
    kind: Kind,
    shape: (bool, u8),
    /// The column's field in the first row that has it.
    example: Node<'a>,
    text_of: Option<Node<'a>>,
    name: String,
}

/// Which of `columns` (in `order`) each of a row's fields lines up with:
/// the most fields matched to columns with the same key, in order, a
/// matching shape counting extra.
fn align(
    fields: &[Field<'_>],
    shapes: &[(bool, u8)],
    order: &[usize],
    columns: &[RepeatColumn<'_>],
) -> Vec<Option<usize>> {
    let (m, n) = (fields.len(), order.len());
    let score = |i: usize, j: usize| -> Option<u32> {
        let c = &columns[order[j]];
        (fields[i].key == c.key).then(|| 2 + u32::from(shapes[i] == c.shape))
    };
    // best[i][j]: the best score for fields[i..] against order[j..].
    let mut best = vec![vec![0u32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            let mut b = best[i + 1][j].max(best[i][j + 1]);
            if let Some(s) = score(i, j) {
                b = b.max(s + best[i + 1][j + 1]);
            }
            best[i][j] = b;
        }
    }
    let mut out = vec![None; m];
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if let Some(s) = score(i, j)
            && best[i][j] == s + best[i + 1][j + 1]
        {
            out[i] = Some(order[j]);
            i += 1;
            j += 1;
        } else if best[i][j] == best[i + 1][j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn read_repeat<'a>(row_selector: String, rows: &[Node<'a>], base: Option<&Url>) -> Option<Records> {
    let rows = &rows[..rows.len().min(MAX_ROWS)];
    let mut columns: Vec<RepeatColumn<'a>> = Vec::new();
    // Column ids in the order they appear in rows.
    let mut order: Vec<usize> = Vec::new();
    let mut read: Vec<HashMap<usize, Field<'a>>> = Vec::new();
    let mut count: Vec<usize> = Vec::new();
    for &row in rows {
        let mut fields = fields(row).unwrap_or_default();
        fields.truncate(ROW_FIELDS);
        let shapes: Vec<(bool, u8)> = fields.iter().map(|f| shape(f.node)).collect();
        let matched = align(&fields, &shapes, &order, &columns);
        let mut values = HashMap::new();
        let mut after: Option<usize> = None;
        for ((f, m), &look) in fields.into_iter().zip(matched).zip(&shapes) {
            let id = match m {
                Some(id) => id,
                None if columns.len() < MAX_COLUMNS => {
                    columns.push(RepeatColumn {
                        key: f.key.clone(),
                        kind: f.kind,
                        shape: look,
                        example: f.node,
                        text_of: f.text_of,
                        name: String::new(),
                    });
                    count.push(0);
                    let id = columns.len() - 1;
                    let at =
                        after.map_or(0, |a| order.iter().position(|&o| o == a).unwrap_or(0) + 1);
                    order.insert(at, id);
                    id
                }
                None => continue,
            };
            after = Some(id);
            count[id] += 1;
            values.insert(id, f);
        }
        read.push(values);
    }
    let kept: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&id| enough(count[id], rows.len()))
        .collect();
    let texts: HashMap<usize, Vec<Option<String>>> = kept
        .iter()
        .map(|&id| {
            let t = read
                .iter()
                .map(|r| r.get(&id).map(|f| squash(&f.node.text())))
                .collect();
            (id, t)
        })
        .collect();
    // Names, first from what the field is.
    for &id in &kept {
        let c = &mut columns[id];
        let row = rows_of(c.example, rows);
        c.name = match c.kind {
            Kind::Image => named(c.example, row).unwrap_or_else(|| "image".into()),
            Kind::Url => String::new(),
            Kind::Text => named(c.example, row).unwrap_or_else(|| {
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
    for (p, &id) in kept.iter().enumerate() {
        if columns[id].kind != Kind::Text {
            continue;
        }
        if let Some(label) = label(columns[id].example, &texts[&id], rows.len())
            && let Some(&next) = kept[p + 1..]
                .iter()
                .find(|&&j| columns[j].kind == Kind::Text)
            && label_of(&texts[&next], rows.len()).is_none()
        {
            columns[next].name = snake(&label);
            drop.insert(id);
            continue;
        }
        if let Some(prefix) = label_of(&texts[&id], rows.len()) {
            columns[id].name = snake(&prefix);
            strip.insert(id, prefix);
        }
    }
    // Links are named after their text.
    for &id in &kept {
        if columns[id].kind == Kind::Url {
            let text = columns[id].text_of.and_then(|t| {
                kept.iter()
                    .find(|&&j| {
                        columns[j].kind == Kind::Text
                            && columns[j].example == t
                            && !drop.contains(&j)
                    })
                    .map(|&j| columns[j].name.clone())
            });
            columns[id].name = match text {
                Some(t) => format!("{t}_url"),
                None => "link".into(),
            };
        }
    }
    let kept: Vec<usize> = kept.into_iter().filter(|id| !drop.contains(id)).collect();
    let content = kept
        .iter()
        .filter(|&&id| columns[id].kind != Kind::Url)
        .count();
    if content < 2 {
        return None;
    }
    let mut names: Vec<String> = kept.iter().map(|&id| columns[id].name.clone()).collect();
    distinct(&mut names);
    let out_rows = read
        .iter()
        .map(|r| {
            kept.iter()
                .zip(&names)
                .map(|(&id, name)| {
                    let c = &columns[id];
                    let v = match r.get(&id) {
                        None => Value::Null,
                        Some(f) => match c.kind {
                            Kind::Text => {
                                let t = squash(&f.node.text());
                                let t = match strip.get(&id) {
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
    let columns = kept
        .iter()
        .zip(names)
        .map(|(&id, name)| {
            let c = &columns[id];
            let row = rows_of(c.example, rows).unwrap_or(rows[0]);
            let mut selector = relative(row, c.example);
            match c.kind {
                Kind::Url => selector.push_str("::attr(href)"),
                Kind::Image => {
                    let attr = image_attr(read.iter().filter_map(|r| r.get(&id).map(|f| f.node)));
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
/// below the row; the row's class is taken off the front when a separator
/// follows it (`hit__title` in `hit` is `title`, `poster` in `post` stays).
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
                        .filter(|s| s.starts_with(['-', '_']))
                        .map(|s| s.trim_start_matches(['-', '_']))
                })
                .filter(|s| !s.is_empty())
                .unwrap_or(class);
            Some(snake(bare)).filter(|s| !s.is_empty())
        })
}

/// The text of a label column: the same short text in most rows, written
/// as a label (`dt`, `th`, `label`, a `label` or `key` class that isn't a
/// badge's `label label-success`, or ending in `:`).
fn label(example: Node<'_>, texts: &[Option<String>], rows: usize) -> Option<String> {
    let (text, n) = most_common(texts.iter().flatten().map(String::as_str))?;
    if rows < MIN_ROWS || (n as f64) < 0.8 * rows as f64 || text.chars().count() > 40 {
        return None;
    }
    let names: Vec<&str> = example
        .attr("class")
        .unwrap_or_default()
        .split_ascii_whitespace()
        .collect();
    let classed = names.iter().any(|c| matches!(*c, "label" | "key"))
        && !names.iter().any(|c| c.starts_with("label-"));
    let looks =
        matches!(example.tag(), Some("dt" | "th" | "label")) || text.ends_with(':') || classed;
    looks.then(|| text.trim_end_matches(':').trim().to_string())
}

/// The label most values start with (`Area:` in `Area: 120 m²`), when
/// most rows' values do. A label starts with a letter, so times (`10:30`)
/// aren't labels, and URLs (`https://`) aren't either.
fn label_of(texts: &[Option<String>], rows: usize) -> Option<String> {
    let labels = texts.iter().flatten().filter_map(|t| {
        let (l, rest) = t.split_once(':')?;
        let l = l.trim();
        (l.chars().next().is_some_and(char::is_alphabetic)
            && l.chars().count() <= 30
            && !rest.trim().is_empty()
            && !rest.starts_with('/'))
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

/// The most frequent item, the first seen of equals.
fn most_common<T: Eq + std::hash::Hash + Copy>(
    items: impl Iterator<Item = T>,
) -> Option<(T, usize)> {
    let mut counts: HashMap<T, usize> = HashMap::new();
    let mut order = Vec::new();
    for i in items {
        let c = counts.entry(i).or_default();
        if *c == 0 {
            order.push(i);
        }
        *c += 1;
    }
    let mut best: Option<(T, usize)> = None;
    for i in order {
        let n = counts[&i];
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((i, n));
        }
    }
    best
}

/// A selector for `n` that finds it, first among matches, inside `row`:
/// the shortest ending of its path from the row that does, trying the
/// last four steps; else the whole path.
fn relative(row: Node<'_>, n: Node<'_>) -> String {
    let chain: Vec<Node<'_>> = std::iter::successors(Some(n), |p| p.parent())
        .take_while(|p| *p != row)
        .collect();
    let segments: Vec<String> = chain.iter().rev().map(|p| segment(*p)).collect();
    for len in 1..=segments.len().min(4) {
        let path = segments[segments.len() - len..].join(" > ");
        if Query::cached(&path).is_ok_and(|q| q.select(row).first() == Some(&n)) {
            return path;
        }
    }
    segments.join(" > ")
}
