//! Evaluating an XPath expression tree against a document.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

use super::parser::{Axis, Expr, NodeTest, Op, Step};
use crate::document::{Node, NodeKind};
use crate::query::Hit;

/// A value a caller can bind to a `$variable`.
#[derive(Debug, Clone, PartialEq)]
pub enum XValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

/// A node in XPath's data model: a tree node, or an attribute (which the
/// tree keeps on its element rather than as a node of its own).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Item<'a> {
    Node(Node<'a>),
    Attr {
        owner: Node<'a>,
        index: u32,
        name: &'a str,
        value: &'a str,
    },
}

impl<'a> Item<'a> {
    /// Position in document order. An element's attributes come after the
    /// element and before its children, whose order is higher.
    fn key(&self) -> (u32, u32) {
        match self {
            Item::Node(n) => (n.order(), 0),
            Item::Attr { owner, index, .. } => (owner.order(), index + 1),
        }
    }

    fn string_value(&self) -> Cow<'a, str> {
        match self {
            Item::Node(n) => match n.kind() {
                NodeKind::Text | NodeKind::Comment => Cow::Borrowed(n.data().unwrap_or_default()),
                _ => Cow::Owned(n.text()),
            },
            Item::Attr { value, .. } => Cow::Borrowed(value),
        }
    }

    fn name(&self) -> &'a str {
        match self {
            Item::Node(n) => n.tag().unwrap_or(""),
            Item::Attr { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone)]
enum Value<'a> {
    Nodes(Vec<Item<'a>>),
    Str(Cow<'a, str>),
    Num(f64),
    Bool(bool),
}

fn text<'a>(s: String) -> Value<'a> {
    Value::Str(Cow::Owned(s))
}

struct Context<'a, 'v> {
    item: Item<'a>,
    position: usize,
    size: usize,
    root: Node<'a>,
    vars: &'v [(String, XValue)],
    kept: &'v RefCell<Kept>,
}

/// Values of a run's [`Expr::Memo`] expressions, by slot and node key. Only
/// a number or a yes or no is ever needed from them.
type Kept = HashMap<(usize, (u32, u32)), Scalar>;

#[derive(Debug, Clone, Copy)]
enum Scalar {
    Num(f64),
    Bool(bool),
}

type Result<T> = std::result::Result<T, String>;

/// Evaluates `expr` with `scope` as the context node.
pub(super) fn run<'a>(
    expr: &Expr,
    scope: Node<'a>,
    vars: &[(String, XValue)],
) -> Result<Vec<Hit<'a>>> {
    let root = scope.doc.root();
    let kept = RefCell::default();
    let ctx = Context {
        item: Item::Node(scope),
        position: 1,
        size: 1,
        root,
        vars,
        kept: &kept,
    };
    Ok(into_hits(eval(expr, &ctx)?))
}

fn into_hits(value: Value<'_>) -> Vec<Hit<'_>> {
    match value {
        Value::Nodes(items) => items
            .into_iter()
            .map(|i| match i {
                Item::Node(n) => Hit::Node(n),
                Item::Attr { value, .. } => Hit::Value(value.to_string()),
            })
            .collect(),
        Value::Str(s) => vec![Hit::Value(s.into_owned())],
        Value::Num(n) => vec![Hit::Value(python_float(n))],
        // parsel gives "1" and "0" for boolean results.
        Value::Bool(b) => vec![Hit::Value(if b { "1" } else { "0" }.to_string())],
    }
}

/// A number as Python prints a float, which is what Scrapy users see from
/// lxml: `3.0`, `0.5`, `nan`, `inf`, `1e+16`.
fn python_float(n: f64) -> String {
    if n.is_nan() {
        return "nan".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "inf" } else { "-inf" }.into();
    }
    if n != 0.0 && (n.abs() >= 1e16 || n.abs() < 1e-4) {
        // Python writes the exponent signed and at least two digits long.
        let s = format!("{n:e}");
        let (mantissa, exponent) = s.split_once('e').unwrap_or((&s, "0"));
        let (sign, digits) = exponent
            .strip_prefix('-')
            .map_or(("+", exponent), |d| ("-", d));
        return format!("{mantissa}e{sign}{digits:0>2}");
    }
    if n.fract() == 0.0 {
        format!("{n:.1}")
    } else {
        format!("{n}")
    }
}

/// Stack kept free before recursing, and the size of each extra segment.
/// The parser caps nesting, but debug builds use large frames and callers
/// may run on small thread stacks.
const RED_ZONE: usize = 64 * 1024;
const GROW: usize = 1024 * 1024;

