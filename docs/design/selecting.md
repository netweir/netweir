# Editing the tree, similar elements, JMESPath and the query cache

Three things people reach for in other libraries and didn't find here:
Beautiful Soup's tree editing (`decompose()` before `get_text()`, mostly),
Scrapling's `find_similar()`, and parsel's `.jmespath()`. This note is why
each works the way it does. The last section is about speed instead: why
a query written inside a loop costs no more than one hoisted out of it.
The user-facing description is in [selecting.md](../selecting.md).

## Editing

### What has to stay true

Before this, a document never changed after parsing, and a lot leaned on
that:

- A Python `Node` is an `Arc<Document>` and a lexbor node pointer. So is
  every entry of a `Selection`, and the declarative and tracking code
  hold the same pointers while they run.
- Queries, `.text`, `.html`, metadata and the rest release the GIL and
  read the tree on whatever thread they're on. On free-threaded Python
  (we ship a 3.14t wheel) nothing holds the GIL at all.
- The document index stores node pointers, parent and subtree-end
  positions, and writes each node's position into lexbor's `user` field.
  XPath and document order read it.
- `Node<'a>` hands out `&'a str` borrowed from lexbor's memory: tag
  names, attribute values, text.

Editing has to keep every one of those safe, without making read-only
pages pay for it.

### Nothing is freed until the document is

lexbor lets you destroy a node (`lxb_dom_node_destroy_deep`) or just
unlink it (`lxb_dom_node_remove`). We only ever unlink. A removed node
stays in lexbor's arena, still complete, until the document is dropped
and the arena with it. That one rule makes handles safe: a pointer to a
removed node, or to anything inside it, points at a valid, unchanged node
for as long as any `Arc<Document>` keeps the document alive. No
generation counters, no lookup table, no check on every access.

The same goes for strings. No edit changes an existing node's text or
attributes in place: `.string = ...` removes the children and adds a new
text node, and attributes are only set on elements `new_tag()` has just
made. So an `&'a str` borrowed before an edit still reads what it read.

The price is memory: `decompose()` returns nothing to the allocator until
the page goes. Pages live for one callback, so that's the right trade.

Given that, a removed node can be readable rather than an error, so it
is: it reads as it was, with no parent. That's kinder than Beautiful
Soup, where touching a decomposed tag is undefined in practice.

The edits use lexbor's `_wo_events` relinking functions. The ordinary
ones call mutation callbacks the HTML document can register; we register
none, but the callbacks are where lexbor would free or update other
state, and skipping them keeps "only pointers change" true by
construction.

### Readers and edits

An edit relinks nodes and can add tag and attribute names to lexbor's
per-document tables. A reader on another thread walking the same links
or looking up the same tables would race with it. So each document has a
small reader/writer lock: an `AtomicU32` holding a reader count and two
flags.

- Every read from Python takes a `Reading` (`Document::read()`): one
  compare-and-swap in, one subtraction out. `Node::get()` in the bindings
  returns the node together with its `Reading`, so the many small getters
  can't forget it, and every closure that releases the GIL takes one
  first. A `read()` and its drop cost 5.0 to 5.6 ns in a release build,
  the same with 1, 4 or 8 threads each reading its own page. (An earlier
  version kept a per-thread `Vec` of the documents held; the vectors' heap
  buffers shared cache lines between threads, and the same loop took 30
  to 57 ns on 4 and 8 threads. A per-thread counter fixed that.) Parse and
  query benchmarks didn't move.
- An edit runs without the GIL. It sets `WAITING`, waits for the reader
  count to reach zero, sets `WRITING`, does its work, drops the index and
  clears `WRITING`. It waits by yielding a few dozen times, then
  sleeping on a condition variable. A timed sleep would be simpler, but
  Windows rounds a 50 µs sleep up to a millisecond or more.
