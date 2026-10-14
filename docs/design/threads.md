# Threads on free-threaded Python

We ship a wheel for free-threaded 3.14, so a crawler can parse a page per
thread, and loaders can read one page from several. Up to 0.3.1 that
barely paid: `node.tag`, about the cheapest call there is, ran 22 million
times a second on one thread and 2.8 million a second per thread on four.
Each extra thread made every thread slower. This note covers the three
locks behind that, what replaced them, and what's still left.

`bench/threads.py` measures it: each case on 1, 2, 4 and 8 threads, on a
page per thread ("separate") or one page for all ("shared"), as calls per
second per thread. Efficiency is the 4-thread rate over the 1-thread one.
Release build, a 12-core M-series Mac, best of two alternating runs:

| case | page | eff@4 before | eff@4 after | 8 threads before → after |
|---|---|---|---|---|
| `node.tag` | separate | 0.14 | 0.84 | 0.9M → 17.5M/s |
| `node.tag` | shared | 0.16 | 0.84 | 0.9M → 18.0M/s |
| `css().get()` | separate | 0.61 | 0.60 | 55k → 62k/s |
| `css().get()` | shared | 0.20 | 0.72 | 10k → 60k/s |
| `parse()` | separate | 0.91 | 0.90 | 8k → 8k/s |
| `node.text` | separate | 0.65 | 0.69 | 249k → 866k/s |
| `node.text` | shared | 0.57 | 0.71 | 258k → 676k/s |

One thread got faster too: `node.tag` went from 22M to 25M calls a second.

## PyO3's reference pool

A `Py<T>` dropped on a thread that isn't attached to Python can't touch
the reference count, so PyO3 parks the pointer in a process-wide pool
(`ReferencePool`, a `Mutex<Vec<_>>` in pyo3 0.29's
`src/internal/state.rs`) and decrements it later. "Later" is every time
a thread attaches: `AttachGuard::assume()`, which the trampoline behind
every `#[pymethods]` and `#[pyfunction]` call runs, calls
`drop_deferred_references()`, which locks that mutex whenever the pool
exists. PyO3 creates the pool lazily so that code which never needs it
never locks it (PyO3 #4178), but `SuspendAttach::drop`, the end of every
`py.detach()`, calls `get_pool()`, which creates it. `parse()` detaches.
So from the first parse on, every call from Python into Netweir took one
global lock and released it, and on free-threaded Python the cache line
holding it bounced between every core making calls.

The evidence: building with PyO3's `--cfg pyo3_disable_reference_pool`
and changing nothing else took `node.tag` on four threads with a page each
from 0.12 of its one-thread rate to 0.79.

So that's how we build now (`.cargo/config.toml`, which the sdist
carries). The flag comes with a contract: nothing may drop a Python
object on a thread that isn't attached. Without the pool, PyO3 aborts the
process when that happens. We keep the contract like this:

- `aio::release()` takes Python objects off a runtime thread to a
  blocking thread that attaches and drops them, so no runtime thread waits
  on Python. Once Python is shutting down it leaks them, as the pool did.
- `aio::future_into_py` wraps the Rust future in `Release`: if asyncio
  cancels it before it finishes, whatever it still holds (a `self` to
  return, a handler) goes through `release()`. The loop and future
  handles it kept for the result go there too when nobody's waiting.
- A future that holds Python objects drops them inside its own `attach()`
  or through `release()` on every path, errors included: `new_page()` on
  failure, `frame()` with a `url` pattern.
- An Item's field defaults are released on drop: a rule page's parse
  job keeps the engine and its rules until it ends, on a runtime thread,
  even after the `next()` that started it was cancelled.
- `release()` counts itself in `ATTACHED` like `attach()`, so Python's
  exit waits for it (a task cancelled as `asyncio.run()` ends lands
  there).
- The crawl's `download()` used pyo3-async-runtimes' bridge, whose
  `TaskLocals` and future handles are dropped on runtime threads. It uses
  ours now, like everything else.

The config adds `pyo3_leak_on_drop_without_reference_pool`, so a drop
we've missed leaks one reference instead of taking a user's crawl down.
CI's test job sets `RUSTFLAGS` to the first flag alone, so a miss aborts
the tests there. The whole suite passes that way.