fn eval<'a>(expr: &Expr, ctx: &Context<'a, '_>) -> Result<Value<'a>> {
    stacker::maybe_grow(RED_ZONE, GROW, || eval_here(expr, ctx))
}

fn eval_here<'a>(expr: &Expr, ctx: &Context<'a, '_>) -> Result<Value<'a>> {
    Ok(match expr {
        Expr::Literal(s) => text(s.clone()),
        Expr::Number(n) => Value::Num(*n),
        Expr::Variable(name) => match ctx.vars.iter().find(|(k, _)| k == name) {
            Some((_, XValue::Str(s))) => text(s.clone()),
            Some((_, XValue::Num(n))) => Value::Num(*n),
            Some((_, XValue::Bool(b))) => Value::Bool(*b),
            None => return Err(format!("no value given for ${name}")),
        },
        Expr::Negate(e) => Value::Num(-number(eval(e, ctx)?)),
        Expr::Binary(op, l, r) => binary(*op, l, r, ctx)?,
        Expr::Union(l, r) => {
            let (Value::Nodes(mut a), Value::Nodes(b)) = (eval(l, ctx)?, eval(r, ctx)?) else {
                return Err("both sides of | must be node-sets".into());
            };
            a.extend(b);
            Value::Nodes(in_document_order(a))
        }
        Expr::Call(name, args) => call(name, args, ctx)?,
        Expr::Memo(e, slot) => {
            let key = (*slot, ctx.item.key());
            // Not borrowed while evaluating: slots inside `e` fill too.
            let known = ctx.kept.borrow().get(&key).copied();
            let scalar = match known {
                Some(s) => s,
                None => {
                    let s = match eval(e, ctx)? {
                        Value::Num(n) => Scalar::Num(n),
                        other => Scalar::Bool(boolean(&other)),
                    };
                    ctx.kept.borrow_mut().insert(key, s);
                    s
                }
            };
            match scalar {
                Scalar::Num(n) => Value::Num(n),
                Scalar::Bool(b) => Value::Bool(b),
            }
        }
        Expr::Path { absolute, steps } => {
            let start = if *absolute {
                Item::Node(ctx.root)
            } else {
                ctx.item
            };
            Value::Nodes(walk(vec![start], steps, ctx)?)
        }
        Expr::Filter {
            primary,
            predicates,
            steps,
        } => {
            let Value::Nodes(mut items) = eval(primary, ctx)? else {
                return Err("predicates and paths can only follow a node-set".into());
            };
            for p in predicates {
                items = filter(items, p, ctx)?;
            }
            Value::Nodes(walk(items, steps, ctx)?)
        }
    })
}

fn in_document_order(items: Vec<Item<'_>>) -> Vec<Item<'_>> {
    // A key costs a call into lexbor, so each is read once.
    let mut keyed: Vec<_> = items.into_iter().map(|i| (i.key(), i)).collect();
    keyed.sort_unstable_by_key(|(k, _)| *k);
    keyed.dedup_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, i)| i).collect()
}

fn walk<'a>(
    mut items: Vec<Item<'a>>,
    steps: &[Step],
    ctx: &Context<'a, '_>,
) -> Result<Vec<Item<'a>>> {
    let mut candidates = Vec::new();
    let mut scanned = Vec::new();
    let mut i = 0;
    while i < steps.len() {
        let step = &steps[i];
        i += 1;
        // "//x[pred]" is descendant-or-self::node()/child::x[pred]: run it
        // as one descendant scan for x, with positions counted among each
        // match's siblings, instead of a child scan from every node.
        if let Some(next) = steps.get(i)
            && step.axis == Axis::DescendantOrSelf
            && step.test == NodeTest::Node
            && step.predicates.is_empty()
            && next.axis == Axis::Child
            && !next.predicates.is_empty()
            && let Some(grouped) = descendants_by_parent(&items, next, ctx)
        {
            items = grouped?;
            i += 1;
            continue;
        }
        let test = Test::resolve(&step.test, step.axis, ctx.root);
        // A leading `[k]` with k a whole number from 1 up.
        let nth = match step.predicates.first() {
            Some(Expr::Number(k)) if *k >= 1.0 && k.fract() == 0.0 && *k < usize::MAX as f64 => {
                Some(*k as usize)
            }
            _ => None,
        };
        let mut next = Vec::new();
        for item in &items {
            candidates.clear();
            // Predicates already applied while collecting candidates.
            let mut done = 0;
            match (item, test.indexable()) {
                // Child and descendant steps on names, *, text() and node()
                // scan the document index instead of walking lexbor's nodes.
                (Item::Node(n), true)
                    if matches!(
                        step.axis,
                        Axis::Child | Axis::Descendant | Axis::DescendantOrSelf
                    ) =>
                {
                    let index = n.doc.index();
                    let at = n.order();
                    let keep = |kind, tag| test.keeps(kind, tag);
                    scanned.clear();
                    match step.axis {
                        Axis::Child => index.children(n.doc, at, keep, &mut scanned),
                        axis => index.descendants(
                            n.doc,
                            at,
                            axis == Axis::DescendantOrSelf,
                            keep,
                            &mut scanned,
                        ),
                    }
                    candidates.extend(scanned.iter().copied().map(Item::Node));
                }
                _ => {
                    let mut on_axis = axis(*item, step.axis).filter(|c| test.matches(c));
                    if nth.is_some() {
                        done = 1;
                    }
                    match nth {
                        // `[k]` first: stop at the kth node instead of
                        // collecting the whole axis (following-sibling::dd[1]).
                        Some(k) => candidates.extend(on_axis.nth(k - 1)),
                        None => candidates.extend(on_axis),
                    }
                }
            }
            let mut kept = std::mem::take(&mut candidates);
            for p in &step.predicates[done..] {
                kept = filter(kept, p, ctx)?;
            }
            next.extend_from_slice(&kept);
            candidates = kept;
        }
        // One context node's results are already in order; several need
        // merging. Skipping the sort for one keeps `//x` a single scan.
        items = if items.len() == 1 && !step.axis.is_reverse() {
            next
        } else {
            in_document_order(next)
        };
    }
    Ok(items)
}

