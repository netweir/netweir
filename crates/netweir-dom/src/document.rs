use std::cell::{Cell, UnsafeCell};
use std::ffi::{c_char, c_int, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::ffi::{self, RawDocument, RawNode, str_at};
use crate::index::Index;

/// A parsed HTML document.
///
/// Built by lexbor following the HTML5 spec, so a page parses the way a
/// browser would parse it: missing `<html>`, `<head>`, `<body>` and `<tbody>`
/// elements are added, and misnested tags are repaired.
pub struct Document {
    raw: *mut RawDocument,
    /// Selector runs take this lock. lexbor does not promise that its matcher
    /// is safe with concurrent readers of one document, so we don't assume it.
    pub(crate) select_lock: Mutex<()>,
    /// A flat, document-order index of every node, built on first use and
    /// dropped by every edit (which has the document to itself).
    index: UnsafeCell<OnceLock<Index>>,
    /// Nodes an edit left without a parent: removed ones, and new ones not
    /// yet placed. The index covers their subtrees too, so a handle to one
    /// keeps working. Pruned whenever the index is built.
    detached: Mutex<Vec<usize>>,
    /// Readers holding a [`Reading`], or `WRITING` while an edit runs;
    /// `WAITING` while an edit waits for readers to finish.
    access: AtomicU32,
    /// Readers held back by an edit (running, or waiting and deferred to):
    /// the high half counts edits finished, the low half the readers held
    /// back since the last one finished. Finishing an edit takes the count
    /// and starts a new one, so a turn is for exactly those readers.
    queue: AtomicU64,
    /// Readers of the current turn not yet in.
    turn_left: AtomicU32,
    /// Readers a turn let in past a waiting edit (not counting ones whose
    /// patience ran out), and the most any turn let in.
    #[cfg(test)]
    turn_entries: AtomicU32,
    #[cfg(test)]
    most_turn_entries: AtomicU32,
    /// Wake-ups sent, so a test can tell a sleep nobody woke from one that
    /// outlasted a slow reader.
    #[cfg(test)]
    wakes: AtomicU32,
    /// Where a waiting edit sleeps until the last reader wakes it.
    gate: Mutex<()>,
    woken: Condvar,
}

const WRITING: u32 = 1 << 31;
const WAITING: u32 = 1 << 30;
/// Set by an edit that finished with readers held back: they go next, and
/// no edit starts until the last of them is in. Readers arriving later wait
/// for the next turn. Without it, an editor working back to back takes the
/// document again before a yielding reader wakes.
const TURN: u32 = 1 << 29;
const READERS: u32 = TURN - 1;

/// How many times a new reader yields to a waiting edit before reading
/// anyway. Without it, readers that follow one another closely could keep
/// an edit out forever; with no limit, a reader that holds Python's GIL
/// could wait for an edit that waits for a reader needing the GIL.
const PATIENCE: u32 = 2000;

// SAFETY: reading never changes the tree; selector matching, which keeps
// state in lexbor, is serialised by `select_lock`. Edits change it, and
// their callers promise that other threads reading at the time hold a
// `Reading`, which an edit waits for. No node is freed before the
// document is, so a pointer into it never dangles.
unsafe impl Send for Document {}
unsafe impl Sync for Document {}

thread_local! {
    /// How many `Reading`s this thread holds, of any document. An edit
    /// from inside one fails: on the same document it would wait for
    /// itself, and on another, two threads doing it crosswise would wait
    /// for each other. A plain counter in thread-local storage, so threads
    /// never touch each other's memory.
    static HELD: Cell<u32> = const { Cell::new(0) };
}

/// Holds off edits to a document while it lives. See [`Document::read`].
pub struct Reading<'a> {
    doc: &'a Document,
    // The thread-local bookkeeping must be undone on the same thread.
    _not_send: PhantomData<*const ()>,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        let before = self.doc.access.fetch_sub(1, Ordering::Release);
        HELD.with(|h| h.set(h.get() - 1));
        // The last reader out wakes an edit waiting for it.
        if before & WAITING != 0 && before & READERS == 1 {
            self.doc.wake();
        }
    }
}

/// Why an edit wasn't made. Nothing changes when one fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The edit makes no sense on these nodes, or a name isn't valid.
    Invalid(String),
    /// This thread holds a [`Reading`] of the document, so editing would
    /// wait for itself.
    Reading,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::Invalid(why) => f.write_str(why),
            EditError::Reading => {
                f.write_str("can't edit a document while this thread is running a query")
            }
        }
    }
}

impl std::error::Error for EditError {}

/// What an edit puts in the tree: a node of the document, or new text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content<'s> {
    Node(NodeId),
    Text(&'s str),
}

impl From<NodeId> for Content<'_> {
    fn from(id: NodeId) -> Self {
        Content::Node(id)
    }
}

fn invalid<T>(why: impl Into<String>) -> Result<T, EditError> {
    Err(EditError::Invalid(why.into()))
}