A build that ignores the config still works, but it gets the pool back,
and the old scaling with it. A `RUSTFLAGS` of your own does that, because
it replaces the config's flags, so anyone building from source with one
has to add `--cfg pyo3_disable_reference_pool` to it. The extension
reports what it was built with as `_native._reference_pool_disabled`.
`test_built_without_pyo3s_reference_pool` asserts it, so CI's tests and
the release workflow's check of every wheel would catch a build that lost
the flag, and `bench/threads.py` prints it.

## The document's reader count

Every read takes a `Reading`, which kept edits out with one `AtomicU32`
per document: a compare-and-swap in, a subtraction out (see
[selecting.md](selecting.md#readers-and-edits)). On a page per thread
that's cheap. On one page shared by several threads, every read wrote
the same cache line. In a Rust loop of `read()` and drop: 6.5 ns on one
thread, 164 ns on four, 656 ns on eight.

Readers now count themselves in one of 16 slots, each on its own 128-byte
line, picked per thread in turn. A reader adds one to its slot, then looks
at the document's word: with no edit running, waiting or handing out a
turn, it's in, having written nothing another thread reads. Otherwise it
takes its count back and goes the old way, which is what queues it fairly
behind the edit. An edit sets `WAITING`, then waits until the old count
and every slot are zero.

Both sides write their own variable and then read the other's, so the
four operations are `SeqCst`: either the edit sees the reader's slot, or
the reader sees `WAITING` and steps back. A reader leaving by the fast way
can't tell if it's the last, so while an edit waits each one wakes it.
`an_edit_waits_for_a_reader_counted_in_its_slot` fails if the edit
doesn't check the slots, and `waiting_edits_are_always_woken` still finds
no lost wake-up.

The cost is on one thread: a `SeqCst` add waits for the core's earlier
stores to land, so `read()` and drop went from 6.5 ns to 11 ns. It's now
the same, about 11 ns, on one page with 4 or 8 threads. A `Document` is
2 KB bigger.

## The selector lock

`css()` held a mutex per document while lexbor matched, because we hadn't
checked that its matcher was safe with several threads on one document.
It is: the matcher keeps all its state in its `lxb_selectors_t`, which
is the calling thread's own (see
[selecting.md](selecting.md#the-query-cache)). `selectors.c` never
writes to the tree or the compiled selector list. From the document it
only does lookups: `lxb_tag_id_by_name` and
`lxb_dom_attr_data_by_local_name` search its name tables. The lock is
gone, and `css().get()` on a shared page went from 0.20 to 0.72 (0.59
on top of the per-thread query cache, which made one thread faster;
see below). `test_sixteen_threads_read_what_one_thread_reads`
checks sixteen threads, half on one page and half on their own, against
what a single thread gets.

## Also fixed

`main_content()`, `markdown()`, `clean_text()` and `article()` read the
tree without the GIL and without a `Reading`, so an edit on another thread
could relink nodes under them. They take one now.

## What's left

- `css()` keeps about 0.6 of its rate on four threads. Before the
  per-thread query cache and matcher, every call made a lexbor CSS parser
  and a matcher and freed them again, and a Rust loop of `Query::new` and
  `run` scaled the same way without Python: `Query::new` alone ran 449k
  times a second on one thread, and only 187k a second per thread on four.
  With the cache and the matcher kept, `css().get()` runs 135k times a
  second on one thread instead of 100k, but still keeps 0.59 on four,
  on a page each or one shared. What holds it there is still to find.
- `css()` and the getters that let go of the GIL clone the
  `Arc<Document>`, an atomic add on a line every thread reading that
  page shares. That's one per call, against a query's microseconds.
- CPython's own counting. Free-threaded Python counts references to an
  object without atomics only on the thread that made it, and other
  threads pay for an atomic. A `Node` made on one thread and used on four
  others did 0.56 in the first version of the benchmark, against 0.79
  with a `Node` per thread (both without the pool). The benchmark makes
  its objects on the thread that uses them. To get the same, do the
  `css()` that finds a node on the thread that reads it.