- Before it sleeps, an edit sets `WAITING` again under the condition
  variable's mutex rather than only reading the word. With two edits
  waiting, the one that gets in clears the other's bit; without setting it
  again, whoever leaves next (that edit, or the last reader) wouldn't know
  to wake the sleeper, and it slept out its 1 ms fallback timeout: 1 in
  about 150 sleeps in a test with 2 to 4 editors. That test now counts
  timeouts and expects none. A `WAITING` bit is only ever set by an edit
  that is still waiting (the winner's compare-and-swap clears it), so a
  stale bit can't hold readers back once no edit wants in.
- A new reader yields to a waiting edit, but only for a while
  (`PATIENCE`). Without the yielding, readers arriving back to back kept
  an edit out for seconds in testing. Without the limit, a reader that
  holds the GIL could wait for an edit that waits for a reader that needs
  the GIL to finish (a `find_all()` calling a Python function).
- Readers held back by an edit (one running, or one waiting that they
  deferred to) take a ticket: the number of edits finished so far. A
  reader that only lost a race to another reader doesn't. An edit that
  finishes with tickets out hands those readers a turn (`TURN`): they go
  in without yielding, and no edit starts until the last of them is in.
  Readers arriving during the turn get the next one, so a turn admits at
  most one reader a thread and a waiting edit's delay is bounded by the
  readers present when the turn began. (The first version counted every
  reader that retried, and let newcomers in during a turn: one turn
  admitted 93,000 readers in a test that now caps it at one a thread.)
  Before turns existed, an editor working back to back took the document
  again in the nanoseconds before a yielding reader woke, and readers got
  in only when their patience ran out. On a 2,000-node page with 8 threads
  each running `count(//p)` and one thread editing back to back (release
  build, 3 s): reads took 748 µs each before turns and 173 µs after (23 µs
  with no editor; most of the difference is rebuilding the index each
  edit drops), and edits 174 µs each, the worst 1.7 ms, since an edit now
  waits out one turn of readers.
- A read nested in another on the same thread (a `find_all()` function
  reading the node it was given) never waits.
- An edit on a thread that holds any `Reading` fails with
  `EditError::Reading` (`RuntimeError` in Python). On the same document it
  would wait for itself. On another it can deadlock: thread 1, inside a
  `find_all()` function on page X, edits page Y while thread 2, inside one
  on Y, edits X, and each waits for the other's read. Refusing every edit
  inside a read is what makes that impossible; a per-document check
  doesn't. So a `find_all()` function can't edit any page.

Edits release the GIL while they wait so a reader that needs the GIL can
finish; holding it would deadlock against exactly that reader.

The Rust edit methods are `unsafe`: like `Document::node()`, they trust
that ids come from this document, and they trust that other threads
reading meanwhile hold a `Reading`. The Python bindings keep both
promises; plain Rust callers on one thread need nothing.

### The index

The index is dropped by every edit and rebuilt by the next query that
wants it, so a page that's only read never rebuilds it, and ten edits in a
row cost one rebuild, not ten. It lives in an `UnsafeCell<OnceLock>`: the
`OnceLock` keeps concurrent first builds safe as before, and only an
edit, which has the document to itself, replaces it.

Text and nodes go in together: `insert()` and `replace()` take
`Content` items (a node, or a string to make into text) and make the text
only after every node has been checked, so a failed `append("x")`
leaves nothing behind. `insert()` finds its position inside the edit, so
an edit on another thread can't shift the children in between.

Removed subtrees are indexed too. The document keeps a list of the nodes
edits left without a parent (removed ones and new ones not yet placed).
A rebuild numbers the document first, then each of those still without a
parent, so every node a handle can reach has a position and XPath works
on removed nodes like anything else. A removed node's XPath `/` is the
top of its own subtree, as in lxml, which needed XPath's root to become
"the top of the scope's tree" rather than "the document"; for a node in
the document they are the same.

`Node::descendants()` used to assume a node's parent chain ends at the
walk's start. An edit on the same thread mid-walk (possible from Rust,
not from Python) can break that, so the walk stops at a missing parent
instead of following a null pointer.

### Checking it

- Rust tests for every edit, edits that must fail without changing
  anything, the index after edits, readers on four threads against a
  writer, and a stress test of 2,000 random edits that keeps every
  handle it ever took and reads them all repeatedly.
- The same tests pass under AddressSanitizer (Rust side; Apple's clang
  and Rust's sanitizer runtimes don't link together for the C side). Miri
  can't run lexbor's C.
- A fuzz target, `edit`, applies any sequence of edits to a page and
  checks after each that the index agrees with a walk of the tree and that
  every node ever seen still reads: 129,000 runs under AddressSanitizer
  found nothing.