/// Elements written without an end tag.
const VOID: [&str; 18] = [
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

impl Document {
    pub fn parse(html: &str) -> Document {
        Document::parse_within(html, Duration::MAX).expect("an unlimited budget cannot run out")
    }

    /// Like [`Document::parse`], but gives up once `budget` has passed.
    ///
    /// The HTML5 spec makes some inputs, such as thousands of unclosed
    /// `<div>`s, take time quadratic in their nesting depth. A 400 KB page of
    /// them takes seconds. Browsers cap the depth; lexbor does not. A crawler
    /// fetching pages it does not control should parse with a budget.
    pub fn parse_within(html: &str, budget: Duration) -> Result<Document, ParseTimeout> {
        // Known limit: the budget is checked between 16 KB chunks, so a parse can
        // overrun it by the time one chunk takes. It bounds time, not memory:
        // a crafted page of distinct formatting tags makes the HTML5 adoption
        // agency clone thousands of elements per paragraph (64 KB reached
        // 6.7 GB in a test). A node cap checked between chunks comes with the
        // crawler.
        parse_in_chunks(html, budget, 16 * 1024)
    }

    /// The document's index. Building it writes each node's `user` field
    /// once, inside the OnceLock, before any reader can see the result.
    pub(crate) fn index(&self) -> &Index {
        // SAFETY: only an edit replaces the OnceLock, and an edit runs with
        // no reader on another thread and none of this crate's code holding
        // the reference on this one.
        unsafe { &*self.index.get() }.get_or_init(|| Index::build(&self.roots()))
    }

    /// The document node, then every node that is the top of a subtree
    /// outside it.
    fn roots(&self) -> Vec<*mut RawNode> {
        let root = unsafe { ffi::nw_root(self.raw) };
        let mut detached = self.detached.lock().unwrap_or_else(|e| e.into_inner());
        detached.sort_unstable();
        detached.dedup();
        // A node put back in a tree is indexed with it.
        detached.retain(|&n| unsafe { ffi::nw_parent(n as *mut RawNode) }.is_null());
        std::iter::once(root)
            .chain(detached.iter().map(|&n| n as *mut RawNode))
            .collect()
    }

    fn order_of(&self, raw: *mut RawNode) -> u32 {
        self.index();
        unsafe { ffi::nw_order(raw) as u32 }
    }

    /// The document node. Its children are the doctype and `<html>`.
    pub fn root(&self) -> Node<'_> {
        Node {
            doc: self,
            raw: unsafe { ffi::nw_root(self.raw) },
        }
    }

    /// Holds off edits until the returned value is dropped. A thread that
    /// reads the document while another may edit it must hold one; reading
    /// on the thread that edits needs none. Cheap: one atomic add, and no
    /// waiting unless an edit is running.
    pub fn read(&self) -> Reading<'_> {
        // A read nested in one this thread holds never waits: an edit it
        // would let in may be waiting for the outer read to end.
        let nested = HELD.with(|h| h.get()) > 0;
        let mut patience = if nested { 0 } else { PATIENCE };
        // The edit count when this reader was held back, if it was.
        let mut ticket: Option<u32> = None;
        loop {
            let now = self.access.load(Ordering::Relaxed);
            // Let a waiting edit go first, unless this reader's turn has
            // come: it was held back before the last edit finished.
            let my_turn =
                now & TURN != 0 && ticket.is_some_and(|t| self.edits_done() == t.wrapping_add(1));
            let defer = now & WAITING != 0 && !my_turn && patience > 0;
            if now & WRITING == 0
                && !defer
                && self
                    .access
                    .compare_exchange_weak(now, now + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                // Counted unless the reader's patience ran out, which is
                // the deliberate limit on how long any reader yields.
                #[cfg(test)]
                if now & TURN != 0 && now & WAITING != 0 && patience > 0 {
                    let n = self.turn_entries.fetch_add(1, Ordering::Relaxed) + 1;
                    self.most_turn_entries.fetch_max(n, Ordering::Relaxed);
                }
                break;
            }
            // Losing a race to another reader isn't being held back.
            if ticket.is_none() && !nested && (now & WRITING != 0 || defer) {
                ticket = Some((self.queue.fetch_add(1, Ordering::Relaxed) >> 32) as u32);
            }
            // An edit is quick and needs nothing from this thread.
            patience = patience.saturating_sub(1);
            std::thread::yield_now();
        }
        // No edit can finish while this reader is in, so the count can't
        // change under these two steps.
        if let Some(t) = ticket {
            if self.edits_done() == t {
                // In before any edit ran: no longer held back.
                self.queue.fetch_sub(1, Ordering::Relaxed);
            } else if self.turn_left.fetch_sub(1, Ordering::Relaxed) == 1 {
                // The last of the turn ends it, and wakes an edit that
                // slept through it.
                let before = self.access.fetch_and(!TURN, Ordering::Relaxed);
                if before & WAITING != 0 {
                    self.wake();
                }
            }
        }
        HELD.with(|h| h.set(h.get() + 1));
        Reading {
            doc: self,
            _not_send: PhantomData,
        }
    }

    /// Runs `edit` with the document to itself, then drops the index, which
    /// the next query rebuilds.
    fn edit<R>(&self, edit: impl FnOnce() -> Result<R, EditError>) -> Result<R, EditError> {
        if HELD.with(|h| h.get()) > 0 {
            return Err(EditError::Reading);
        }
        let mut tries = 0u32;
        loop {
            // Ask new readers to hold back, then take the document once
            // the ones reading are done.
            let now = self.access.fetch_or(WAITING, Ordering::Relaxed) | WAITING;
            if now == WAITING
                && self
                    .access
                    .compare_exchange_weak(now, WRITING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                break;
            }
            // Readers can take a while (a find_all calling a Python
            // function per node), so after a few yields, sleep until the
            // last one wakes us. A condition variable rather than a timed
            // sleep: Windows rounds short sleeps up to a millisecond or more.
            tries += 1;
            if tries < 64 {
                std::thread::yield_now();
                continue;
            }
            let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            // A reader leaving after this check has to take `gate` to wake
            // us, which it can only do once we're waiting.
            // Set WAITING again rather than only looking: an edit that got
            // in first cleared it, and then whoever leaves next (that edit,
            // or the last reader) wouldn't know to wake us.
            if self.access.fetch_or(WAITING, Ordering::Relaxed) & !WAITING != 0 {
                #[cfg(test)]
                let wakes = self.wakes.load(Ordering::Relaxed);
                // The timeout only covers a wake-up this reasoning missed.
                let (_gate, waited) = self
                    .woken
                    .wait_timeout(gate, Duration::from_millis(1))
                    .unwrap_or_else(|e| e.into_inner());
                #[cfg(test)]
                let _gate = self.check_wake(_gate, waited.timed_out(), wakes);
                #[cfg(not(test))]
                let _ = waited;
            }
        }
        struct Done<'a>(&'a Document);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                // SAFETY: this thread has the document to itself.
                unsafe { *self.0.index.get() = OnceLock::new() };
                // Readers held back while this edit ran or waited get the
                // next turn; later ones count towards the turn after. A
                // WAITING bit set by another edit meanwhile is kept, so
                // whoever ends the turn knows to wake it.
                let queue = &self.0.queue;
                let mut q = queue.load(Ordering::Relaxed);
                let next = |q: u64| u64::from(((q >> 32) as u32).wrapping_add(1)) << 32;
                while let Err(now) =
                    queue.compare_exchange_weak(q, next(q), Ordering::Relaxed, Ordering::Relaxed)
                {
                    q = now;
                }
                let held = q as u32;
                self.0.turn_left.store(held, Ordering::Relaxed);
                let turn = if held > 0 { TURN } else { 0 };
                #[cfg(test)]
                self.0.turn_entries.store(0, Ordering::Relaxed);
                let before = self.0.access.fetch_xor(WRITING | turn, Ordering::Release);
                if before & WAITING != 0 && turn == 0 {
                    self.0.wake();
                }
            }
        }
        let _done = Done(self);
        edit()
    }

    /// After a sleep: counts it, and if it ran out, whether it was a lost
    /// wake-up. A sleep outlasting a reader that holds the page is expected
    /// (a busy machine deschedules readers). A lost one is a sleep that ran
    /// out with the way clear (no reader in, no turn, no edit) and no wake
    /// sent since it began. A reader that has just left may be waiting for
    /// `gate` to send its wake, so the sleep is given 100 ms more for it
    /// first.
    #[cfg(test)]
    fn check_wake<'g>(
        &self,
        mut gate: std::sync::MutexGuard<'g, ()>,
        timed_out: bool,
        wakes: u32,
    ) -> std::sync::MutexGuard<'g, ()> {
        tests::WAITS.fetch_add(1, Ordering::Relaxed);
        if !timed_out {
            return gate;
        }
        tests::TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        let clear =
            |d: &Document| d.access.load(Ordering::Relaxed) & (READERS | TURN | WRITING) == 0;
        if !clear(self) || self.wakes.load(Ordering::Relaxed) != wakes {
            return gate;
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        while self.wakes.load(Ordering::Relaxed) == wakes {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tests::LOST.fetch_add(1, Ordering::Relaxed);
                break;
            }
            gate = self
                .woken
                .wait_timeout(gate, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        gate
    }

    fn edits_done(&self) -> u32 {
        (self.queue.load(Ordering::Relaxed) >> 32) as u32
    }

    fn wake(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        self.wakes.fetch_add(1, Ordering::Relaxed);
        self.woken.notify_all();
    }

    fn mark_detached(&self, n: *mut RawNode) {
        self.detached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n as usize);
    }

    /// Takes `node` (and everything in it) out of its tree. A handle to
    /// it, or to anything in it, keeps working on the removed subtree,
    /// which queries from the document no longer see. Its memory is freed
    /// with the document. Removing a node that has no parent does nothing.
    ///
    /// # Safety
    /// This and every other edit needs ids from this document, and any
    /// other thread reading the document meanwhile must hold a
    /// [`Document::read`]. Edits wait for those to be dropped.
    pub unsafe fn remove(&self, node: NodeId) -> Result<(), EditError> {
        let n = node.raw();
        self.edit(|| {
            if n == unsafe { ffi::nw_root(self.raw) } {
                return invalid("the document itself can't be removed");
            }
            if !unsafe { ffi::nw_parent(n) }.is_null() {
                unsafe { ffi::nw_remove(n) };
                self.mark_detached(n);
            }
            Ok(())
        })
    }

    /// Puts `items`, in order, into `parent`: at position `at` among its
    /// children (counting text; negative counts from the end; past either
    /// end is the start or the end), or at the end. A node that is in a
    /// tree is taken out of it first. The position is found inside the
    /// edit, so no other thread's edit can move it in between.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn insert<'s, C: Copy + Into<Content<'s>>>(
        &self,
        parent: NodeId,
        at: Option<isize>,
        items: &[C],
    ) -> Result<(), EditError> {
        let p = parent.raw();
        self.edit(|| {
            let before = at.and_then(|at| {
                let children: Vec<*mut RawNode> = std::iter::successors(
                    Some(unsafe { ffi::nw_first_child(p) }).filter(|c| !c.is_null()),
                    |&c| Some(unsafe { ffi::nw_next(c) }).filter(|c| !c.is_null()),
                )
                .collect();
                let i = if at < 0 {
                    at + children.len() as isize
                } else {
                    at
                };
                children.get(i.max(0) as usize).copied()
            });
            unsafe { self.link(p, before, items) }
        })
    }

    /// Puts `with` where `node` is, and removes `node`.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn replace<'s, C: Copy + Into<Content<'s>>>(
        &self,
        node: NodeId,
        with: &[C],
    ) -> Result<(), EditError> {
        let n = node.raw();
        self.edit(|| {
            let parent = unsafe { ffi::nw_parent(n) };
            if parent.is_null() {
                return invalid("a node with no parent can't be replaced");
            }
            if with.iter().any(|&c| c.into() == Content::Node(node)) {
                return invalid("a node can't be replaced with itself");
            }
            unsafe { self.link(parent, Some(n), with)? };
            unsafe { ffi::nw_remove(n) };
            self.mark_detached(n);
            Ok(())
        })
    }

    /// Replaces `node` with its children, leaving it removed and empty.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn unwrap(&self, node: NodeId) -> Result<(), EditError> {
        let n = node.raw();
        self.edit(|| {
            let parent = unsafe { ffi::nw_parent(n) };
            if parent.is_null() {
                return invalid("a node with no parent can't be unwrapped");
            }
            let mut child = unsafe { ffi::nw_first_child(n) };
            while !child.is_null() {
                let next = unsafe { ffi::nw_next(child) };
                unsafe {
                    ffi::nw_remove(child);
                    ffi::nw_insert_before(n, child);
                }
                child = next;
            }
            unsafe { ffi::nw_remove(n) };
            self.mark_detached(n);
            Ok(())
        })
    }

    /// Replaces everything in the element `node` with one text node.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn set_text(&self, node: NodeId, text: &str) -> Result<(), EditError> {
        let n = node.raw();
        self.edit(|| {
            if unsafe { ffi::nw_type(n) } != ffi::ELEMENT {
                return invalid("only an element's text can be set");
            }
            let tag = Node::from_raw(self, n).tag().unwrap_or_default();
            if VOID.contains(&tag) {
                return invalid(format!("<{tag}> can't hold text"));
            }
            let t = self.new_text(text);
            loop {
                let child = unsafe { ffi::nw_first_child(n) };
                if child.is_null() {
                    break;
                }
                unsafe { ffi::nw_remove(child) };
                self.mark_detached(child);
            }
            unsafe { ffi::nw_append(n, t) };
            Ok(())
        })
    }

    /// A new element, not yet in the tree: place it with
    /// [`Document::insert`]. Names are lowercased.
    // Known limit: attributes are only ever set here, on an element nothing
    // has borrowed from yet. Changing one on an existing element would
    // rewrite memory a `&str` from `attr()` may still point at.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn create_element(
        &self,
        tag: &str,
        attrs: &[(&str, &str)],
    ) -> Result<NodeId, EditError> {
        let valid_tag = tag.starts_with(|c: char| c.is_ascii_alphabetic())
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
        if !valid_tag {
            return invalid(format!("{tag:?} isn't a valid tag name"));
        }
        // What the HTML spec allows in an attribute name.
        let bad_attr = attrs.iter().find(|(name, _)| {
            name.is_empty()
                || name.chars().any(|c| {
                    c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '>' | '/' | '=')
                })
        });
        if let Some((name, _)) = bad_attr {
            return invalid(format!("{name:?} isn't a valid attribute name"));
        }
        let tag = tag.to_ascii_lowercase();
        self.edit(|| {
            let n = unsafe { ffi::nw_create_element(self.raw, tag.as_ptr().cast(), tag.len()) };
            assert!(!n.is_null(), "lexbor failed to allocate an element");
            for (name, value) in attrs {
                let name = name.to_ascii_lowercase();
                let status = unsafe {
                    ffi::nw_set_attr(
                        n,
                        name.as_ptr().cast(),
                        name.len(),
                        value.as_ptr().cast(),
                        value.len(),
                    )
                };
                assert_eq!(status, 0, "lexbor failed to allocate an attribute");
            }
            self.mark_detached(n);
            Ok(NodeId(n as usize))
        })
    }

    /// A new text node, not yet in the tree.
    ///
    /// # Safety
    /// As for [`Document::remove`].
    pub unsafe fn create_text(&self, text: &str) -> Result<NodeId, EditError> {
        self.edit(|| {
            let n = self.new_text(text);
            self.mark_detached(n);
            Ok(NodeId(n as usize))
        })
    }

    /// A text node with no parent. Only an edit may call it.
    fn new_text(&self, text: &str) -> *mut RawNode {
        let n = unsafe { ffi::nw_create_text(self.raw, text.as_ptr().cast(), text.len()) };
        assert!(!n.is_null(), "lexbor failed to allocate a text node");
        n
    }

    /// Puts `items` into `parent` before its child `before` (or at the
    /// end), without taking the document. Checks every node first and
    /// makes the text last, so a failure changes nothing.
    unsafe fn link<'s, C: Copy + Into<Content<'s>>>(
        &self,
        parent: *mut RawNode,
        before: Option<*mut RawNode>,
        items: &[C],
    ) -> Result<(), EditError> {
        if !matches!(
            unsafe { ffi::nw_type(parent) },
            ffi::ELEMENT | ffi::DOCUMENT
        ) {
            return invalid("only an element or the document can have children");
        }
        if before.is_some_and(|b| unsafe { ffi::nw_parent(b) } != parent) {
            return invalid("the node to insert before isn't a child of the parent");
        }
        let given = || {
            items.iter().filter_map(|&c| match c.into() {
                Content::Node(id) => Some(id.raw()),
                Content::Text(_) => None,
            })
        };
        for n in given() {
            if !matches!(
                unsafe { ffi::nw_type(n) },
                ffi::ELEMENT | ffi::TEXT | ffi::CDATA | ffi::COMMENT
            ) {
                return invalid("only elements, text and comments can be inserted");
            }
            // A node can't go inside itself.
            let mut up = parent;
            while !up.is_null() {
                if up == n {
                    return invalid("a node can't be put inside itself");
                }
                up = unsafe { ffi::nw_parent(up) };
            }
        }
        // Inserting before a node that is itself being moved means before
        // the first one after it that isn't.
        let mut before = before;
        while let Some(b) = before.filter(|b| given().any(|n| n == *b)) {
            before = Some(unsafe { ffi::nw_next(b) }).filter(|n| !n.is_null());
        }
        for &item in items {
            let n = match item.into() {
                Content::Node(id) => id.raw(),
                Content::Text(text) => self.new_text(text),
            };
            unsafe {
                if !ffi::nw_parent(n).is_null() {
                    ffi::nw_remove(n);
                }
                match before {
                    Some(b) => ffi::nw_insert_before(b, n),
                    None => ffi::nw_append(parent, n),
                }
            }
        }
        Ok(())
    }

    /// Rebuilds a node from an id returned by [`Node::id`].
    ///
    /// # Safety
    /// `id` must come from a node of this same document.
    pub unsafe fn node(&self, id: NodeId) -> Node<'_> {
        Node {
            doc: self,
            raw: id.0 as *mut RawNode,
        }
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        unsafe { ffi::nw_destroy(self.raw) }
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Document")
    }
}