/// `//x[pred]` from each item in `items`: every descendant passing `step`'s
/// test, grouped by parent so predicate positions count siblings, in
/// document order. None when the test can't be read from the index.
fn descendants_by_parent<'a>(
    items: &[Item<'a>],
    step: &Step,
    ctx: &Context<'a, '_>,
) -> Option<Result<Vec<Item<'a>>>> {
    let test = Test::resolve(&step.test, step.axis, ctx.root);
    if !test.indexable() {
        return None;
    }
    let mut found = Vec::new();
    // Items come in document order. One inside an earlier item's subtree
    // was scanned with it, and scanning it again would count its
    // descendants twice.
    let mut covered_to = 0;
    for item in items {
        // An attribute has no children.
        let Item::Node(n) = item else { continue };
        let index = n.doc.index();
        let at = n.order();
        if at < covered_to {
            continue;
        }
        covered_to = index.end_of(at);
        index.descendants(n.doc, at, false, |k, t| test.keeps(k, t), &mut found);
    }
    // Group by parent, keeping each group in document order.
    let mut groups: Vec<(u32, Vec<Item<'a>>)> = Vec::new();
    let mut slot: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for node in found {
        let parent = node.doc.index().parent_of(node.order());
        let at = *slot.entry(parent).or_insert_with(|| {
            groups.push((parent, Vec::new()));
            groups.len() - 1
        });
        groups[at].1.push(Item::Node(node));
    }
    let run = || -> Result<Vec<Item<'a>>> {
        let mut out = Vec::new();
        for (_, mut group) in groups {
            for p in &step.predicates {
                group = filter(group, p, ctx)?;
            }
            out.extend(group);
        }
        Ok(in_document_order(out))
    };
    Some(run())
}

/// `[@name = 'value']`, `['value' = @name]` and `[@name]`: answered from
/// the element directly, without building its attribute list. None for any
/// other predicate.
fn quick_predicate(pred: &Expr, item: &Item<'_>) -> Option<bool> {
    let attr_name = |e: &Expr| match e {
        Expr::Path {
            absolute: false,
            steps,
        } => match steps.as_slice() {
            [
                Step {
                    axis: Axis::Attribute,
                    test: NodeTest::Name(n),
                    predicates,
                },
                // lexbor folds case when looking an attribute up by name, but
                // XPath names are case-sensitive and HTML attribute names are
                // stored lowercase: leave capitals to the general path.
            ] if predicates.is_empty() && !n.bytes().any(|b| b.is_ascii_uppercase()) => {
                Some(n.clone())
            }
            _ => None,
        },
        _ => None,
    };
    let Item::Node(node) = item else { return None };
    match pred {
        Expr::Binary(Op::Eq, l, r) => {
            let (name, value) = match (attr_name(l), attr_name(r), &**l, &**r) {
                (Some(n), None, _, Expr::Literal(v)) | (None, Some(n), Expr::Literal(v), _) => {
                    (n, v)
                }
                _ => return None,
            };
            Some(node.attr(&name) == Some(value.as_str()))
        }
        other => attr_name(other).map(|n| node.attr(&n).is_some()),
    }
}

