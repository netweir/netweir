//! Evaluating an XPath expression tree against a document.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::index::{CDATA, COMMENT, ELEMENT, PI, Scopes, TEXT};
use crate::parser::{Axis, Expr, NodeTest, Op, Step};
use crate::{Doc, Extensions, Item, NodeId, Value, XML_DECL, XML_NS, XValue};

/// How much work one run may do before it gives up: nodes looked at on
/// axes and by `lang()`, predicate tests, and nodes and every 16 bytes of
/// text read for string-values. Some queries are quadratic on any engine
/// (`//p[following::p]`), and a hostile page can make them run for hours.
/// A unit takes one to twenty-odd nanoseconds, so this is a few seconds
/// at most: still over ten times what the slowest query of the benchmark
/// pages needs.
pub const WORK_LIMIT: u64 = 1 << 27;

struct Context<'a, 'v> {
    item: Item,
    position: usize,
    size: usize,
    doc: Doc<'a>,
    root: NodeId,
    vars: &'v [(String, XValue)],
    namespaces: &'v [(String, String)],
    kept: &'v RefCell<Kept>,
    /// What's in scope at each block of namespace declarations this run
    /// has needed, worked out once each.
    in_scope: &'v RefCell<InScope>,
    ext: &'v dyn Extensions,
    /// Work done so far, and the most allowed (see [`WORK_LIMIT`]).
    spent: &'v Cell<u64>,
    limit: u64,
    /// Inside a predicate, where the same node can be tested many times.
    nested: bool,
}

/// Values of a run's [`Expr::Memo`] expressions, by slot and node key. Only
/// a number or a yes or no is ever needed from them.
type Kept = HashMap<(usize, (u32, u64)), Scalar>;

/// The namespaces in scope by block of declarations (see
/// [`crate::index::Scopes`]): the numbers of the declarations that bind
/// them, ascending, which is the order the axis gives them in.
#[derive(Default)]
struct InScope {
    lists: HashMap<u32, Rc<[u32]>>,
    /// How often each block has been climbed past without being worked
    /// out.
    passes: Vec<u8>,
    /// The prefixes seen while working one out, marked with `pass`.
    seen: Vec<u32>,
    pass: u32,
}

/// What's in scope at `block`: its declarations, those of the blocks
/// `between` it and the next block up, and that one's, which must be in
/// `lists` unless there's none. Gives the list and the work it took.
fn gather(
    scopes: &Scopes,
    lists: &HashMap<u32, Rc<[u32]>>,
    seen: &mut [u32],
    pass: &mut u32,
    block: u32,
    between: &[u32],
) -> (Rc<[u32]>, usize) {
    *pass = pass.wrapping_add(1);
    if *pass == 0 {
        seen.fill(0);
        *pass = 1;
    }
    // A block's declarations are numbered after those above it, and stored
    // last written first, so going down the numbers meets each prefix's
    // binding nearest first, and the first written of a node's own first.
    let mut found = Vec::new();
    let mut looked = 0;
    let mut keep = |d: u32| {
        let decl = &scopes.decls[d as usize];
        if seen[decl.prefix as usize] != *pass {
            seen[decl.prefix as usize] = *pass;
            if !decl.undeclares {
                found.push(d);
            }
        }
    };
    let mut top = u32::MAX;
    for &b in std::iter::once(&block).chain(between) {
        let at = &scopes.blocks[b as usize];
        looked += at.len as usize + 1;
        for d in (at.first..at.first + at.len).rev() {
            keep(d);
        }
        top = at.parent;
    }
    if let Some(above) = lists.get(&top) {
        looked += above.len();
        for &d in above.iter().rev() {
            keep(d);
        }
    }
    found.reverse();
    (found.into(), looked)
}

#[derive(Debug, Clone, Copy)]
enum Scalar {
    Num(f64),
    Bool(bool),
}

type Result<T> = std::result::Result<T, String>;

fn text<'a>(s: String) -> Value<'a> {
    Value::Str(Cow::Owned(s))
}

/// Evaluates `expr` with `scope` as the context node.
pub(crate) fn run<'a>(
    expr: &Expr,
    doc: Doc<'a>,
    scope: NodeId,
    vars: &[(String, XValue)],
    namespaces: &[(String, String)],
    ext: &dyn Extensions,
    limit: u64,
) -> Result<Value<'a>> {
    // `/` is the top of the scope's tree: the document, or for a node
    // removed from it, the top of what was removed (as lxml has it).
    let mut root = scope;
    while let Some(up) = doc.tree.parent(root) {
        root = up;
    }
    if doc.index.order(root) == u32::MAX {
        return Err(format!(
            "the top of node {scope}'s tree, node {root}, isn't in the index"
        ));
    }
    let kept = RefCell::default();
    let in_scope = RefCell::default();
    let spent = Cell::new(0);
    let ctx = Context {
        item: Item::Node(scope),
        position: 1,
        size: 1,
        doc,
        root,
        vars,
        namespaces,
        kept: &kept,
        in_scope: &in_scope,
        ext,
        spent: &spent,
        limit,
        nested: false,
    };
    eval(expr, &ctx)
}