fn parse_in_chunks(html: &str, budget: Duration, chunk: usize) -> Result<Document, ParseTimeout> {
    let start = Instant::now();
    let raw = unsafe { ffi::nw_chunk_begin() };
    // lexbor only fails when it cannot allocate. Rust aborts on allocation
    // failure, so netweir treats lexbor's the same way.
    assert!(!raw.is_null(), "lexbor failed to allocate a document");
    let doc = Document {
        raw,
        select_lock: Mutex::new(()),
        index: UnsafeCell::new(OnceLock::new()),
        detached: Mutex::new(Vec::new()),
        access: AtomicU32::new(0),
        queue: AtomicU64::new(0),
        turn_left: AtomicU32::new(0),
        #[cfg(test)]
        turn_entries: AtomicU32::new(0),
        #[cfg(test)]
        most_turn_entries: AtomicU32::new(0),
        #[cfg(test)]
        wakes: AtomicU32::new(0),
        gate: Mutex::new(()),
        woken: Condvar::new(),
    };
    let mut rest = html;
    while !rest.is_empty() {
        if start.elapsed() > budget {
            return Err(ParseTimeout {
                budget,
                parsed: html.len() - rest.len(),
            });
        }
        let mut cut = rest.len().min(chunk);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (head, tail) = rest.split_at(cut);
        let status = unsafe { ffi::nw_chunk(doc.raw, head.as_ptr().cast(), head.len()) };
        assert_eq!(status, 0, "lexbor failed to allocate while parsing");
        rest = tail;
    }
    assert_eq!(
        unsafe { ffi::nw_chunk_end(doc.raw, has_template(html) as c_int) },
        0,
        "lexbor failed to allocate while parsing"
    );
    Ok(doc)
}