- Python tests for each method, mutation after a held `Selection`, the
  `find_all()` case, readers on threads against a writer, and churn with
  garbage collection in between.

They stay in the index for the page's life, and every rebuild walks
them. Dropping one that nothing refers to would need to know that no
Python handle holds a node in it, which nothing tracks, and a handle to an
unindexed node would read a stale position. So it isn't done; the user
docs say to edit first and query after, since a loop alternating the two
rebuilds the whole index each turn.

### What's left out

Setting or deleting one attribute would change memory that a borrowed
`&str` may point at, so it would need copying strings out or another
level of indirection. It isn't in this milestone. `insert_before`,
`insert_after`, `extend`, `wrap` and `new_string` are all thin wrappers
over `insert()`, `replace_with()` and strings, and were left until
someone asks.

`new_tag()` always makes an HTML-namespace element and lowercases
attribute names. Creating the element in its future parent's namespace
would mean choosing the namespace at insertion, after creation, which
lexbor doesn't support without rebuilding the element; scrapers read
pages rather than render them, so it's a documented limit.

## find_similar

The definition is kept simple enough to check by hand, because you'll
want to know why an element was or wasn't included:

1. same tag;
2. same depth, with the same tag at every level up to the root;
3. at least `threshold` Jaccard similarity of attribute features, where a
   feature is an attribute name or one class token.

Attribute values other than classes are ignored: `href`, `src`, `id` and
`data-*` values are exactly what differs between cards. Text is ignored
for the same reason. The default threshold, 0.5, lets a card carry one
extra class (`card sale`) and shuts out a sibling that only shares a
generic one.

Steps 1 and 2 run on the index as integer work: tag ids are lexbor's
interned ids, and the ancestor walk compares parent positions and tag ids,
stopping as soon as the two chains meet at a shared ancestor, which for
siblings is the first step. Only survivors have their attributes read,
and features are borrowed `&str`s, so nothing is allocated per candidate
beyond a small vector.

On the shop listing from `bench/pages.py` (release build, an arm64 Mac),
finding the other cards from the first:

| Page | Found | First call (builds the index) | After |
|---|---|---|---|
| 1 MB | 997 | 0.53 ms | 0.14 ms |
| 10 MB | 9,961 | 12.6 ms | 1.6 ms |

## JMESPath

Two choices: the Python `jmespath` package, as parsel uses, or the Rust
`jmespath` crate (MIT, so `cargo deny` would accept it).

We use the Python package, as an optional extra
(`pip install 'netweir[jmespath]'`):

- It's the reference implementation, by the people who wrote the
  specification, and what parsel users already have. Results match
  parsel's exactly, which is what someone switching wants.
- The data is usually Python already: `js_data()` returns dicts and
  lists, and `json.loads` is fast. The Rust crate would mean converting
  Python objects to `serde_json` values and back on every call, which
  costs more than the search on the JSON a page carries.
- It adds nothing to the wheel or the build, and nobody who doesn't
  use JMESPath installs it.

Missing, it raises an `ImportError` naming the extra.