impl<'a> Context<'a, '_> {
    /// Counts `units` of work, failing once the run has done too much.
    fn spend(&self, units: usize) -> Result<()> {
        let spent = self.spent.get().saturating_add(units as u64);
        self.spent.set(spent);
        if spent > self.limit {
            return Err(format!(
                "the query needs more than {} steps on this document, so it was stopped",
                self.limit
            ));
        }
        Ok(())
    }

    /// Counts a string-value's text, which [`Context::spend`] then sees.
    fn charge_text(&self, bytes: usize) {
        let units = (bytes / 16) as u64;
        self.spent.set(self.spent.get().saturating_add(units));
    }

    fn order(&self, n: NodeId) -> u32 {
        self.doc.index.order(n)
    }

    /// Position in document order. An element's namespace nodes come
    /// after the element, then its attributes, and then its children,
    /// whose order is higher.
    fn key(&self, item: &Item) -> (u32, u64) {
        match *item {
            Item::Node(n) => (self.order(n), 0),
            Item::Ns { owner, decl } => (
                self.order(owner),
                if decl == XML_DECL { 1 } else { decl as u64 + 2 },
            ),
            Item::Attr { owner, index } => (self.order(owner), (1 << 33) + index as u64),
        }
    }

    /// The namespace nodes of the element `n`: `xml`, then the rest in
    /// scope, as numbers of the declarations that bind them.
    fn namespaces(&self, n: NodeId) -> Result<Rc<[u32]>> {
        let (scopes, built) = self.doc.index.scopes(self.doc.tree);
        if built {
            self.spend(self.doc.index.len())?;
        }
        let block = scopes.block_at(self.order(n));
        if block == u32::MAX {
            return Ok(Rc::from([]));
        }
        let mut cache = self.in_scope.borrow_mut();
        let InScope {
            lists,
            passes,
            seen,
            pass,
        } = &mut *cache;
        if let Some(list) = lists.get(&block) {
            return Ok(list.clone());
        }
        if seen.len() < scopes.prefixes {
            seen.resize(scopes.prefixes, 0);
        }
        if passes.len() < scopes.blocks.len() {
            passes.resize(scopes.blocks.len(), 0);
        }
        // The blocks above this one up to one already worked out, nearest
        // first. One climbed past a second time is worked out itself, so
        // a long chain of declarations is climbed at most once, and what's
        // worked out from the one above it costs no more than the
        // namespace nodes it has.
        let mut climbed = Vec::new();
        let mut b = scopes.blocks[block as usize].parent;
        while b != u32::MAX && !lists.contains_key(&b) {
            passes[b as usize] = passes[b as usize].saturating_add(1);
            climbed.push(b);
            b = scopes.blocks[b as usize].parent;
        }
        let again = climbed
            .iter()
            .position(|&b| passes[b as usize] > 1)
            .unwrap_or(climbed.len());
        let mut looked = 0;
        for &b in climbed[again..].iter().rev() {
            let (list, cost) = gather(scopes, lists, seen, pass, b, &[]);
            looked += cost;
            lists.insert(b, list);
        }
        let (list, cost) = gather(scopes, lists, seen, pass, block, &climbed[..again]);
        looked += cost;
        lists.insert(block, list.clone());
        drop(cache);
        self.spend(looked)?;
        Ok(list)
    }

    fn kind(&self, n: NodeId) -> u8 {
        self.doc.index.kind_at(self.order(n))
    }

    /// A text, comment or processing instruction node's own text.
    fn data(&self, n: NodeId) -> &'a str {
        self.doc.tree.data(n)
    }

    fn string_value(&self, item: &Item) -> Cow<'a, str> {
        let n = match item {
            Item::Attr { owner, index, .. } => {
                return Cow::Borrowed(self.doc.attr_at(*owner, *index).1);
            }
            Item::Ns { decl, .. } => return Cow::Borrowed(self.doc.namespace_at(*decl).1),
            Item::Node(n) => *n,
        };
        let index = self.doc.index;
        let at = self.order(n);
        match index.kind_at(at) {
            TEXT | CDATA | COMMENT | PI => {
                let data = self.data(n);
                self.charge_text(data.len());
                Cow::Borrowed(data)
            }
            _ => {
                // Every text node below, read off the index's range.
                let end = index.end_of(at);
                self.spent
                    .set(self.spent.get().saturating_add((end - at) as u64));
                // Borrowed while there is only one piece, as in `<p>x</p>`.
                let mut out = Cow::Borrowed("");
                for j in at + 1..end {
                    if matches!(index.kind_at(j), TEXT | CDATA) {
                        let piece = self.data(index.node_at(j));
                        // What's done with it (compared, searched) is in
                        // proportion to its length.
                        self.charge_text(piece.len());
                        if out.is_empty() {
                            out = Cow::Borrowed(piece);
                        } else {
                            out.to_mut().push_str(piece);
                        }
                    }
                }
                out
            }
        }
    }

    fn pi_target(&self, n: NodeId) -> Option<&'a str> {
        self.doc.tree.pi_target(n)
    }

    /// The attribute an item is, if it's one.
    fn attribute(&self, item: &Item) -> Option<crate::Attr<'a>> {
        match item {
            Item::Attr { owner, index } => self.doc.tree.attribute(*owner, *index),
            _ => None,
        }
    }

    /// The name with its prefix, as `name()` gives it.
    fn name(&self, item: &Item) -> &'a str {
        match item {
            Item::Node(n) => match self.pi_target(*n) {
                Some(target) => target,
                None => self.doc.tree.qualified_name(*n),
            },
            Item::Attr { .. } => self.attribute(item).map_or("", |a| a.qualified_name),
            // A namespace node's name is its prefix, "" for the default.
            Item::Ns { decl, .. } => self.doc.namespace_at(*decl).0,
        }
    }

    /// The name without its prefix, as `local-name()` gives it.
    fn local_name(&self, item: &Item) -> &'a str {
        match item {
            Item::Node(n) => match self.pi_target(*n) {
                Some(target) => target,
                None => self.doc.tree.local_name(*n),
            },
            Item::Attr { .. } => self.attribute(item).map_or("", |a| a.local_name),
            Item::Ns { decl, .. } => self.doc.namespace_at(*decl).0,
        }
    }

    fn namespace_uri(&self, item: &Item) -> &'a str {
        match item {
            Item::Node(n) => self.doc.tree.namespace_uri(*n),
            Item::Attr { .. } => self.attribute(item).map_or("", |a| a.namespace_uri),
            Item::Ns { .. } => "",
        }
    }

    /// The position of the top of the tree being queried, and one past its
    /// last node.
    fn tree_range(&self) -> (u32, u32) {
        let top = self.order(self.root);
        (top, self.doc.index.end_of(top))
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
        Expr::Negate(e) => Value::Num(-number(eval(e, ctx)?, ctx)),
        Expr::Binary(op, l, r) => binary(*op, l, r, ctx)?,
        Expr::Union(l, r) => {
            let (Value::Nodes(mut a), Value::Nodes(b)) = (eval(l, ctx)?, eval(r, ctx)?) else {
                return Err("both sides of | must be node-sets".into());
            };
            a.extend(b);
            Value::Nodes(in_document_order(a, ctx))
        }
        Expr::Call(name, args) => call(name, args, ctx)?,
        Expr::Memo(e, slot) => {
            let key = (*slot, ctx.key(&ctx.item));
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

fn in_document_order<'a>(items: Vec<Item>, ctx: &Context<'a, '_>) -> Vec<Item> {
    // Often they already are, and sorting a copy would double the memory.
    if items.windows(2).all(|w| ctx.key(&w[0]) < ctx.key(&w[1])) {
        return items;
    }
    // A key costs an index lookup, so each is read once.
    let mut keyed: Vec<_> = items.into_iter().map(|i| (ctx.key(&i), i)).collect();
    keyed.sort_unstable_by_key(|(k, _)| *k);
    keyed.dedup_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, i)| i).collect()
}