/// Keeps the items for which `pred` holds, numbering them in the order
/// given (axis order inside a step, document order in a filter).
fn filter<'a>(items: Vec<Item<'a>>, pred: &Expr, ctx: &Context<'a, '_>) -> Result<Vec<Item<'a>>> {
    let size = items.len();
    let mut kept = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        if let Some(keep) = quick_predicate(pred, &item) {
            if keep {
                kept.push(item);
            }
            continue;
        }
        let inner = Context {
            item,
            position: i + 1,
            size,
            root: ctx.root,
            vars: ctx.vars,
            kept: ctx.kept,
        };
        let keep = match eval(pred, &inner)? {
            Value::Num(n) => n == (i + 1) as f64,
            other => boolean(&other),
        };
        if keep {
            kept.push(item);
        }
    }
    Ok(kept)
}

/// A node test made ready for one document: names become lexbor tag ids,
/// so matching an element is one integer comparison.
enum Test<'t> {
    Node,
    Text,
    Comment,
    Never,
    /// `*` on the axis's principal node type.
    AnyElement,
    AnyAttribute,
    ElementId(usize),
    /// A name with capitals: no HTML element has one (lexbor lowercases
    /// them), but SVG and MathML elements can, so compare the text.
    ElementName(&'t str),
    AttributeName(&'t str),
}

impl<'t> Test<'t> {
    fn resolve(test: &'t NodeTest, axis: Axis, root: Node<'_>) -> Test<'t> {
        let attr = axis == Axis::Attribute;
        match test {
            NodeTest::Node => Test::Node,
            NodeTest::Text if attr => Test::Never,
            NodeTest::Text => Test::Text,
            NodeTest::Comment if attr => Test::Never,
            NodeTest::Comment => Test::Comment,
            // HTML5 parsing turns processing instructions into comments.
            NodeTest::ProcessingInstruction(_) => Test::Never,
            NodeTest::Any if attr => Test::AnyAttribute,
            NodeTest::Any => Test::AnyElement,
            NodeTest::Name(name) if attr => Test::AttributeName(name),
            NodeTest::Name(name) if name.bytes().any(|b| b.is_ascii_uppercase()) => {
                Test::ElementName(name)
            }
            NodeTest::Name(name) => root.tag_id_named(name).map_or(Test::Never, Test::ElementId),
        }
    }

    /// Whether `keeps` can decide this test from the index alone.
    fn indexable(&self) -> bool {
        matches!(
            self,
            Test::Node
                | Test::Text
                | Test::Comment
                | Test::AnyElement
                | Test::ElementId(_)
                | Test::Never
        )
    }

    /// The test, from a node's lexbor type and tag id.
    fn keeps(&self, kind: u8, tag: usize) -> bool {
        const ELEMENT: u8 = 0x01;
        const TEXT: u8 = 0x03;
        const CDATA: u8 = 0x04;
        const COMMENT: u8 = 0x08;
        match self {
            Test::Node => true,
            Test::Text => kind == TEXT || kind == CDATA,
            Test::Comment => kind == COMMENT,
            Test::AnyElement => kind == ELEMENT,
            Test::ElementId(id) => kind == ELEMENT && tag == *id,
            _ => false,
        }
    }

    fn matches(&self, item: &Item<'_>) -> bool {
        match (self, item) {
            (Test::Node, _) => true,
            (Test::Never, _) => false,
            (Test::ElementId(id), Item::Node(n)) => {
                n.tag_id() == *id && n.kind() == NodeKind::Element
            }
            (Test::ElementName(name), Item::Node(n)) => n.tag() == Some(name),
            (Test::AnyElement, Item::Node(n)) => n.kind() == NodeKind::Element,
            (Test::Text, Item::Node(n)) => n.kind() == NodeKind::Text,
            (Test::Comment, Item::Node(n)) => n.kind() == NodeKind::Comment,
            (Test::AnyAttribute, Item::Attr { .. }) => true,
            (Test::AttributeName(name), Item::Attr { name: a, .. }) => a == name,
            _ => false,
        }
    }
}

