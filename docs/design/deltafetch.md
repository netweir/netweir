# Skipping pages that already gave items

A crawl run every day over a catalogue fetches every product page every
day. Most of them haven't changed, and their items are already in
yesterday's output. Scrapy users reach for scrapy-deltafetch: it remembers
which pages gave items, and a later run skips them. Listing pages give
only links, so they are never remembered and are fetched every run, which
is how new products are found.

With `deltafetch` set to a directory, Netweir does the same, with the
differences listed at the end.

## Settings

- `deltafetch`: a directory, or `None` (the default). The record is
  `<dir>/deltafetch.sqlite3`, so the directory can be the one the cache or
  the checkpoint uses.
- `deltafetch_reset`: `True` empties the record when the crawl starts, so
  this run fetches everything, records afresh and starts its output files
  afresh. `False` by default.

## The output keeps every run's items

Skipping a page is only safe if its items are already somewhere. So with
`deltafetch`, a run adds to its output files rather than starting them
afresh: the output holds every item every run has collected, which is what
the record's pages stand for. Scrapy's own feeds append by default too.

- JSON Lines and CSV are appended to. CSV writes no second header and
  keeps the file's columns: without `fields=`, they're read from its
  header, not taken from this run's first item, so a run whose items have
  their keys in another order (or more keys) still writes each value under
  its own column, warning once about keys with none. `fields=` that differ
  from the header are refused, before any output is touched. An item with
  no keys can't start a file (a header of no columns would hold every
  later row to one empty column): it's left out, with a warning, until an
  item with keys gives the file its columns.
- JSON and XML go on after the file's last item (the writer takes the
  closing `]` or root tag off and puts it back at close, as it does when a
  checkpoint resumes).
- Parquet gets a part per run beside the file (`items.1.parquet`, ...), as
  on a checkpoint resume, and a run that got no items writes no part.
- Batches start with `append`: each one goes to the first free name,
  numbered on from the files already there, so no run writes over another
  run's batch, whatever the path's `{time}` says (it changes once a
  second). A run that got no items writes no batch. For a remote batch,
  whether a name is free is asked of the filesystem.
- A single file in cloud storage is refused: it's uploaded whole at close,
  so each run would replace the last run's items, and an object can't be
  added to.

Someone who wants only each run's new items moves the file away after each
run (the next one starts it again), or names each run's file, a date in
the path.

### Which file is which