fn walk<'a>(mut items: Vec<Item>, steps: &[Step], ctx: &Context<'a, '_>) -> Result<Vec<Item>> {
    let index = ctx.doc.index;
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
        let test = Test::resolve(&step.test, step.axis, ctx)?;
        // Without predicates, a step's result is a set, whichever context
        // node each member came from: work it out for the set at once, so
        // nested or neighbouring context nodes don't each walk the same
        // nodes again (`//div//div` on nested divs was quadratic).
        if step.predicates.is_empty()
            && items.len() > 1
            && let Some(found) = whole_set(&items, step.axis, &test, ctx)?
        {
            items = found;
            continue;
        }
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
                // scan the document index instead of walking the tree.
                (Item::Node(n), true)
                    if matches!(
                        step.axis,
                        Axis::Child | Axis::Descendant | Axis::DescendantOrSelf
                    ) =>
                {
                    let at = ctx.order(*n);
                    let keep = |kind, tag| test.keeps(kind, tag);
                    scanned.clear();
                    match step.axis {
                        Axis::Child => index.children(at, keep, &mut scanned),
                        axis => {
                            ctx.spend((index.end_of(at) - at) as usize)?;
                            index.descendants(
                                at,
                                axis == Axis::DescendantOrSelf,
                                keep,
                                &mut scanned,
                            )
                        }
                    }
                    candidates.extend(
                        scanned
                            .iter()
                            .copied()
                            .map(Item::Node)
                            .filter(|i| !test.exact() || test.matches(i, ctx)),
                    );
                }
                _ => {
                    let looked = Cell::new(0usize);
                    let mut on_axis = axis(*item, step.axis, ctx)?
                        .inspect(|_| looked.set(looked.get() + 1))
                        .filter(|c| test.matches(c, ctx));
                    if nth.is_some() {
                        done = 1;
                    }
                    match nth {
                        // `[k]` first: stop at the kth node instead of
                        // collecting the whole axis (following-sibling::dd[1]).
                        Some(k) => candidates.extend(on_axis.nth(k - 1)),
                        None => candidates.extend(on_axis),
                    }
                    ctx.spend(looked.get())?;
                }
            }
            ctx.spend(candidates.len() + 1)?;
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
            in_document_order(next, ctx)
        };
    }
    Ok(items)
}