fn attributes(n: Node<'_>) -> Vec<Item<'_>> {
    n.attrs()
        .into_iter()
        .enumerate()
        .map(|(i, (name, value))| Item::Attr {
            owner: n,
            index: i as u32,
            name,
            value,
        })
        .collect()
}

fn ancestors(n: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(n.parent(), |p| p.parent())
}

/// Everything after `n` in document order that isn't its descendant.
fn following(n: Node<'_>) -> Vec<Item<'_>> {
    let mut out = Vec::new();
    for a in std::iter::once(n).chain(ancestors(n)) {
        for s in std::iter::successors(a.next_sibling(), |s| s.next_sibling()) {
            out.push(Item::Node(s));
            out.extend(s.descendants().map(Item::Node));
        }
    }
    out
}

/// Everything before `n` that isn't its ancestor, nearest first.
fn preceding(n: Node<'_>) -> Vec<Item<'_>> {
    let mut out = Vec::new();
    for a in std::iter::once(n).chain(ancestors(n)) {
        for s in std::iter::successors(a.prev_sibling(), |s| s.prev_sibling()) {
            let mut below: Vec<Item<'_>> = s.descendants().map(Item::Node).collect();
            below.reverse();
            out.extend(below);
            out.push(Item::Node(s));
        }
    }
    out
}

/// The nodes on `axis` from `item`, in axis order (nearest first for
/// reverse axes), produced lazily.
fn axis<'a>(item: Item<'a>, axis: Axis) -> Box<dyn Iterator<Item = Item<'a>> + 'a> {
    match item {
        Item::Node(n) => match axis {
            Axis::Child => Box::new(n.children().map(Item::Node)),
            Axis::Descendant => Box::new(n.descendants().map(Item::Node)),
            Axis::DescendantOrSelf => {
                Box::new(std::iter::once(n).chain(n.descendants()).map(Item::Node))
            }
            Axis::Parent => Box::new(n.parent().map(Item::Node).into_iter()),
            Axis::Ancestor => Box::new(ancestors(n).map(Item::Node)),
            Axis::AncestorOrSelf => {
                Box::new(std::iter::once(n).chain(ancestors(n)).map(Item::Node))
            }
            Axis::FollowingSibling => Box::new(
                std::iter::successors(n.next_sibling(), |s| s.next_sibling()).map(Item::Node),
            ),
            Axis::PrecedingSibling => Box::new(
                std::iter::successors(n.prev_sibling(), |s| s.prev_sibling()).map(Item::Node),
            ),
            Axis::Following => Box::new(following(n).into_iter()),
            Axis::Preceding => Box::new(preceding(n).into_iter()),
            Axis::Attribute if n.kind() == NodeKind::Element => Box::new(attributes(n).into_iter()),
            Axis::Attribute | Axis::Namespace => Box::new(std::iter::empty()),
            Axis::Itself => Box::new(std::iter::once(item)),
        },
        Item::Attr { owner, .. } => match axis {
            Axis::Parent => Box::new(std::iter::once(Item::Node(owner))),
            Axis::Ancestor => Box::new(
                std::iter::once(owner)
                    .chain(ancestors(owner))
                    .map(Item::Node),
            ),
            Axis::AncestorOrSelf => Box::new(
                std::iter::once(item).chain(
                    std::iter::once(owner)
                        .chain(ancestors(owner))
                        .map(Item::Node),
                ),
            ),
            Axis::Itself | Axis::DescendantOrSelf => Box::new(std::iter::once(item)),
            Axis::Following => {
                Box::new(owner.descendants().map(Item::Node).chain(following(owner)))
            }
            Axis::Preceding => Box::new(preceding(owner).into_iter()),
            _ => Box::new(std::iter::empty()),
        },
    }
}

// --- conversions (section 4) ----------------------------------------------

fn string(v: &Value<'_>) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Num(n) => number_to_string(*n),
        Value::Bool(b) => b.to_string(),
        Value::Nodes(items) => items
            .first()
            .map(|i| i.string_value().into_owned())
            .unwrap_or_default(),
    }
}

fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if n == 0.0 {
        "0".into()
    } else {
        // Rust prints f64 without an exponent and without a trailing ".0",
        // as XPath requires.
        format!("{n}")
    }
}

fn number(v: Value<'_>) -> f64 {
    match v {
        Value::Num(n) => n,
        Value::Bool(b) => b as u8 as f64,
        Value::Str(s) => parse_number(&s),
        nodes @ Value::Nodes(_) => parse_number(&string(&nodes)),
    }
}

/// XPath's Number grammar: optional minus, digits with an optional
/// fraction, and XML whitespace around. Anything else is NaN.
fn parse_number(s: &str) -> f64 {
    let t = s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
    let digits = t.strip_prefix('-').unwrap_or(t);
    let valid = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1
        && digits != ".";
    if valid {
        t.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

fn boolean(v: &Value<'_>) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Num(n) => *n != 0.0 && !n.is_nan(),
        Value::Str(s) => !s.is_empty(),
        Value::Nodes(items) => !items.is_empty(),
    }
}

// --- operators (section 3.4, 3.5) -----------------------------------------

fn binary<'a>(op: Op, l: &Expr, r: &Expr, ctx: &Context<'a, '_>) -> Result<Value<'a>> {
    Ok(match op {
        Op::Or => Value::Bool(boolean(&eval(l, ctx)?) || boolean(&eval(r, ctx)?)),
        Op::And => Value::Bool(boolean(&eval(l, ctx)?) && boolean(&eval(r, ctx)?)),
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => {
            let (a, b) = (number(eval(l, ctx)?), number(eval(r, ctx)?));
            Value::Num(match op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                Op::Div => a / b,
                // Truncating remainder, which is what Rust's % does on f64.
                _ => a % b,
            })
        }
        _ => Value::Bool(compare(op, eval(l, ctx)?, eval(r, ctx)?)),
    })
}