/// Whether `html` has a `<template` tag, in any case: the only way a
/// template gets into a parsed document.
fn has_template(html: &str) -> bool {
    // Faster than checking every `<`, of which a page has hundreds of
    // thousands; `<te` in each case is rare.
    let bytes = html.as_bytes();
    [b"<te", b"<tE", b"<Te", b"<TE"].iter().any(|start| {
        memchr::memmem::find_iter(bytes, start).any(|i| {
            bytes
                .get(i + 1..i + 9)
                .is_some_and(|name| name.eq_ignore_ascii_case(b"template"))
        })
    })
}

/// Returned by [`Document::parse_within`] when parsing ran out of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTimeout {
    pub budget: Duration,
    /// Bytes parsed before giving up.
    pub parsed: usize,
}

impl fmt::Display for ParseTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parsing took longer than {:?} (gave up after {} bytes)",
            self.budget, self.parsed
        )
    }
}

impl std::error::Error for ParseTimeout {}

/// A node's identity, stable for the life of its document. Lets code that
/// cannot hold a borrow (such as the Python bindings) refer to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

impl NodeId {
    fn raw(self) -> *mut RawNode {
        self.0 as *mut RawNode
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Doctype,
    Element,
    Text,
    Comment,
    Other,
}

#[derive(Clone, Copy)]
pub struct Node<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) raw: *mut RawNode,
}