/// A step without predicates from every one of `items` (in document
/// order) at once, in document order: None for the axes that gain nothing
/// from it.
fn whole_set<'a>(
    items: &[Item],
    axis: Axis,
    test: &Test<'_>,
    ctx: &Context<'a, '_>,
) -> Result<Option<Vec<Item>>> {
    let index = ctx.doc.index;
    let (_, tree_end) = ctx.tree_range();
    let keep = |j: u32| {
        let item = Item::Node(index.node_at(j));
        if test.indexable() {
            test.keeps(index.kind_at(j), index.tag_at(j))
                && (!test.exact() || test.matches(&item, ctx))
        } else {
            test.matches(&item, ctx)
        }
    };
    let mut out = Vec::new();
    match axis {
        Axis::Descendant | Axis::DescendantOrSelf => {
            // A node inside an earlier item's subtree was scanned with it.
            let mut covered_to = 0;
            for item in items {
                match item {
                    Item::Node(n) => {
                        let at = ctx.order(*n);
                        if at < covered_to {
                            continue;
                        }
                        covered_to = index.end_of(at);
                        let from = if axis == Axis::DescendantOrSelf {
                            at
                        } else {
                            at + 1
                        };
                        ctx.spend((covered_to - from) as usize)?;
                        out.extend(
                            (from..covered_to)
                                .filter(|&j| keep(j))
                                .map(|j| Item::Node(index.node_at(j))),
                        );
                    }
                    // An attribute has no descendants, only itself.
                    attr if axis == Axis::DescendantOrSelf && test.matches(attr, ctx) => {
                        out.push(*attr)
                    }
                    _ => {}
                }
            }
            return Ok(Some(in_document_order(out, ctx)));
        }
        Axis::Following => {
            // Everything after the subtree that ends first: an attribute's
            // following nodes start with its element's children.
            let Some(from) = items
                .iter()
                .map(|i| match i {
                    Item::Node(n) => index.end_of(ctx.order(*n)),
                    Item::Attr { owner, .. } | Item::Ns { owner, .. } => ctx.order(*owner) + 1,
                })
                .min()
            else {
                return Ok(Some(out));
            };
            ctx.spend(tree_end.saturating_sub(from) as usize)?;
            out.extend(
                (from..tree_end)
                    .filter(|&j| keep(j))
                    .map(|j| Item::Node(index.node_at(j))),
            );
        }
        Axis::Preceding => {
            // The last item's preceding nodes hold every other item's.
            let Some(last) = items.iter().map(|i| ctx.key(i).0).max() else {
                return Ok(Some(out));
            };
            let (top, _) = ctx.tree_range();
            ctx.spend(last.saturating_sub(top) as usize)?;
            // Not its ancestors, whose subtrees reach past it.
            out.extend(
                (top..last)
                    .filter(|&j| index.end_of(j) <= last && keep(j))
                    .map(|j| Item::Node(index.node_at(j))),
            );
        }
        Axis::Ancestor | Axis::AncestorOrSelf => {
            // Climb from each item until reaching a node already climbed.
            let mut seen = HashSet::new();
            for item in items {
                let start = match item {
                    Item::Node(n) if axis == Axis::Ancestor => index.parent_of(ctx.order(*n)),
                    Item::Node(n) => ctx.order(*n),
                    Item::Attr { owner, .. } | Item::Ns { owner, .. } => {
                        if axis == Axis::AncestorOrSelf && test.matches(item, ctx) {
                            out.push(*item);
                        }
                        ctx.order(*owner)
                    }
                };
                let mut at = start;
                while at != u32::MAX && seen.insert(at) {
                    ctx.spend(1)?;
                    if keep(at) {
                        out.push(Item::Node(index.node_at(at)));
                    }
                    at = index.parent_of(at);
                }
            }
            return Ok(Some(in_document_order(out, ctx)));
        }
        Axis::FollowingSibling | Axis::PrecedingSibling => {
            // Per parent, only the siblings of its first item (following)
            // or its last (preceding) count. Attributes have no siblings.
            let tree = ctx.doc.tree;
            let mut from: HashMap<NodeId, NodeId> = HashMap::new();
            for item in items {
                if let Item::Node(n) = item
                    && let Some(p) = tree.parent(*n)
                {
                    if axis == Axis::FollowingSibling {
                        from.entry(p).or_insert(*n);
                    } else {
                        from.insert(p, *n);
                    }
                }
            }
            for &n in from.values() {
                let step = |&s: &NodeId| match axis {
                    Axis::FollowingSibling => tree.next_sibling(s),
                    _ => tree.prev_sibling(s),
                };
                for s in std::iter::successors(step(&n), step) {
                    ctx.spend(1)?;
                    if keep(ctx.order(s)) {
                        out.push(Item::Node(s));
                    }
                }
            }
            return Ok(Some(in_document_order(out, ctx)));
        }
        _ => return Ok(None),
    }
    Ok(Some(out))
}

/// `//x[pred]` from each item in `items`: every descendant passing `step`'s
/// test, grouped by parent so predicate positions count siblings, in
/// document order. None when the test can't be read from the index.
fn descendants_by_parent<'a>(
    items: &[Item],
    step: &Step,
    ctx: &Context<'a, '_>,
) -> Option<Result<Vec<Item>>> {
    let test = match Test::resolve(&step.test, step.axis, ctx) {
        Ok(test) => test,
        Err(e) => return Some(Err(e)),
    };
    if !test.indexable() {
        return None;
    }
    let index = ctx.doc.index;
    let mut found = Vec::new();
    // Items come in document order. One inside an earlier item's subtree
    // was scanned with it, and scanning it again would count its
    // descendants twice.
    let mut covered_to = 0;
    for item in items {
        // An attribute has no children.
        let Item::Node(n) = item else { continue };
        let at = ctx.order(*n);
        if at < covered_to {
            continue;
        }
        covered_to = index.end_of(at);
        if let Err(e) = ctx.spend((covered_to - at) as usize) {
            return Some(Err(e));
        }
        index.descendants(at, false, |k, t| test.keeps(k, t), &mut found);
    }
    if test.exact() {
        found.retain(|&n| test.matches(&Item::Node(n), ctx));
    }
    // Predicates blind to the position need no sibling groups: the matches
    // are in document order already. Each is tested once, so outside
    // another predicate, keeping their answers would only take memory.
    if !step.predicates.iter().any(Expr::positional) {
        let mut items: Vec<Item> = found.into_iter().map(Item::Node).collect();
        for p in &step.predicates {
            let p = match p {
                Expr::Memo(inner, _) if !ctx.nested => inner,
                p => p,
            };
            items = match filter(items, p, ctx) {
                Ok(items) => items,
                Err(e) => return Some(Err(e)),
            };
        }
        return Some(Ok(items));
    }
    // Group by parent, keeping each group in document order.
    let mut groups: Vec<(u32, Vec<Item>)> = Vec::new();
    let mut slot: HashMap<u32, usize> = HashMap::new();
    for node in found {
        let parent = index.parent_of(ctx.order(node));
        let at = *slot.entry(parent).or_insert_with(|| {
            groups.push((parent, Vec::new()));
            groups.len() - 1
        });
        groups[at].1.push(Item::Node(node));
    }
    let run = || -> Result<Vec<Item>> {
        let mut out = Vec::new();
        for (_, mut group) in groups {
            for p in &step.predicates {
                group = filter(group, p, ctx)?;
            }
            out.extend(group);
        }
        Ok(in_document_order(out, ctx))
    };
    Some(run())
}

