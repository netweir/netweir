//! Tracked selectors: what an element looked like, and finding it again
//! after a redesign broke the selector that used to find it.
//!
//! A fingerprint keeps what tends to survive a redesign: the element's tag
//! and text, its attributes, the tags above and beside it, its parent, and
//! the nearest text before it (a label such as `<dt>UPC</dt>`, or the title
//! above a price). When the selector stops matching, every element of the
//! same kind is scored against the fingerprint and the best one is taken if
//! it scores at least the threshold. The weights are tuned against the
//! redesign pairs in `tests/fixtures/redesigns`.

use serde::{Deserialize, Serialize};

use crate::document::{Node, NodeKind};

/// The score a candidate needs, by default, to count as the same element.
pub const THRESHOLD: f64 = 0.75;

/// How far below the threshold a candidate may score when nothing else on
/// the page has the shape of the text this element is known to hold.
const LONE_SHAPE_ALLOWANCE: f64 = 0.15;

/// Scores closer than this say nothing about which element is the one.
const TIE: f64 = 0.02;

/// How much each kind of evidence counts; they add up to 1.
const WEIGHTS: Weights = Weights {
    text: 0.45,
    attrs: 0.15,
    preceding: 0.15,
    path: 0.10,
    parent: 0.10,
    siblings: 0.05,
};

struct Weights {
    text: f64,
    attrs: f64,
    preceding: f64,
    path: f64,
    parent: f64,
    siblings: f64,
}

/// Longer texts are compared by their start; it's enough to recognise them
/// and keeps scoring cheap on big pages.
const TEXT_KEPT: usize = 120;

/// Sibling tags kept in a fingerprint.
const MAX_SIBLINGS: usize = 32;

/// What an element looked like when its selector last matched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    tag: String,
    text: String,
    class: String,
    id: Option<String>,
    attr_names: Vec<String>,
    /// Tags from the root down to the parent.
    path: Vec<String>,
    /// The parent's other element children's tags.
    siblings: Vec<String>,
    parent_tag: Option<String>,
    parent_class: String,
    /// The nearest text before the element, outside it.
    preceding: String,
    /// Whether the text, and the text before it, have differed between
    /// pages where the selector matched (a product's name and price
    /// differ from product to product). Differences there are then no
    /// evidence against a candidate.
    #[serde(default)]
    text_varies: bool,
    #[serde(default)]
    preceding_varies: bool,
}

fn clip(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.chars().take(TEXT_KEPT).collect()
}

impl Fingerprint {
    pub fn of(node: Node<'_>) -> Fingerprint {
        let attrs = node.attrs();
        let attr = |name: &str| {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        };
        let mut attr_names: Vec<String> =
            attrs.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        attr_names.sort();
        let mut path = Vec::new();
        let mut up = node.parent();
        while let Some(p) = up {
            if let Some(tag) = p.tag() {
                path.push(tag.to_string());
            }
            up = p.parent();
        }
        path.reverse();
        let parent = node.parent().filter(|p| p.kind() == NodeKind::Element);
        // A parent with thousands of children would make every fingerprint
        // cost thousands of steps; the first few siblings say enough.
        let siblings = parent
            .map(|p| {
                p.children()
                    .filter(|c| c.kind() == NodeKind::Element && *c != node)
                    .filter_map(|c| c.tag().map(str::to_string))
                    .take(MAX_SIBLINGS)
                    .collect()
            })
            .unwrap_or_default();
        Fingerprint {
            tag: node.tag().unwrap_or_default().to_string(),
            text: clip(&text_start(node)),
            class: attr("class").unwrap_or_default(),
            id: attr("id"),
            attr_names,
            path,
            siblings,
            parent_tag: parent.and_then(|p| p.tag()).map(str::to_string),
            parent_class: parent
                .and_then(|p| p.attr("class"))
                .unwrap_or_default()
                .to_string(),
            preceding: preceding_text(node),
            text_varies: false,
            preceding_varies: false,
        }
    }

    /// Carries over what the previous fingerprint of the same element
    /// learned, and learns from the difference between the two.
    pub fn learn_from(&mut self, previous: &Fingerprint) {
        self.text_varies = previous.text_varies || previous.text != self.text;
        self.preceding_varies = previous.preceding_varies || previous.preceding != self.preceding;
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a fingerprint is plain data")
    }

    pub fn from_json(s: &str) -> Option<Fingerprint> {
        serde_json::from_str(s).ok()
    }