impl<'a> Node<'a> {
    pub(crate) fn from_raw(doc: &'a Document, raw: *mut RawNode) -> Node<'a> {
        Node { doc, raw }
    }

    pub(crate) fn wrap(doc: &'a Document, raw: *mut RawNode) -> Option<Node<'a>> {
        (!raw.is_null()).then_some(Node { doc, raw })
    }

    pub fn id(&self) -> NodeId {
        NodeId(self.raw as usize)
    }

    pub fn kind(&self) -> NodeKind {
        match unsafe { ffi::nw_type(self.raw) } {
            ffi::ELEMENT => NodeKind::Element,
            ffi::TEXT | ffi::CDATA => NodeKind::Text,
            ffi::COMMENT => NodeKind::Comment,
            ffi::DOCUMENT => NodeKind::Document,
            ffi::DOCTYPE => NodeKind::Doctype,
            _ => NodeKind::Other,
        }
    }

    /// The lowercase tag name, for elements.
    pub fn tag(&self) -> Option<&'a str> {
        if self.kind() != NodeKind::Element {
            return None;
        }
        let mut len = 0;
        Some(unsafe { str_at(ffi::nw_tag(self.raw, &mut len), len) })
    }

    pub fn attr(&self, name: &str) -> Option<&'a str> {
        if self.kind() != NodeKind::Element {
            return None;
        }
        let mut len = 0;
        let ptr = unsafe { ffi::nw_get_attr(self.raw, name.as_ptr().cast(), name.len(), &mut len) };
        (!ptr.is_null()).then(|| unsafe { str_at(ptr, len) })
    }