/// `[@name = 'value']`, `['value' = @name]` and `[@name]`: answered from
/// the element directly, without building its attribute list. None for any
/// other predicate.
fn quick_predicate(pred: &Expr, item: &Item, doc: Doc<'_>) -> Option<bool> {
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
                // Doc::attr folds case when looking an attribute up, but
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
            Some(doc.attr(*node, &name) == Some(value.as_str()))
        }
        other => attr_name(other).map(|n| doc.attr(*node, &n).is_some()),
    }
}

/// Keeps the items for which `pred` holds, numbering them in the order
/// given (axis order inside a step, document order in a filter).
fn filter<'a>(items: Vec<Item>, pred: &Expr, ctx: &Context<'a, '_>) -> Result<Vec<Item>> {
    let size = items.len();
    ctx.spend(size)?;
    // Kept in place: a second set as large would double the memory.
    let mut kept = items;
    let mut len = 0;
    for i in 0..size {
        let item = kept[i];
        if let Some(keep) = quick_predicate(pred, &item, ctx.doc) {
            if keep {
                kept[len] = item;
                len += 1;
            }
            continue;
        }
        let inner = Context {
            item,
            position: i + 1,
            size,
            nested: true,
            ..*ctx
        };
        let keep = match eval(pred, &inner)? {
            Value::Num(n) => n == (i + 1) as f64,
            other => boolean(&other),
        };
        if keep {
            kept[len] = item;
            len += 1;
        }
    }
    kept.truncate(len);
    Ok(kept)
}

/// The URI `prefix` is bound to for this run.
fn namespace<'v>(prefix: &str, ctx: &Context<'_, 'v>) -> Result<&'v str> {
    if let Some((_, uri)) = ctx.namespaces.iter().rev().find(|(p, _)| p == prefix) {
        return Ok(uri);
    }
    if prefix == "xml" {
        return Ok(XML_NS);
    }
    Err(format!("undefined namespace prefix {prefix:?}"))
}