    /// How much `node` looks like the element this was taken from, 0 to 1.
    pub fn score(&self, node: Node<'_>) -> f64 {
        let other = Fingerprint::of(node);
        let w = &WEIGHTS;
        let attrs = 0.5 * trigram_similarity(&self.class, &other.class)
            + 0.25
                * match (&self.id, &other.id) {
                    (None, None) => 1.0,
                    (Some(a), Some(b)) if a == b => 1.0,
                    (Some(a), Some(b)) => ratio(a, b),
                    _ => 0.5,
                }
            + 0.25 * jaccard(&self.attr_names, &other.attr_names);
        let parent = 0.5 * f64::from(u8::from(self.parent_tag == other.parent_tag))
            + 0.5 * one_sided(&self.parent_class, &other.parent_class, trigram_similarity);
        // A label (the text before the element) that never changed and now
        // reads differently means a different field, however alike the
        // two look: the Shipping cell is not the Total cell.
        if !self.preceding_varies
            && !self.preceding.is_empty()
            && !other.preceding.is_empty()
            && ratio(&self.preceding, &other.preceding) < 0.5
        {
            return 0.0;
        }
        let text = if self.text_varies {
            ratio(&shape(&self.text), &shape(&other.text))
        } else {
            text_similarity(&self.text, &other.text)
        };
        let preceding = if self.preceding_varies {
            0.5
        } else {
            one_sided(&self.preceding, &other.preceding, ratio)
        };
        let score = w.text * text
            + w.attrs * attrs
            + w.preceding * preceding
            + w.path * lcs_ratio(&self.path, &other.path)
            + w.parent * parent
            + w.siblings * jaccard(&self.siblings, &other.siblings);
        // A different tag of the same kind (h1 to h2) costs a little.
        if self.tag == other.tag {
            score
        } else {
            score * 0.95
        }
    }
}

/// `measure`, except that something on one side only counts as no evidence
/// either way: an element moved to the start of its block has lost its
/// preceding text, not gained a different one.
fn one_sided(a: &str, b: &str, measure: fn(&str, &str) -> f64) -> f64 {
    if a.is_empty() != b.is_empty() {
        0.5
    } else {
        measure(a, b)
    }
}

/// The nearest non-blank text before `node` in document order, outside it.
fn preceding_text(node: Node<'_>) -> String {
    let mut at = node.prev_in_order();
    for _ in 0..200 {
        let Some(n) = at else { break };
        if n.kind() == NodeKind::Text
            && let Some(t) = n.data()
            && !t.trim().is_empty()
        {
            return clip(t);
        }
        at = n.prev_in_order();
    }
    String::new()
}

/// Tags that a redesign may swap for one another.
fn family(tag: &str) -> &str {
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "h",
        "b" | "strong" => "b",
        "i" | "em" => "i",
        "p" | "span" | "div" | "td" | "dd" | "li" => "text",
        other => other,
    }
}

/// The element below `root` most like `fp`, with its score.
pub fn best<'a>(root: Node<'a>, fp: &Fingerprint) -> Option<(Node<'a>, f64)> {
    let kind = family(&fp.tag);
    root.descendants()
        .filter(|n| n.kind() == NodeKind::Element && n.tag().is_some_and(|t| family(t) == kind))
        .map(|n| (n, fp.score(n)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// `best`, if it scores at least `threshold` and the element isn't one of
/// a list that's still there (see `still_repeated`).
pub fn relocate<'a>(root: Node<'a>, fp: &Fingerprint, threshold: f64) -> Option<(Node<'a>, f64)> {
    if still_repeated(root, fp) {
        return None;
    }
    best(root, fp).filter(|(node, score)| accepted(root, fp, *node, *score, threshold))
}

/// Whether the best candidate, `node`, is good enough to count as the
/// element: it scores at least `threshold`, or nearly that and nothing else
/// has its shape, and no other element with its shape scores about the same.
pub fn accepted(
    root: Node<'_>,
    fp: &Fingerprint,
    node: Node<'_>,
    score: f64,
    threshold: f64,
) -> bool {
    if score >= threshold {
        !tied(root, fp, node, score)
    } else {
        score >= threshold - LONE_SHAPE_ALLOWANCE && lone_shape(root, fp, node)
    }
}

/// Whether another element with the text's shape scores within a hair of
/// `node`: two prices that look equally right, and nothing to choose by.
fn tied(root: Node<'_>, fp: &Fingerprint, node: Node<'_>, score: f64) -> bool {
    if !fp.text_varies || !fp.text.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let wanted = number_shape(&fp.text);
    root.descendants().any(|n| {
        n != node
            && n.kind() == NodeKind::Element
            && !n.children().any(|c| c.kind() == NodeKind::Element)
            && number_shape(&clip(&text_start(n))) == wanted
            && (fp.score(n) - score).abs() < TIE
    })
}

/// Whether `node` is the one element below `root` whose own text has the
/// shape this element's text has had on every page: a rebuilt page moves a
/// price's surroundings, and its being the only price left is evidence for
/// it. Only text known to vary counts, and only with a digit in it, since
/// other text has no shape to speak of.
fn lone_shape(root: Node<'_>, fp: &Fingerprint, node: Node<'_>) -> bool {
    if !fp.text_varies || !fp.text.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let wanted = number_shape(&fp.text);
    let mut alike = root.descendants().filter(|n| {
        n.kind() == NodeKind::Element
            && !n.children().any(|c| c.kind() == NodeKind::Element)
            && number_shape(&clip(&text_start(*n))) == wanted
    });
    alike.next() == Some(node) && alike.next().is_none()
}

/// The text with every run of digits a single 0, so $9.99 and $1299.99 are
/// the same kind of thing.
fn number_shape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if !c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('0') {
            out.push('0');
        }
    }
    out
}