fn compare_atoms(op: Op, a: &Value<'_>, b: &Value<'_>) -> bool {
    if matches!(op, Op::Eq | Op::Neq) {
        let equal = if matches!(a, Value::Bool(_)) || matches!(b, Value::Bool(_)) {
            boolean(a) == boolean(b)
        } else if matches!(a, Value::Num(_)) || matches!(b, Value::Num(_)) {
            number(a.clone()) == number(b.clone())
        } else {
            string(a) == string(b)
        };
        return equal == (op == Op::Eq);
    }
    let (x, y) = (number(a.clone()), number(b.clone()));
    match op {
        Op::Lt => x < y,
        Op::Le => x <= y,
        Op::Gt => x > y,
        _ => x >= y,
    }
}

/// Comparisons involving node-sets are true if any member makes them true.
fn compare(op: Op, a: Value<'_>, b: Value<'_>) -> bool {
    match (&a, &b) {
        (Value::Nodes(xs), Value::Nodes(ys)) => {
            let ys: Vec<Value<'_>> = ys.iter().map(|y| Value::Str(y.string_value())).collect();
            xs.iter().any(|x| {
                let x = Value::Str(x.string_value());
                ys.iter().any(|y| compare_atoms(op, &x, y))
            })
        }
        (Value::Nodes(_), Value::Bool(_)) | (Value::Bool(_), Value::Nodes(_)) => {
            compare_atoms(op, &Value::Bool(boolean(&a)), &Value::Bool(boolean(&b)))
        }
        (Value::Nodes(xs), other) => xs
            .iter()
            .any(|x| compare_atoms(op, &atom_like(x, other), other)),
        (other, Value::Nodes(ys)) => ys
            .iter()
            .any(|y| compare_atoms(op, other, &atom_like(y, other))),
        _ => compare_atoms(op, &a, &b),
    }
}

/// A node's value as the type it is compared with.
fn atom_like<'a>(item: &Item<'a>, other: &Value<'_>) -> Value<'a> {
    match other {
        Value::Num(_) => Value::Num(parse_number(&item.string_value())),
        _ => Value::Str(item.string_value()),
    }
}

// --- functions (section 4, plus extensions) -------------------------------