/// A node test made ready for one document: names become tag ids,
/// so matching an element is one integer comparison.
enum Test<'t> {
    Node,
    Text,
    Comment,
    /// `processing-instruction()`, with the target if one is given.
    Pi(Option<&'t str>),
    Never,
    /// `*` on the axis's principal node type.
    AnyElement,
    AnyAttribute,
    AnyNamespace,
    /// A namespace node by prefix.
    NamespaceName(&'t str),
    ElementId(usize),
    /// A name with capitals: no HTML element has one (the parser lowercases
    /// them), but SVG and MathML elements can, so compare the text.
    ElementName(&'t str),
    AttributeName(&'t str),
    /// An XML element by namespace URI and local name. `id` is the
    /// index's id for the lowercased name, which the index can check
    /// first; the case and namespace are checked after.
    XmlElement {
        id: usize,
        local: &'t str,
        ns: &'t str,
    },
    /// Any XML element in a namespace.
    XmlElementIn(&'t str),
    XmlAttribute {
        local: &'t str,
        ns: &'t str,
    },
    XmlAttributeIn(&'t str),
}

impl<'t> Test<'t> {
    fn resolve(test: &'t NodeTest, axis: Axis, ctx: &Context<'_, 't>) -> Result<Test<'t>> {
        if axis == Axis::Namespace {
            // Namespace nodes are named by prefix, and in no namespace.
            return Ok(match test {
                NodeTest::Node => Test::Node,
                NodeTest::Any => Test::AnyNamespace,
                NodeTest::Name(prefix) => Test::NamespaceName(prefix),
                NodeTest::Prefixed(p, _) | NodeTest::AnyIn(p) => {
                    namespace(p, ctx)?;
                    Test::Never
                }
                _ => Test::Never,
            });
        }
        let attr = axis == Axis::Attribute;
        let index = ctx.doc.index;
        if ctx.doc.is_xml() {
            // The URI a prefix stands for here.
            let ns = |prefix: &str| namespace(prefix, ctx);
            return Ok(match test {
                NodeTest::Name(local) if attr => Test::XmlAttribute { local, ns: "" },
                NodeTest::Prefixed(p, local) if attr => Test::XmlAttribute { local, ns: ns(p)? },
                NodeTest::AnyIn(p) if attr => Test::XmlAttributeIn(ns(p)?),
                NodeTest::Name(local) => match index.tag_id_named(local) {
                    Some(id) => Test::XmlElement { id, local, ns: "" },
                    None => Test::Never,
                },
                NodeTest::Prefixed(p, local) => match (ns(p)?, index.tag_id_named(local)) {
                    (ns, Some(id)) => Test::XmlElement { id, local, ns },
                    _ => Test::Never,
                },
                NodeTest::AnyIn(p) => Test::XmlElementIn(ns(p)?),
                other => Test::resolve_html(other, attr, ctx),
            });
        }
        if let NodeTest::Prefixed(p, _) | NodeTest::AnyIn(p) = test {
            // Checked, so a typo is still an error; but HTML has no
            // namespaces, so nothing matches.
            namespace(p, ctx)?;
            return Ok(Test::Never);
        }
        Ok(Test::resolve_html(test, attr, ctx))
    }

    fn resolve_html(test: &'t NodeTest, attr: bool, ctx: &Context<'_, '_>) -> Test<'t> {
        match test {
            NodeTest::Node => Test::Node,
            NodeTest::Text if attr => Test::Never,
            NodeTest::Text => Test::Text,
            NodeTest::Comment if attr => Test::Never,
            NodeTest::Comment => Test::Comment,
            // HTML5 parsing turns processing instructions into comments;
            // XML keeps them.
            NodeTest::ProcessingInstruction(_) if attr => Test::Never,
            NodeTest::ProcessingInstruction(target) => Test::Pi(target.as_deref()),
            NodeTest::Any if attr => Test::AnyAttribute,
            NodeTest::Any => Test::AnyElement,
            NodeTest::Name(name) if attr => Test::AttributeName(name),
            NodeTest::Name(name) if name.bytes().any(|b| b.is_ascii_uppercase()) => {
                Test::ElementName(name)
            }
            NodeTest::Name(name) => ctx
                .doc
                .index
                .tag_id_named(name)
                .map_or(Test::Never, Test::ElementId),
            NodeTest::Prefixed(..) | NodeTest::AnyIn(_) => Test::Never,
        }
    }

    /// Whether `keeps` can decide this test from the index alone, or (for
    /// an `exact` one) narrow it down.
    fn indexable(&self) -> bool {
        matches!(
            self,
            Test::Node
                | Test::Text
                | Test::Comment
                | Test::Pi(None)
                | Test::AnyElement
                | Test::ElementId(_)
                | Test::XmlElement { .. }
                | Test::Never
        )
    }

    /// Whether what `keeps` lets through still needs `matches`.
    fn exact(&self) -> bool {
        matches!(self, Test::XmlElement { .. })
    }

    /// The test, from a node's kind and tag id.
    fn keeps(&self, kind: u8, tag: usize) -> bool {
        match self {
            Test::Node => true,
            Test::Text => kind == TEXT || kind == CDATA,
            Test::Comment => kind == COMMENT,
            Test::Pi(None) => kind == PI,
            Test::AnyElement => kind == ELEMENT,
            Test::ElementId(id) | Test::XmlElement { id, .. } => kind == ELEMENT && tag == *id,
            _ => false,
        }
    }

    fn matches(&self, item: &Item, ctx: &Context<'_, '_>) -> bool {
        let kind = |n: &NodeId| ctx.kind(*n);
        match (self, item) {
            (Test::Node, _) => true,
            (Test::Never, _) => false,
            (Test::ElementId(id), Item::Node(n)) => {
                let at = ctx.order(*n);
                ctx.doc.index.tag_at(at) == *id && ctx.doc.index.kind_at(at) == ELEMENT
            }
            (Test::ElementName(name), Item::Node(n)) => {
                kind(n) == ELEMENT && ctx.doc.tree.local_name(*n) == *name
            }
            (Test::AnyElement, Item::Node(n)) => kind(n) == ELEMENT,
            (Test::Text, Item::Node(n)) => matches!(kind(n), TEXT | CDATA),
            (Test::Comment, Item::Node(n)) => kind(n) == COMMENT,
            (Test::Pi(target), Item::Node(n)) => {
                kind(n) == PI && target.is_none_or(|t| ctx.pi_target(*n) == Some(t))
            }
            (Test::AnyAttribute, Item::Attr { .. }) => true,
            (Test::AnyNamespace, Item::Ns { .. }) => true,
            (Test::NamespaceName(prefix), Item::Ns { decl, .. }) => {
                ctx.doc.namespace_at(*decl).0 == *prefix
            }
            (Test::AttributeName(name), Item::Attr { owner, index }) => {
                ctx.doc.attr_at(*owner, *index).0 == *name
            }
            (Test::XmlElement { local, ns, .. }, Item::Node(n)) => {
                kind(n) == ELEMENT
                    && ctx.doc.tree.local_name(*n) == *local
                    && ctx.doc.tree.namespace_uri(*n) == *ns
            }
            (Test::XmlElementIn(ns), Item::Node(n)) => {
                kind(n) == ELEMENT && ctx.doc.tree.namespace_uri(*n) == *ns
            }
            (Test::XmlAttribute { local, ns }, i @ Item::Attr { .. }) => ctx
                .attribute(i)
                .is_some_and(|a| a.namespace_uri == *ns && a.local_name == *local),
            (Test::XmlAttributeIn(ns), i @ Item::Attr { .. }) => {
                ctx.attribute(i).is_some_and(|a| a.namespace_uri == *ns)
            }
            _ => false,
        }
    }
}

fn attributes(n: NodeId, doc: Doc<'_>) -> impl Iterator<Item = Item> + use<> {
    (0..doc.tree.attribute_count(n)).map(move |index| Item::Attr { owner: n, index })
}

/// The nodes on `axis` from `item`, in axis order (nearest first for
/// reverse axes), produced lazily.
fn axis<'a>(
    item: Item,
    axis: Axis,
    ctx: &Context<'a, '_>,
) -> Result<Box<dyn Iterator<Item = Item> + 'a>> {
    let doc = ctx.doc;
    let (tree, index) = (doc.tree, doc.index);
    let (top, tree_end) = ctx.tree_range();
    let node_at = move |j: u32| Item::Node(index.node_at(j));
    let ancestors =
        move |n: NodeId| std::iter::successors(tree.parent(n), move |&p| tree.parent(p));
    let children =
        move |n: NodeId| std::iter::successors(tree.first_child(n), move |&c| tree.next_sibling(c));
    // Everything after `n` in document order that isn't its descendant.
    let following = move |n: NodeId| (index.end_of(index.order(n))..tree_end).map(node_at);
    // Everything before `n` that isn't its ancestor, nearest first.
    let preceding = move |n: NodeId| {
        let at = index.order(n);
        (top..at)
            .rev()
            .filter(move |&j| index.end_of(j) <= at)
            .map(node_at)
    };
    let descendants = move |n: NodeId| {
        let at = index.order(n);
        (at + 1..index.end_of(at)).map(node_at)
    };
    Ok(match item {
        Item::Node(n) => match axis {
            Axis::Child => Box::new(children(n).map(Item::Node)),
            Axis::Descendant => Box::new(descendants(n)),
            Axis::DescendantOrSelf => Box::new(std::iter::once(item).chain(descendants(n))),
            Axis::Parent => Box::new(tree.parent(n).map(Item::Node).into_iter()),
            Axis::Ancestor => Box::new(ancestors(n).map(Item::Node)),
            Axis::AncestorOrSelf => {
                Box::new(std::iter::once(n).chain(ancestors(n)).map(Item::Node))
            }
            Axis::FollowingSibling => Box::new(
                std::iter::successors(tree.next_sibling(n), move |&s| tree.next_sibling(s))
                    .map(Item::Node),
            ),
            Axis::PrecedingSibling => Box::new(
                std::iter::successors(tree.prev_sibling(n), move |&s| tree.prev_sibling(s))
                    .map(Item::Node),
            ),
            Axis::Following => Box::new(following(n)),
            Axis::Preceding => Box::new(preceding(n)),
            Axis::Attribute if ctx.kind(n) == ELEMENT => Box::new(attributes(n, doc)),
            Axis::Namespace if ctx.kind(n) == ELEMENT => {
                let list = ctx.namespaces(n)?;
                Box::new(
                    std::iter::once(XML_DECL)
                        .chain((0..list.len()).map(move |k| list[k]))
                        .map(move |decl| Item::Ns { owner: n, decl }),
                )
            }
            Axis::Attribute | Axis::Namespace => Box::new(std::iter::empty()),
            Axis::Itself => Box::new(std::iter::once(item)),
        },
        Item::Attr { owner, .. } | Item::Ns { owner, .. } => match axis {
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
            Axis::Following => Box::new(descendants(owner).chain(following(owner))),
            Axis::Preceding => Box::new(preceding(owner)),
            _ => Box::new(std::iter::empty()),
        },
    })
}

// --- conversions (section 4) ----------------------------------------------

fn string<'a>(v: &Value<'a>, ctx: &Context<'a, '_>) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Num(n) => number_to_string(*n),
        Value::Bool(b) => b.to_string(),
        Value::Nodes(items) => items
            .first()
            .map(|i| ctx.string_value(i).into_owned())
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

fn number<'a>(v: Value<'a>, ctx: &Context<'a, '_>) -> f64 {
    match v {
        Value::Num(n) => n,
        Value::Bool(b) => b as u8 as f64,
        Value::Str(s) => parse_number(&s),
        nodes @ Value::Nodes(_) => parse_number(&string(&nodes, ctx)),
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
            let (a, b) = (number(eval(l, ctx)?, ctx), number(eval(r, ctx)?, ctx));
            Value::Num(match op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                Op::Div => a / b,
                // Truncating remainder, which is what Rust's % does on f64.
                _ => a % b,
            })
        }
        _ => Value::Bool(compare(op, eval(l, ctx)?, eval(r, ctx)?, ctx)),
    })
}