/// The text with every digit a 0: what a price or a date looks like.
fn shape(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect()
}

/// Text similarity that also recognises the same shape with different
/// numbers: a price that changed is still a price.
fn text_similarity(a: &str, b: &str) -> f64 {
    ratio(a, b).max(0.85 * ratio(&shape(a), &shape(b)))
}

/// The start of the text below `node`: enough to compare, without walking
/// a subtree that may be the whole page.
fn text_start(node: Node<'_>) -> String {
    let mut out = String::new();
    for n in node.descendants() {
        if n.kind() == NodeKind::Text {
            out.push_str(n.data().unwrap_or_default());
            if out.len() > TEXT_KEPT * 4 {
                break;
            }
        }
    }
    out
}

/// Whether several elements still carry all of the remembered element's
/// classes: a list, where a selector that misses means this item has no
/// such element (a sold-out product's price), not that the site changed.
pub fn still_repeated(root: Node<'_>, fp: &Fingerprint) -> bool {
    let wanted: Vec<&str> = fp.class.split_ascii_whitespace().collect();
    if wanted.is_empty() {
        return false;
    }
    let kind = family(&fp.tag);
    root.descendants()
        .filter(|n| n.kind() == NodeKind::Element && n.tag().is_some_and(|t| family(t) == kind))
        .filter(|n| {
            let class = n.attr("class").unwrap_or_default();
            wanted
                .iter()
                .all(|w| class.split_ascii_whitespace().any(|c| c == *w))
        })
        .take(2)
        .count()
        >= 2
}

/// 1 minus the edit distance over the longer length; 1 for two blanks.
fn ratio(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(ca != cb));
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    1.0 - row[b.len()] as f64 / longest as f64
}

/// Similarity of two class strings by shared three-letter pieces, so a
/// renamed class that keeps a word (`price` in `ProductPrice`) still counts
/// for something.
fn trigram_similarity(a: &str, b: &str) -> f64 {
    let grams = |s: &str| -> std::collections::HashSet<String> {
        let s: Vec<char> = s
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        s.windows(3).map(|w| w.iter().collect()).collect()
    };
    let (x, y) = (grams(a), grams(b));
    if x.is_empty() && y.is_empty() {
        return 1.0;
    }
    x.intersection(&y).count() as f64 / x.union(&y).count() as f64
}

/// Shared items over all items, counting repeats; 1 for two empty lists.
fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut rest = b.to_vec();
    let mut shared = 0;
    for x in a {
        if let Some(i) = rest.iter().position(|y| y == x) {
            rest.swap_remove(i);
            shared += 1;
        }
    }
    shared as f64 / (a.len() + b.len() - shared) as f64
}

/// Longest common subsequence over the average length.
fn lcs_ratio(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in 0..a.len() {
        for j in 0..b.len() {
            table[i + 1][j + 1] = if a[i] == b[j] {
                table[i][j] + 1
            } else {
                table[i][j + 1].max(table[i + 1][j])
            };
        }
    }
    2.0 * table[a.len()][b.len()] as f64 / (a.len() + b.len()) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures() {
        assert_eq!(ratio("", ""), 1.0);
        assert_eq!(ratio("abc", "abc"), 1.0);
        assert!((ratio("kitten", "sitting") - (1.0 - 3.0 / 7.0)).abs() < 1e-9);
        assert!(text_similarity("£51.77", "£53.74") >= 0.85);
        assert!(trigram_similarity("price_color", "ProductPrice-sc") > 0.1);
        assert_eq!(jaccard(&["a".into(), "a".into()], &["a".into()]), 0.5);
        assert_eq!(
            lcs_ratio(
                &["a".into(), "b".into()],
                &["a".into(), "x".into(), "b".into()]
            ),
            0.8
        );
    }
}