    /// Every attribute as `(name, value)`, in source order.
    pub fn attrs(&self) -> Vec<(&'a str, &'a str)> {
        let mut out = Vec::new();
        if self.kind() != NodeKind::Element {
            return out;
        }
        let mut a = unsafe { ffi::nw_first_attr(self.raw) };
        while !a.is_null() {
            let (mut nl, mut vl) = (0, 0);
            let name = unsafe { str_at(ffi::nw_attr_name(a, &mut nl), nl) };
            let value = unsafe { str_at(ffi::nw_attr_value(a, &mut vl), vl) };
            out.push((name, value));
            a = unsafe { ffi::nw_next_attr(a) };
        }
        out
    }

    /// The node's own text, for text and comment nodes.
    pub fn data(&self) -> Option<&'a str> {
        match self.kind() {
            NodeKind::Text | NodeKind::Comment => {
                let mut len = 0;
                Some(unsafe { str_at(ffi::nw_char_data(self.raw, &mut len), len) })
            }
            _ => None,
        }
    }

    /// lexbor's id for this node's name. Two elements share an id exactly
    /// when their lowercase names are equal.
    pub(crate) fn tag_id(&self) -> usize {
        unsafe { ffi::nw_tag_id(self.raw) }
    }

    /// The id elements named `name` (compared lowercased) carry in this
    /// node's document; `None` if no element has that name.
    pub(crate) fn tag_id_named(&self, name: &str) -> Option<usize> {
        let id = unsafe { ffi::nw_tag_id_named(self.raw, name.as_ptr().cast(), name.len()) };
        (id != 0).then_some(id)
    }

    pub fn next_sibling(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_next(self.raw) })
    }

    pub fn prev_sibling(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_prev(self.raw) })
    }

    pub fn first_child(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_first_child(self.raw) })
    }

    pub fn last_child(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_last_child(self.raw) })
    }

    /// The node after this one in document order: its first child, or the
    /// next sibling of it or its nearest ancestor that has one.
    pub fn next_in_order(&self) -> Option<Node<'a>> {
        if let Some(child) = self.first_child() {
            return Some(child);
        }
        let mut at = *self;
        loop {
            if let Some(next) = at.next_sibling() {
                return Some(next);
            }
            at = at.parent()?;
        }
    }

    /// The node before this one in document order: the last node inside its
    /// previous sibling, or else its parent.
    pub fn prev_in_order(&self) -> Option<Node<'a>> {
        let Some(mut at) = self.prev_sibling() else {
            return self.parent();
        };
        while let Some(last) = at.last_child() {
            at = last;
        }
        Some(at)
    }

    /// The node's position in document order: smaller comes first.
    pub fn order(&self) -> u32 {
        self.doc.order_of(self.raw)
    }

    /// The node as HTML: an element with its attributes and everything in
    /// it, a text node escaped, a comment as `<!--...-->`.
    pub fn html(&self) -> String {
        extern "C" fn push(data: *const c_char, len: usize, ctx: *mut c_void) {
            // SAFETY: lexbor passes `len` valid bytes; ctx is the Vec below.
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
            unsafe { (*ctx.cast::<Vec<u8>>()).extend_from_slice(bytes) }
        }
        let mut out: Vec<u8> = Vec::new();
        // Only fails when lexbor cannot allocate; return what was written.
        unsafe { ffi::nw_serialize(self.raw, push, (&mut out as *mut Vec<u8>).cast()) };
        // lexbor writes UTF-8; anything else would be memory corruption.
        String::from_utf8(out).unwrap_or_default()
    }

    /// The node alone as HTML: an element's start tag, escaped text, a
    /// comment or a doctype.
    fn html_alone(&self) -> String {
        extern "C" fn push(data: *const c_char, len: usize, ctx: *mut c_void) {
            // SAFETY: lexbor passes `len` valid bytes; ctx is the Vec below.
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
            unsafe { (*ctx.cast::<Vec<u8>>()).extend_from_slice(bytes) }
        }
        let mut out: Vec<u8> = Vec::new();
        unsafe { ffi::nw_serialize_one(self.raw, push, (&mut out as *mut Vec<u8>).cast()) };
        String::from_utf8(out).unwrap_or_default()
    }

    /// The node as indented HTML, one tag or piece of text to a line, one
    /// space of indent a level, as Beautiful Soup's `prettify()` lays it
    /// out. Text is trimmed and whitespace-only text left out, except in
    /// `<pre>`, `<textarea>`, `<script>` and `<style>`, which are written
    /// as they are.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        self.pretty_into(0, &mut out);
        out
    }

    fn pretty_into(&self, depth: usize, out: &mut String) {
        // Where whitespace matters, or the content is raw text.
        const AS_IS: [&str; 9] = [
            "pre",
            "textarea",
            "script",
            "style",
            "xmp",
            "iframe",
            "noembed",
            "noframes",
            "plaintext",
        ];
        let line = |out: &mut String, text: &str| {
            out.extend(std::iter::repeat_n(' ', depth));
            out.push_str(text);
            out.push('\n');
        };
        match (self.kind(), self.tag()) {
            (NodeKind::Document, _) => {
                for child in self.children() {
                    child.pretty_into(depth, out);
                }
            }
            (NodeKind::Element, Some(tag)) if AS_IS.contains(&tag) => line(out, &self.html()),
            (NodeKind::Element, Some(tag)) => {
                let start = self.html_alone();
                line(out, &start);
                if VOID.contains(&tag) {
                    return;
                }
                for child in self.children() {
                    child.pretty_into(depth + 1, out);
                }
                // The name as the start tag has it: `tag()` lowercases
                // SVG's `foreignObject`.
                let name = start[1..]
                    .split(|c: char| c.is_ascii_whitespace() || c == '>')
                    .next()
                    .unwrap_or(tag);
                line(out, &format!("</{name}>"));
            }
            (NodeKind::Text, _) => {
                let text = self.html_alone();
                if !text.trim().is_empty() {
                    line(out, text.trim());
                }
            }
            _ => line(out, &self.html_alone()),
        }
    }

    pub fn parent(&self) -> Option<Node<'a>> {
        Node::wrap(self.doc, unsafe { ffi::nw_parent(self.raw) })
    }

    pub fn children(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let doc = self.doc;
        std::iter::successors(
            Node::wrap(doc, unsafe { ffi::nw_first_child(self.raw) }),
            move |n| Node::wrap(doc, unsafe { ffi::nw_next(n.raw) }),
        )
    }

    /// Every node below this one, in document order.
    pub fn descendants(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let doc = self.doc;
        let top = self.raw;
        let mut next = unsafe { ffi::nw_first_child(top) };
        std::iter::from_fn(move || {
            let current = Node::wrap(doc, next)?;
            next = unsafe { ffi::nw_first_child(current.raw) };
            let mut up = current.raw;
            while next.is_null() && up != top {
                next = unsafe { ffi::nw_next(up) };
                up = unsafe { ffi::nw_parent(up) };
                // An edit on this thread took `current` out of the tree
                // mid-walk: stop rather than climb past its old place.
                if up.is_null() {
                    break;
                }
            }
            Some(current)
        })
    }

    /// The text nodes below this node, in order, leaving out what is code
    /// rather than reading matter: the contents of `<script>`, `<style>`
    /// and `<template>` (Beautiful Soup's `get_text()` does the same). On a
    /// script or style element itself, gives its own text.
    pub fn readable_text_pieces(&self) -> Vec<&'a str> {
        const CODE: [&str; 3] = ["script", "style", "template"];
        if self.tag().is_some_and(|t| CODE.contains(&t)) || self.kind() == NodeKind::Text {
            return self.text_pieces();
        }
        let mut out = Vec::new();
        let mut next = self.first_child();
        while let Some(n) = next {
            let skip = n.tag().is_some_and(|t| CODE.contains(&t));
            if !skip && n.kind() == NodeKind::Text {
                out.extend(n.data());
            }
            // Descend unless skipping; otherwise move on past this subtree.
            next = if !skip { n.first_child() } else { None }.or_else(|| {
                let mut at = n;
                loop {
                    if at.raw == self.raw {
                        return None;
                    }
                    if let Some(s) = at.next_sibling() {
                        return Some(s);
                    }
                    at = at.parent()?;
                    if at.raw == self.raw {
                        return None;
                    }
                }
            });
        }
        out
    }

    /// Every text node's text below (or at) this node, in order.
    fn text_pieces(&self) -> Vec<&'a str> {
        std::iter::once(*self)
            .chain(self.descendants())
            .filter(|n| n.kind() == NodeKind::Text)
            .filter_map(|n| n.data())
            .collect()
    }

    /// All text below this node, joined. Comments are not text.
    pub fn text(&self) -> String {
        if let (NodeKind::Text, Some(d)) = (self.kind(), self.data()) {
            return d.to_string();
        }
        self.descendants()
            .filter(|n| n.kind() == NodeKind::Text)
            .filter_map(|n| n.data())
            .collect()
    }
}