fn compare_atoms<'a>(op: Op, a: &Value<'a>, b: &Value<'a>, ctx: &Context<'a, '_>) -> bool {
    if matches!(op, Op::Eq | Op::Neq) {
        let equal = if matches!(a, Value::Bool(_)) || matches!(b, Value::Bool(_)) {
            boolean(a) == boolean(b)
        } else if matches!(a, Value::Num(_)) || matches!(b, Value::Num(_)) {
            number(a.clone(), ctx) == number(b.clone(), ctx)
        } else {
            // Without copying strings: a node-set compared with a string
            // compares every member.
            fn text<'v, 'a>(v: &'v Value<'a>, ctx: &Context<'a, '_>) -> Cow<'v, str> {
                match v {
                    Value::Str(s) => Cow::Borrowed(s),
                    other => Cow::Owned(string(other, ctx)),
                }
            }
            text(a, ctx) == text(b, ctx)
        };
        return equal == (op == Op::Eq);
    }
    let (x, y) = (number(a.clone(), ctx), number(b.clone(), ctx));
    match op {
        Op::Lt => x < y,
        Op::Le => x <= y,
        Op::Gt => x > y,
        _ => x >= y,
    }
}

/// Comparisons involving node-sets are true if any member makes them true.
fn compare<'a>(op: Op, a: Value<'a>, b: Value<'a>, ctx: &Context<'a, '_>) -> bool {
    match (&a, &b) {
        (Value::Nodes(xs), Value::Nodes(ys)) => {
            let ys: Vec<Value<'_>> = ys.iter().map(|y| Value::Str(ctx.string_value(y))).collect();
            xs.iter().any(|x| {
                let x = Value::Str(ctx.string_value(x));
                ys.iter().any(|y| compare_atoms(op, &x, y, ctx))
            })
        }
        (Value::Nodes(_), Value::Bool(_)) | (Value::Bool(_), Value::Nodes(_)) => compare_atoms(
            op,
            &Value::Bool(boolean(&a)),
            &Value::Bool(boolean(&b)),
            ctx,
        ),
        (Value::Nodes(xs), other) => xs
            .iter()
            .any(|x| compare_atoms(op, &atom_like(x, other, ctx), other, ctx)),
        (other, Value::Nodes(ys)) => ys
            .iter()
            .any(|y| compare_atoms(op, other, &atom_like(y, other, ctx), ctx)),
        _ => compare_atoms(op, &a, &b, ctx),
    }
}

/// A node's value as the type it is compared with.
fn atom_like<'a>(item: &Item, other: &Value<'_>, ctx: &Context<'a, '_>) -> Value<'a> {
    match other {
        Value::Num(_) => Value::Num(parse_number(&ctx.string_value(item))),
        _ => Value::Str(ctx.string_value(item)),
    }
}