fn call<'a>(name: &str, args: &[Expr], ctx: &Context<'a, '_>) -> Result<Value<'a>> {
    let arity = |min: usize, max: usize| -> Result<()> {
        if args.len() < min || args.len() > max {
            let expected = if min == max {
                format!("{min}")
            } else if max == usize::MAX {
                format!("at least {min}")
            } else {
                format!("{min} to {max}")
            };
            return Err(format!(
                "{name}() takes {expected} arguments, got {}",
                args.len()
            ));
        }
        Ok(())
    };
    let arg = |i: usize| eval(&args[i], ctx);
    let str_arg = |i: usize| -> Result<String> { Ok(string(&eval(&args[i], ctx)?)) };
    let nodes_arg = |i: usize| -> Result<Vec<Item<'a>>> {
        match eval(&args[i], ctx)? {
            Value::Nodes(n) => Ok(n),
            _ => Err(format!("{name}() needs a node-set argument")),
        }
    };
    // The context node as a one-item node-set, for functions whose
    // argument defaults to it.
    let context_or = |i: usize| -> Result<Vec<Item<'a>>> {
        if args.len() > i {
            nodes_arg(i)
        } else {
            Ok(vec![ctx.item])
        }
    };
    Ok(match name {
        "last" => {
            arity(0, 0)?;
            Value::Num(ctx.size as f64)
        }
        "position" => {
            arity(0, 0)?;
            Value::Num(ctx.position as f64)
        }
        "count" => {
            arity(1, 1)?;
            Value::Num(nodes_arg(0)?.len() as f64)
        }
        "id" => {
            arity(1, 1)?;
            let wanted: Vec<String> = match arg(0)? {
                Value::Nodes(items) => items
                    .iter()
                    .map(|i| i.string_value().into_owned())
                    .collect(),
                other => vec![string(&other)],
            };
            let index = ctx.root.doc.index();
            let found = wanted
                .iter()
                .flat_map(|s| s.split_whitespace())
                .flat_map(|id| index.with_id(ctx.root.doc, id))
                .filter(|n| {
                    n.order() >= ctx.root.order() && n.order() < index.end_of(ctx.root.order())
                })
                .map(Item::Node)
                .collect();
            Value::Nodes(in_document_order(found))
        }
        "local-name" | "name" => {
            arity(0, 1)?;
            text(
                in_document_order(context_or(0)?)
                    .first()
                    .map(|i| i.name().to_string())
                    .unwrap_or_default(),
            )
        }
        "namespace-uri" => {
            arity(0, 1)?;
            context_or(0)?;
            text(String::new())
        }
        "string" => {
            arity(0, 1)?;
            text(if args.is_empty() {
                ctx.item.string_value().into_owned()
            } else {
                str_arg(0)?
            })
        }
        "concat" => {
            arity(2, usize::MAX)?;
            let mut out = String::new();
            for i in 0..args.len() {
                out.push_str(&str_arg(i)?);
            }
            text(out)
        }
        "starts-with" => {
            arity(2, 2)?;
            Value::Bool(str_arg(0)?.starts_with(&str_arg(1)?))
        }
        "contains" => {
            arity(2, 2)?;
            Value::Bool(str_arg(0)?.contains(&str_arg(1)?))
        }
        "substring-before" => {
            arity(2, 2)?;
            let (s, t) = (str_arg(0)?, str_arg(1)?);
            text(s.find(&t).map(|i| s[..i].to_string()).unwrap_or_default())
        }
        "substring-after" => {
            arity(2, 2)?;
            let (s, t) = (str_arg(0)?, str_arg(1)?);
            text(
                s.find(&t)
                    .map(|i| s[i + t.len()..].to_string())
                    .unwrap_or_default(),
            )
        }
        "substring" => {
            arity(2, 3)?;
            let s = str_arg(0)?;
            let start = round(number(arg(1)?));
            let end = if args.len() == 3 {
                start + round(number(arg(2)?))
            } else {
                f64::INFINITY
            };
            // Characters are numbered from 1; comparisons with NaN are false.
            text(
                s.chars()
                    .enumerate()
                    .filter(|(i, _)| {
                        let p = (*i + 1) as f64;
                        p >= start && p < end
                    })
                    .map(|(_, c)| c)
                    .collect(),
            )
        }
        "string-length" => {
            arity(0, 1)?;
            let s = if args.is_empty() {
                ctx.item.string_value().into_owned()
            } else {
                str_arg(0)?
            };
            Value::Num(s.chars().count() as f64)
        }
        "normalize-space" => {
            arity(0, 1)?;
            let s = if args.is_empty() {
                ctx.item.string_value().into_owned()
            } else {
                str_arg(0)?
            };
            text(
                s.split([' ', '\t', '\n', '\r'])
                    .filter(|w| !w.is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
        "translate" => {
            arity(3, 3)?;
            let (s, from, to) = (str_arg(0)?, str_arg(1)?, str_arg(2)?);
            let from: Vec<char> = from.chars().collect();
            let to: Vec<char> = to.chars().collect();
            text(
                s.chars()
                    .filter_map(|c| match from.iter().position(|&f| f == c) {
                        Some(i) => to.get(i).copied(),
                        None => Some(c),
                    })
                    .collect(),
            )
        }
        "boolean" => {
            arity(1, 1)?;
            Value::Bool(boolean(&arg(0)?))
        }
        "not" => {
            arity(1, 1)?;
            Value::Bool(!boolean(&arg(0)?))
        }
        "true" => {
            arity(0, 0)?;
            Value::Bool(true)
        }
        "false" => {
            arity(0, 0)?;
            Value::Bool(false)
        }
        "lang" => {
            arity(1, 1)?;
            let want = str_arg(0)?.to_ascii_lowercase();
            let node = match ctx.item {
                Item::Node(n) => n,
                Item::Attr { owner, .. } => owner,
            };
            let declared = std::iter::once(node)
                .chain(ancestors(node))
                .find_map(|n| n.attr("xml:lang").or_else(|| n.attr("lang")))
                .map(str::to_ascii_lowercase);
            Value::Bool(declared.is_some_and(|l| l == want || l.starts_with(&format!("{want}-"))))
        }
        "number" => {
            arity(0, 1)?;
            Value::Num(if args.is_empty() {
                parse_number(&ctx.item.string_value())
            } else {
                number(arg(0)?)
            })
        }
        "sum" => {
            arity(1, 1)?;
            // Fold from 0.0: Rust's f64 sum() of nothing is -0.0.
            Value::Num(
                nodes_arg(0)?
                    .iter()
                    .map(|i| parse_number(&i.string_value()))
                    .fold(0.0, |a, b| a + b),
            )
        }
        "floor" => {
            arity(1, 1)?;
            Value::Num(number(arg(0)?).floor())
        }
        "ceiling" => {
            arity(1, 1)?;
            Value::Num(number(arg(0)?).ceil())
        }
        "round" => {
            arity(1, 1)?;
            Value::Num(round(number(arg(0)?)))
        }
        // parsel's extension: the context element has every class given.
        "has-class" => {
            arity(1, usize::MAX)?;
            let classes = match ctx.item {
                Item::Node(n) => n.attr("class").unwrap_or(""),
                Item::Attr { .. } => "",
            };
            let have: Vec<&str> = classes.split_ascii_whitespace().collect();
            let mut all = true;
            for i in 0..args.len() {
                all &= have.contains(&str_arg(i)?.as_str());
            }
            Value::Bool(all)
        }
        "re:test" => {
            arity(2, 3)?;
            let flags = if args.len() == 3 {
                str_arg(2)?
            } else {
                String::new()
            };
            Value::Bool(regex(&str_arg(1)?, &flags)?.is_match(&str_arg(0)?))
        }
        "re:replace" => {
            arity(4, 4)?;
            let (input, flags, with) = (str_arg(0)?, str_arg(2)?, str_arg(3)?);
            let re = regex(&str_arg(1)?, &flags)?;
            let expand = |caps: &regex::Captures<'_>| substitute(caps, &with);
            text(if flags.contains('g') {
                re.replace_all(&input, expand).into_owned()
            } else {
                re.replace(&input, expand).into_owned()
            })
        }
        _ => return Err(format!("unknown function {name}()")),
    })
}

/// XPath's round: halves go up, and -0.5 <= x < 0 rounds to -0.
fn round(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        x
    } else if (-0.5..0.0).contains(&x) {
        -0.0
    } else {
        // Not (x + 0.5).floor(): the addition rounds, which turns
        // 0.49999999999999994 into 1 and moves odd integers above 2^52.
        let f = x.floor();
        if x - f >= 0.5 { f + 1.0 } else { f }
    }
}

/// A replacement written as lxml (and Python's `re`) reads it: `\1` to
/// `\99` and `\g<name>` are groups, `\\` is a backslash, `\n` and `\t`
/// are escapes, and `$` is just a dollar sign.
fn substitute(caps: &regex::Captures<'_>, with: &str) -> String {
    let group = |out: &mut String, m: Option<regex::Match<'_>>| {
        if let Some(m) = m {
            out.push_str(m.as_str());
        }
    };
    let mut out = String::new();
    let mut chars = with.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(d @ '0'..='9') => {
                let mut n = d.to_digit(10).unwrap() as usize;
                if let Some(e) = chars.peek().and_then(|e| e.to_digit(10)) {
                    n = n * 10 + e as usize;
                    chars.next();
                }
                group(&mut out, caps.get(n));
            }
            Some('g') if chars.peek() == Some(&'<') => {
                chars.next();
                let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
                let m = match name.parse::<usize>() {
                    Ok(n) => caps.get(n),
                    Err(_) => caps.name(&name),
                };
                group(&mut out, m);
            }
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// EXSLT flags: `i` ignores case, `g` (replace only) replaces every match.
fn regex(pattern: &str, flags: &str) -> Result<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(flags.contains('i'))
        .build()
        // The regex crate's message spans several lines with a caret; its
        // last line says what is wrong.
        .map_err(|e| {
            let text = e.to_string();
            let why = text
                .lines()
                .last()
                .unwrap_or("")
                .trim_start_matches("error: ")
                .to_string();
            format!("bad regular expression {pattern:?}: {why}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_float_formatting() {
        for (n, s) in [
            (3.0, "3.0"),
            (-2.0, "-2.0"),
            (0.5, "0.5"),
            (12.5, "12.5"),
            (-0.0, "-0.0"),
            (1e16, "1e+16"),
            (1.5e-5, "1.5e-05"),
            (1e100, "1e+100"),
            (20000.0, "20000.0"),
        ] {
            assert_eq!(python_float(n), s, "{n}");
        }
    }

    #[test]
    fn xpath_number_grammar() {
        assert_eq!(parse_number(" 12.5\n"), 12.5);
        assert_eq!(parse_number("-.5"), -0.5);
        assert_eq!(parse_number("5."), 5.0);
        for bad in ["", ".", "1e3", "+1", "0x10", "1.2.3", "inf", "NaN", "- 1"] {
            assert!(parse_number(bad).is_nan(), "{bad:?}");
        }
    }
}
