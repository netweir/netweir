# Netweir worker processes

Status: draft, October 2026
Author: James Conway

## What it is for

The engine fetches, schedules and parses in Rust, off Python's GIL, but a
spider's callbacks run in one Python process, one after another. A spider
whose callbacks do real work, such as heavy parsing, a model, or image
decoding, is held to one core however fast the engine is.
`Settings(workers=4)` runs callbacks in four worker processes while the
engine keeps fetching.

## What runs where

The main process keeps everything that writes state: the engine, its
queue and checkpoint, `start()`, pipelines and exporters, `on_block` and
errbacks, and the item ids that make resuming exact. Workers run
callbacks and nothing else.

| Where | What |
|---|---|
| main | engine, `start()`, pipelines, exporters, checkpoint, errbacks, `on_block`, repairs |
| workers | callbacks of fetched pages, including those the rules call |
| main, always | callbacks of browser requests, whose live `page.browser` can't leave the process |

## How a batch goes

The run loop already takes events in batches. With workers, the
callbacks of a batch's pages are sent to the pool at once; each worker
runs its callback to the end (a generator is drained, a coroutine
awaited, an async generator iterated) and sends back everything it
yielded, in order. The main process then handles every page's results
in the batch's own order, exactly as with one process: items through the
pipelines with the same ids, requests submitted with the same depths.
So `workers=4` writes the same items, with the same ids, as `workers=1`,
and a checkpointed crawl resumes the same way. (Pages finish fetching in
a different order from run to run either way, so the order of items in
a file was never fixed.)

A page goes to a worker as its response's parts (URL, status, version,
headers, body) and its request; the worker builds the `Page` again and
parses it there. Requests come back with callbacks by name, as a
checkpoint already requires; a callback that isn't a method of the
spider, by name or bound, is an error with workers, as with a
checkpoint.

## The spider in a worker

Each worker builds its own spider: `type(spider)()` with the crawl's
settings. The class must be importable by the worker: defined in a
module (not inside a function), or in the file `netweir crawl` was given,
which the workers load the same way. Workers start with Python's spawn
method on every platform, so nothing is inherited by accident.

State a callback sets on the spider stays in that worker. That's
documented, not hidden: callbacks that count or collect across pages
should yield items and let a pipeline do it in the main process.

## Tracking, logging and errors

Tracked selectors work in workers: each opens the crawl's track store
(the checkpoint's SQLite file, or `~/.netweir/tracks.db`), which SQLite
shares between processes. A tracked selector that breaks asks the main
process for a repair through the results, with the page's HTML. Log
records from workers are sent to the main process's `netweir` logger, so
warnings appear once per crawl as before. An exception in a callback
comes back with its traceback, counted in `callback_errors` and logged in
the main process. Anything sent back is checked in the worker to rebuild
from its pickle, so one that wouldn't can't break the pool. With
`fail_fast`, the first error to come back from any of a batch's jobs
stops the crawl, and the workers are terminated rather than waited for.
A worker that dies breaks the pool, and the crawl ends with an error
naming the page; workers watch the main process and exit with it, so a
crawl killed outright leaves none behind. Workers send a page's HTML for
a repair only when the spider has a repairer, once per site and name.

## Testing

- The same spider writes the same items, ids and order with `workers=1`
  and `workers=3`.
- Items record the process they came from: more than one worker ran.
- Errors in worker callbacks are counted and logged; `fail_fast` stops
  the crawl.
- A checkpointed crawl killed and resumed with workers loses and repeats
  nothing.
- Tracked selectors relocate in workers and their warnings reach the
  main log.
- `netweir crawl spider.py -s workers=2` works for a spider defined in
  the file.
- `bench/workers.py` measures a CPU-heavy callback with 1 and 4 workers.

## Milestone

**M12, workers.** `Settings.workers`, the pool, results in batch order,
pickling for responses, logging and repairs across processes, the CLI
path, the tests and the benchmark above, docs.