impl PartialEq for Node<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            NodeKind::Element => write!(f, "<{}>", self.tag().unwrap_or("?")),
            NodeKind::Text => write!(f, "{:?}", self.data().unwrap_or("")),
            kind => write!(f, "{kind:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// Edits whose sleep ran out instead of being woken.
    pub(super) static TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
    /// Edits that slept at all, so the test knows it tested something.
    pub(super) static WAITS: AtomicUsize = AtomicUsize::new(0);
    /// Sleeps that ran out with the way clear and no wake-up ever sent.
    pub(super) static LOST: AtomicUsize = AtomicUsize::new(0);

    /// Several edits waiting at once, against short and long reads: each
    /// must be woken, never left to sleep out its timeout.
    /// Readers arriving in a stream, each read a new one, against an edit
    /// waiting to get in: a turn lets in at most the readers that were
    /// waiting when it began (one a thread), never the newcomers after them,
    /// so the edit's wait has a bound.
    #[test]
    fn a_readers_turn_admits_only_the_readers_already_waiting() {
        const READERS: u32 = 8;
        let doc = Arc::new(Document::parse(&"<p>x</p>".repeat(200)));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let (doc, stop) = (doc.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _reading = doc.read();
                        std::hint::black_box(doc.root().first_child());
                    }
                })
            })
            .collect();
        let mut slowest = Duration::ZERO;
        for _ in 0..2000 {
            let start = Instant::now();
            // SAFETY: the readers hold a Reading.
            unsafe {
                let p = doc.create_element("p", &[]).unwrap();
                doc.remove(p).unwrap();
            }
            slowest = slowest.max(start.elapsed() / 2);
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
        let most = doc.most_turn_entries.load(Ordering::Relaxed);
        assert!(most <= READERS, "one turn let in {most} readers");
        assert!(
            slowest < Duration::from_millis(200),
            "an edit waited {slowest:?}"
        );
    }

    #[test]
    fn waiting_edits_are_always_woken() {
        for (writers, long_reads) in [(2, false), (4, false), (2, true), (4, true)] {
            let doc = Arc::new(Document::parse(&"<p>x</p>".repeat(200)));
            let body = doc.root().descendants().find(|n| n.tag() == Some("body"));
            let body = body.unwrap().id();
            let threads: Vec<_> = (0..8)
                .map(|t| {
                    let doc = doc.clone();
                    std::thread::spawn(move || {
                        for _ in 0..10_000 {
                            if t < writers {
                                // SAFETY: the readers hold a Reading.
                                unsafe {
                                    let p = doc.create_element("p", &[]).unwrap();
                                    doc.insert(body, None, &[p]).unwrap();
                                    doc.remove(p).unwrap();
                                }
                            } else {
                                let _reading = doc.read();
                                if long_reads {
                                    std::hint::black_box(doc.root().descendants().count());
                                }
                            }
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
        }
        let (waits, timeouts, lost) = (
            WAITS.load(Ordering::Relaxed),
            TIMEOUTS.load(Ordering::Relaxed),
            LOST.load(Ordering::Relaxed),
        );
        // Sleeps that outlast a descheduled reader are fine (CI machines
        // have fewer cores than this test has threads); one nobody woke
        // isn't.
        println!("{waits} sleeps, {timeouts} ran out, {lost} wake-ups lost");
        assert!(waits > 100, "edits never had to sleep");
        assert_eq!(lost, 0, "{lost} of {waits} sleeps were never woken");
    }

    fn shape(doc: &Document) -> Vec<String> {
        doc.root()
            .descendants()
            .map(|n| format!("{:?}{:?}", n, n.attrs()))
            .collect()
    }

    #[test]
    fn tiny_chunks_build_the_same_tree_as_one_chunk() {
        // 7-byte chunks split tags, entities, attribute values and multi-byte
        // characters at every offset.
        let html = "<!doctype html><p class=\"a b\">é€😀 &amp; &eacute;</p>\
                    <script>if (a < b) {}</script><textarea>x<y</textarea>\
                    <table>t<tr><td>c</table><svg><path d=\"M0 0\"/></svg>"
            .repeat(50);
        let one = parse_in_chunks(&html, Duration::MAX, usize::MAX).unwrap();
        let small = parse_in_chunks(&html, Duration::MAX, 7).unwrap();
        assert_eq!(shape(&one), shape(&small));
    }
}