The record keeps, for each output file (by absolute path; batches aren't
tracked, as they're files of their own), how long it was when pages were
last recorded, a digest of its end (SHA-256 over the length and the 64 KiB
before it) and whether the run that wrote it closed it. That flag is set
in the same transaction as the final length, once the writer has closed
and synced the file at the end of a run, and cleared by the first record
of the next run that writes to it (made as soon as the file is opened). Before a run touches any output:

- a file the record knows must be at least that long and match the digest,
  or the run is refused: another run wrote it afresh, or it was replaced,
  and cutting it back would cut someone else's item in half;
- a file that grew past its length is refused if the run that wrote it
  closed it cleanly: that run left it ending exactly at the length on
  record, so the extra bytes are someone else's. Only when that run didn't
  close cleanly (it crashed, or was killed) are the bytes past the length
  taken as its own torn tail and cut (see below). Known limit: lines a
  user adds to a file after a crash can't be told from that tail, and are
  cut with it;
- a file the record doesn't know, while the record has pages, is refused
  if it's there: the items of the record's pages may be in another file,
  and starting this one afresh would lose what it holds. If it isn't
  there, it's started, and gets only what's new; that's the way to archive
  each run's items, by moving the file away;
- with a record that has no pages (new, or just reset), a file that's
  there is started afresh: every page is about to be fetched again.

A checkpoint's lengths get the same digest and the same flag (in the
counters it saves; a finished checkpoint's files were closed cleanly), and
a resuming checkpoint's lengths are used in place of the record's.

### After a crash

A file appended to after a crash would carry a damaged tail: a half-written
line, or items whose pages the record never got and will be fetched again.
So each run cuts each file back to its recorded length before adding to it,
exactly as a checkpoint resume does. Everything after the cut came from
pages not recorded, which this run fetches again, or from errbacks of
requests that weren't recorded either, so nothing is lost and nothing is
written twice.

Lengths are recorded whenever items were delivered since the last record,
not only when pages were: an errback's items move the file on too.

They are also recorded at the start of each run, after the files are cut
back and before any is opened, as what each will be once it is, and as not
closed: a JSON or XML file taken up again without its closing `]` or root
tag (the writer works out where that leaves it without touching the file),
any other file taken up again as it is, and a file started afresh as
empty. A crash at any point after that leaves each file at that length or
longer, never shorter, with the record saying it wasn't closed, so the next
run cuts it back to exactly that point. Recording only after opening would
leave a window in which the closing tag is gone but the record still has
the length with it and the file closed: every later run would refuse the
file as shorter than recorded. And a new output file killed before its
first item would be refused for ever as one the record doesn't know. The
same goes for a checkpoint taken up again.

Closing a file syncs it to disk before its length is recorded, and a
Parquet part's directory is synced once it's written and once it's named.

### Parquet parts

A Parquet file is written under a `.partial` name and readable only once
closed, so its pages wait for the end of the run. Then, in this order: the
part is closed and synced; its pages are recorded with the part's final
name, in one transaction; the part gets its name. A crash before the
record leaves a `.partial` the next run deletes (its pages are fetched
again); a crash after it leaves one the next run names, as the record has
it. The checkpoint does the same with its own list of parts.

A part is named once its items are on record somewhere: its pages in
deltafetch's record, or its items as delivered in the checkpoint. If
neither (a pipeline failed to close, the write failed, or the checkpoint's
commit did), the part isn't named: it's left as a `.partial`, with a
warning, for the next run to delete, since that run fetches its pages
again and a named part would give their items twice.

With a checkpoint as well, a run whose pipelines didn't all close records
nothing more in the checkpoint at the end, and marks none of its last
requests done: the part's items aren't on record as delivered, so a run
taking that checkpoint up again fetches its pages and writes them again,
rather than skipping items that are in no named part. Before this, the
part went into the checkpoint's list and was named by the resume, and a
later run with another checkpoint, finding its pages unrecorded, wrote
its items a second time. A run that closed cleanly but whose deltafetch
write then failed still names its part, as the checkpoint already has its
items as delivered: a run taking that checkpoint up again won't write them,
and the part is where they are. A later run with another checkpoint
fetches those pages again and writes their items twice; that's the
remaining duplicate, against a loss.

Parts are read together as one dataset only if their schemas agree, and
each part's columns come from its own items: a field that appears, goes,
or changes type between runs gives parts a reader may refuse to combine.

### With a checkpoint

The checkpoint's writes are committed by a thread of its own, a batch at a
time. The record must never be ahead of it: a resume cuts the files back to
the checkpoint's lengths, and items of pages the record already skips would
be gone. So before pages are recorded, the run waits for the checkpoint to
commit what it has (its writer commits at once when a flush is waiting,
rather than gathering for 50 ms more), and if that commit failed, the pages
wait for the next batch. A failed commit's writes go again, first, with
the next one, so no later commit lands without them: one that did would
hold counters whose file lengths cover items the checkpoint doesn't have
as delivered, and a resume would write those items again. Once a commit
with them in it lands, recording again is safe. Until then every flush
reports the failure, the one that closes the crawl included. Held back,
the counters and each cookie jar's snapshot keep only their latest, as
each replaces the last whole; past a million held writes the checkpoint
stops where its last good commit left it, and the crawl with it.

## What is recorded

A request's key, once its page has given at least one item that came out
of every pipeline (one counted in `stats["items"]`) and the callback it was
given to has finished without raising. An item a pipeline dropped, or a
validate stage failed, doesn't count. A callback that raises part way may
have had more items to give, so its page isn't recorded and is fetched
again next run, even with `fail_fast` off, and the items it gave before
the error are written a second time; the same goes for a page whose
callback `max_items` stopped.

The key is the duplicate filter's fingerprint of the request as it was
submitted (method, canonical URL and body), not of where a redirect took
it. A request with `meta["deltafetch_key"]` is keyed by that instead, as in
scrapy-deltafetch: a product reached through several URLs can be keyed by
its SKU. As there, a key is a str or bytes, a str standing for its UTF-8
bytes, and any falsy value (`""`, `0`, None, an empty list) is no key at
all, so the fingerprint is used. Anything else drops the request with an error, counted as a callback
error, whether the request came from a callback or `start()`; with
`fail_fast` it stops the crawl. Without `deltafetch` the key is ignored. A
custom key is hashed into the same 16 bytes as a fingerprint, with a prefix
so it can never equal one.

`meta["deltafetch_enabled"] = False` leaves a request out, as in
scrapy-deltafetch: it's never skipped, and its page is never recorded.

Every way a page gives items records its request:

- a callback in this process;
- a callback in a worker process (`workers`), recorded in the main process
  when its results come back without an error;
- a link rule's `Item`, extracted in Rust, which records the GET of the
  link that led to the page;
- a link rule's callback.

An errback or `on_block` yields items with no page behind them, so they
record nothing: there is nothing to skip.

An item a resumed crawl finds already written (`items_already_written`)
counts as delivered for its page. It came out of the pipelines in the run
before, and the record of it may be what the crash lost.

## When it is recorded

Only once the item is on disk. While a callback runs, `_Run` holds its
page's request and whether it has given a delivered item; when the callback
finishes, the page's key goes into a set. `settle`, after each batch,
flushes the exporters (and, with a checkpoint, waits for its commit) and
only then hands the set to the engine, with the files' lengths, in one
transaction. A Parquet exporter can't flush (its file is readable only
once closed), and a file bound for cloud storage is there only once it's
uploaded; with either, the keys wait for the end of the crawl, after the
exporters close, and are dropped if any pipeline failed to close. The set
holds 16-byte keys as Python bytes, 80 to 100 bytes a page with the set's
own room.