Return values are plain Python values, not a Selection. parsel wraps
JSON results in Selectors so you can chain; here there's nothing useful
to chain to (you can't run CSS on a dict), and `page.jmespath(...)`
returning the list you asked for is what people use `.get()` and
`.getall()` to get anyway. `Selection.jmespath()` gives one value per
result, leaving out Nones, rather than flattening lists the way parsel
does, so a list result stays the list it was.

## The query cache

`for card in page.css("article"): card.css("h3 a::attr(title)").get()` is
how most people pull items out of a page, and it's what parsel's docs
show. Every call used to compile its query from scratch. On one small
card, that was most of the bill. Here's where 3.3 µs went, for that call on
the generated shop page:

| | Before | After |
|---|---|---|
| Compiling the selector (lexbor's parser, and freeing it) | 2.2 µs | 0 (a hash lookup) |
| Making and freeing lexbor's matcher | 0.4 µs | 0 |
| Matching inside the card | 0.4 µs | 0.4 µs |
| Python call, `Selection`, `get()`'s string | 0.3 µs | 0.3 µs |

XPath was gentler, since our parser is ours and cheap, but compiling was
still about half of a 2.2 µs `.//p[@class='price_color']/text()`.

So compiled queries are cached, keyed by the query text. XPath variables
are bound when the query runs, not when it's compiled, so they aren't part
of the key, and there are no namespaces to key on. Errors aren't cached;
they raise, and nobody loops over a query that raises.

**Per thread, not shared.** A compiled `Query` is safe to share (its
`Sync` comment has the lexbor reading), so this is about speed. A shared
map needs a lock, and on free-threaded Python every query from every
thread would take it. A thread-local map takes none. The cost is
duplication: each thread compiles its own copy of the queries it uses.

**Bounded.** Each thread keeps up to 64 CSS queries and 64 XPath ones
(`QUERY_CACHE_SIZE`), and at most 8 KB of query text in each cache
(`QUERY_CACHE_TEXT`). Both limits matter, because a compiled query's
memory grows with its text:

| Query | Memory |
|---|---|
| `h3 a::attr(title)` | 6.3 KB |
| 60 comma-separated selectors (2 KB of text) | 45 KB |
| `:is()` with 400 branches | 99 KB |
| a typical XPath | 1.5 KB |
| densest CSS or XPath (`a,a,a,...`, stacked predicates) | about 130 bytes per byte of text |

Ordinary queries hit the count first: a full pair of caches is about
0.5 MB per thread. Pathological ones hit the text limit: about 1.1 MB per
cache, 2.2 MB a thread at worst. Either way, times the number of threads
that query: 32 MB for a 64-thread executor that has filled every cache
with ordinary queries. Nothing shrinks until the thread exits. When a cache is full it's
emptied rather than trimmed to the least recently used. A loop of unique queries (an id pasted into each
selector) thrashes an LRU just as badly, and hot queries cost one compile
each to come back. If a real workload has more than 64 hot queries, LRU is
the upgrade.

The 6 KB used to be 80 KB, which would have made a full cache 5 MB a
thread. lexbor's default memory for a selector list
(`lxb_css_memory_init`) is sized for a stylesheet, and most of it isn't
the arenas but the free-block index inside each of its two `mraw`
allocators: `lexbor_mraw_init` always gives that index room for 512
entries, about 28 KB apiece, before anything is freed. A compiled list
frees next to nothing. So the shim builds the memory itself
(`nw_css_memory_new`): 1 KB arenas, room for 16 selector objects and an
8-entry free-block index, each of which grows if a long selector needs
it. And one query gets one memory, shared by all its comma-separated
parts (each part is compiled separately, for its own `::text` or
`::attr()` ending). With a memory per part, a 60-selector list held
376 KB; shared, 45 KB. The query frees the memory, and with it every
part's list, when it's dropped.

**The matcher.** `nw_select` used to create and destroy an
`lxb_selectors_t` on every call, which cost more than the match. Each
thread now keeps one, and `nw_select` cleans it after each run
(`lxb_selectors_clean` keeps the first block of its two arenas and frees
the rest), so a big query doesn't leave a big matcher behind. A thread
never uses its matcher twice at once: nothing a match calls reaches back
into `select`. The document's `select_lock` stays, since it was never
about the matcher. It's made on a thread's first query; if lexbor can't
allocate it, `Query::try_run` returns `OutOfMemory` (Python sees
`MemoryError`) and the next query tries again.

XPath's `run_with` also stopped walking the expression for `$variables` on
every call; `XPath::new` lists them once.

`bench/items.py` times the pattern: 998 cards, four fields each, every
library parsing outside the timing:

| Per query | CSS | XPath |
|---|---|---|
| Netweir, before | 3.3 µs | 2.2 µs |
| Netweir, after | 0.8 µs | 1.3 µs |
| selectolax (`css_first`) | 1.2 µs | |
| parsel | 7.0 µs | 5.6 µs |
| lxml (`cssselect`, `xpath`) | 10.3 µs | 6.0 µs |

Before, selectolax was three times faster per item; whole-page queries were
never affected, and `bench/parse.py` and `bench/query.py` read the same
before and after.

### Not fixed here: threads

Queries scale badly across threads on free-threaded Python, and it isn't
the cache: four threads, each reading `.tag` from its own document, get
about a sixth of one thread's per-thread rate. Profiles of the query loop
on four threads show a third of the time waiting on one mutex, PyO3's
deferred-decref pool (`ReferencePool::drop_deferred_references`), which
every call into Rust locks once anything has ever dropped a Python object
while detached. That's its own piece of work.