// --- functions (section 4) -------------------------------------------------

/// The arguments of a call to an extension function, worked out when
/// asked for, as the core library's are.
pub struct Args<'c, 'a> {
    name: &'c str,
    exprs: &'c [Expr],
    ctx: &'c Context<'a, 'c>,
}

impl<'a> Args<'_, 'a> {
    pub fn len(&self) -> usize {
        self.exprs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    /// An error unless there are `min` to `max` arguments.
    pub fn arity(&self, min: usize, max: usize) -> Result<()> {
        arity(self.name, self.exprs.len(), min, max)
    }

    /// Argument `i`, evaluated.
    pub fn value(&self, i: usize) -> Result<Value<'a>> {
        eval(&self.exprs[i], self.ctx)
    }

    /// Argument `i`, converted as `string()` converts it.
    pub fn string(&self, i: usize) -> Result<String> {
        Ok(string(&self.value(i)?, self.ctx))
    }

    /// The context node.
    pub fn context(&self) -> Item {
        self.ctx.item
    }

    pub fn doc(&self) -> Doc<'a> {
        self.ctx.doc
    }
}

fn arity(name: &str, got: usize, min: usize, max: usize) -> Result<()> {
    if got < min || got > max {
        let expected = if min == max {
            format!("{min}")
        } else if max == usize::MAX {
            format!("at least {min}")
        } else {
            format!("{min} to {max}")
        };
        return Err(format!("{name}() takes {expected} arguments, got {got}"));
    }
    Ok(())
}

fn call<'a>(name: &str, args: &[Expr], ctx: &Context<'a, '_>) -> Result<Value<'a>> {
    let arity = |min: usize, max: usize| arity(name, args.len(), min, max);
    let arg = |i: usize| eval(&args[i], ctx);
    let str_arg = |i: usize| -> Result<String> { Ok(string(&eval(&args[i], ctx)?, ctx)) };
    let nodes_arg = |i: usize| -> Result<Vec<Item>> {
        match eval(&args[i], ctx)? {
            Value::Nodes(n) => Ok(n),
            _ => Err(format!("{name}() needs a node-set argument")),
        }
    };
    // The context node as a one-item node-set, for functions whose
    // argument defaults to it.
    let context_or = |i: usize| -> Result<Vec<Item>> {
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
                    .map(|i| ctx.string_value(i).into_owned())
                    .collect(),
                other => vec![string(&other, ctx)],
            };
            let index = ctx.doc.index;
            let doc = ctx.doc;
            let (top, end) = ctx.tree_range();
            let found = wanted
                .iter()
                .flat_map(|s| s.split_whitespace())
                .flat_map(|id| index.with_id(id, |n| doc.tree.id(n)))
                .filter(|&n| (top..end).contains(&ctx.order(n)))
                .map(Item::Node)
                .collect();
            Value::Nodes(in_document_order(found, ctx))
        }
        "local-name" | "name" | "namespace-uri" => {
            arity(0, 1)?;
            let first = in_document_order(context_or(0)?, ctx).into_iter().next();
            text(
                first
                    .map(|i| match name {
                        "name" => ctx.name(&i),
                        "local-name" => ctx.local_name(&i),
                        _ => ctx.namespace_uri(&i),
                    })
                    .unwrap_or_default()
                    .to_string(),
            )
        }
        "string" => {
            arity(0, 1)?;
            text(if args.is_empty() {
                ctx.string_value(&ctx.item).into_owned()
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
            let start = round(number(arg(1)?, ctx));
            let end = if args.len() == 3 {
                start + round(number(arg(2)?, ctx))
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
                ctx.string_value(&ctx.item).into_owned()
            } else {
                str_arg(0)?
            };
            Value::Num(s.chars().count() as f64)
        }
        "normalize-space" => {
            arity(0, 1)?;
            let s = if args.is_empty() {
                ctx.string_value(&ctx.item).into_owned()
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
                Item::Attr { owner, .. } | Item::Ns { owner, .. } => owner,
            };
            let tree = ctx.doc.tree;
            // A climb per call: counted, as an ancestor axis is.
            let mut declared = None;
            let mut at = Some(node);
            while let Some(n) = at {
                ctx.spend(1)?;
                declared = tree.lang(n);
                if declared.is_some() {
                    break;
                }
                at = tree.parent(n);
            }
            let declared = declared.map(str::to_ascii_lowercase);
            Value::Bool(declared.is_some_and(|l| l == want || l.starts_with(&format!("{want}-"))))
        }
        "number" => {
            arity(0, 1)?;
            Value::Num(if args.is_empty() {
                parse_number(&ctx.string_value(&ctx.item))
            } else {
                number(arg(0)?, ctx)
            })
        }
        "sum" => {
            arity(1, 1)?;
            // Fold from 0.0: Rust's f64 sum() of nothing is -0.0.
            Value::Num(
                nodes_arg(0)?
                    .iter()
                    .map(|i| parse_number(&ctx.string_value(i)))
                    .fold(0.0, |a, b| a + b),
            )
        }
        "floor" => {
            arity(1, 1)?;
            Value::Num(number(arg(0)?, ctx).floor())
        }
        "ceiling" => {
            arity(1, 1)?;
            Value::Num(number(arg(0)?, ctx).ceil())
        }
        "round" => {
            arity(1, 1)?;
            Value::Num(round(number(arg(0)?, ctx)))
        }
        _ => {
            let args = Args {
                name,
                exprs: args,
                ctx,
            };
            return ctx
                .ext
                .call(name, &args)
                .unwrap_or_else(|| Err(format!("unknown function {name}()")));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