So a crash loses at most the keys of the batches not yet handed over, and
their pages are fetched again next run, after their partial output is cut
away. Recording before the item was on disk would be the dangerous
direction: a page skipped for ever whose item never reached the output.

Known limit: a pipeline of your own that holds items and writes them only
when it closes (a database it commits to at the end) can't say so, so its
pages are recorded batch by batch as for a file. A crash before it closes
loses those items, and the next run skips their pages. A `flush()` hook for
pipelines, asked like the exporters' before recording, would close that
gap.

## When a request is skipped

When it is submitted, in the engine's `submit`, and only if its key is in
the record, it doesn't have `dont_filter=True` and deltafetch isn't off for
it. It's counted as `skipped_delta` and `submit` returns
`Submitted::Delta`; Python logs it at debug, as it does other skips.
Nothing about it is fetched, cached or checked against robots.txt.

The order of checks is: a valid URL and proxy, then offsite, depth and
traps (each counted under its own name), then the duplicate filter, then
the record, then `max_pages_per_domain`. After the duplicate filter, so a
product linked from fifty listing pages counts once in `skipped_delta` and
forty-nine times in `duplicates`, which is what happened. Before
`max_pages_per_domain`, so skipped pages don't use up a site's allowance.
A skipped request is marked seen in memory only, not in the checkpoint: a
resumed run looks it up in the record again, with the same answer.

Start requests are submitted like any other, so they are skipped too. That
is a deliberate difference: scrapy-deltafetch is a spider middleware and
only sees what callbacks yield, so it fetches start requests every run.
Skipping them is the consistent rule (a start URL that is a product page is
a product page), and `dont_filter=True` fetches one every run.

Requests read back from a checkpoint are not checked. They were accepted
before and never finished, and a crash may have left the record knowing
their page while the output file was cut back without its items (the
checkpoint records items only once its own commit lands). Fetching them
again lets the checkpoint write those items; skipping them would lose
them.

## Deltafetch, the checkpoint and the cache

They answer different questions and work together:

- The checkpoint is one crawl, interrupted and carried on. It skips what
  this crawl already finished, listing pages included, and forgets nothing
  until its directory is deleted. A finished crawl with a checkpoint run
  again does nothing.
- Deltafetch is a crawl run again and again. It skips only pages that gave
  items, in any earlier run, so every run sees the listings fresh and
  finds what's new.
- The cache is for writing a spider: every page comes off the disk instead
  of the site, items and all.

With both deltafetch and a checkpoint, the checkpoint keeps a run's
progress and deltafetch keeps what all runs found. They meet in two places:
read-back requests aren't checked against the record, and a resuming
checkpoint's file lengths win over the record's. With the cache, a skipped
request never reaches the cache, and a cached page that gives items is
recorded like a fetched one.

## Differences from scrapy-deltafetch

- Start requests are skipped too (above).
- A page is recorded after its items are through the pipelines and on
  disk, and only if its callback finished; scrapy-deltafetch records it
  when the callback yields an item, before any pipeline.
- `dont_filter=True` fetches a recorded page; scrapy-deltafetch ignores it.
- Output files are added to and cut back after a crash; Scrapy leaves feeds
  to their own settings.
- `DELTAFETCH_RESET` and `-a deltafetch_reset=1` are
  `deltafetch_reset=True`; `DELTAFETCH_DIR` is `deltafetch` itself.

## The file

SQLite in WAL mode with `synchronous=NORMAL` and a busy timeout, like the
checkpoint and the cache: a second crawl on the same directory waits its
turn. A table of 16-byte keys and a table of output files: each one's
length, the digest of its end (0 and none for a Parquet output and its
parts) and whether it was closed cleanly. The format is
the file's `user_version`, 1; a file from a newer Netweir is refused before
anything is written.

The keys are read into memory when the crawl starts, as the checkpoint's
seen set is, because the check runs under the engine's lock and must never
wait on the file. A `HashSet<[u8; 16]>` takes 16 bytes a key plus a control
byte, in a table up to half empty after it grows: 35 to 40 bytes a key, so
a catalogue of a million products costs some 40 MB. Keys recorded during
the run are added to the set too: a request submitted after its key was
recorded is skipped in the same run. One submitted before, while the first
page's items are still on their way to disk, is fetched; the duplicate
filter already stops the common case of one URL linked twice.

A write that fails doesn't stop the crawl. It warns once, naming the file,
and its pages wait for the next write, which records them with lengths that
cover their items. If no later write lands (the crawl dies first), the
lengths on record don't cover those items either, so the next run cuts
them off and fetches their pages again: repeated work, nothing lost or
written twice.

Known limit: nothing expires. A product whose page changes is never
fetched again until the record is reset, as with scrapy-deltafetch. A
per-key age, and a setting to refetch anything older, would fix that.
