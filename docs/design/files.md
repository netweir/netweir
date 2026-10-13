# Downloading the files and images items link to

Scrapy's FilesPipeline and ImagesPipeline are among the parts people
port first: an item lists URLs in `file_urls` or `image_urls`, and the
pipeline fetches each, stores it once under a name made from its URL,
and notes on the item what it stored. Netweir had nothing like it, so a
spider that wanted a datasheet or a cover had to fetch it itself, with
none of the crawl's politeness. This adds `netweir.files` and
`netweir.images`, two pipeline stages.

## What they do

```python
pipelines = [netweir.files("downloads/"), netweir.export.jsonl("items.jsonl")]
```

- Field names, result entries and store layout are Scrapy's, so items
  and stores carry over: `file_urls` in, `files` out, each entry
  `{"url", "path", "checksum", "status"}`; files at
  `full/<sha1 of the URL>.<ext>`, images at `full/<sha1>.jpg` and
  thumbnails at `thumbs/<name>/<sha1>.jpg`. `checksum` is the MD5 of
  what was stored. The code is written afresh; only the public layout is
  matched. Names match Scrapy's for most files, not all: Scrapy hashes
  the URL after percent-encoding it, takes no extension from the
  Content-Type, and names extensions by Python's mimetypes.
- `status` is `"downloaded"`, `"cached"` (the crawl's cache answered) or
  `"uptodate"` (the store had it, younger than `expires`, and nothing was
  fetched).
- A failed download is logged and left out of the list; the item goes
  on, as in Scrapy. Any exception storing a file or converting an image
  fails that file, not the item: cloud SDKs and Pillow raise errors of
  their own kinds, and one escaping `process_item` would count as a
  callback error and drop the rest of the page's items. Each URL's task
  returns None rather than raising, so later items naming it don't
  raise again either. The stats count `files_downloaded`, `files_cached`,
  `files_uptodate`, `files_failed` and, for images, `images_too_small`.
- `store` is a directory or an fsspec URL, resolved when the stage is
  made, as exporters resolve theirs, so a missing `s3fs` fails before
  the crawl. Each file is written as it's stored: a remote store gets it
  with one `pipe_file`, a local one through a temporary file and
  `os.replace`, so no reader ever sees half a file.

## How a download goes through the engine

The requirement was that a file is fetched exactly as the crawl's pages
are: the same browser profile and session, robots.txt and TDMRep, the
per-host delay and concurrency, proxies and rotation, retries, the cache,
the archive and `max_response_size`. A side client would have had none of
that, or copies of it that drift. So the download goes into the engine.

The obvious way, submitting it as an ordinary request and waiting for its
event, deadlocks. Pipelines run inside the crawl loop: the loop reads a
batch of events, runs callbacks, and each item goes through the stages
before the next is handled. A stage that waits for an event waits for the
very loop that would read it. And the engine stops starting fetches once
`2 × concurrency` events wait to be read, so even reading events while
waiting wouldn't be enough: the download might never start.

`Crawler::download` in netweir-core is the way through. It queues the
request on its host like any other, so it waits for robots.txt, the
host's delay and a `per_domain` slot, and goes out through the host's
session and proxy. But:

- its outcome goes to a oneshot channel the caller awaits, not to the
  events, so the loop never has to read it;
- it starts while pages are held back, for room in the backlog or for
  `max_pages`, so a stage can wait for it in the middle of a batch;
- it goes ahead of its host's pages (the highest priority), since an
  item is waiting on it;
- it skips what makes a request a page: `allowed_domains`, `max_depth`,
  `max_pages_per_domain`, the duplicate filter, deltafetch, the
  checkpoint, the shared queue and Chrome, and it isn't counted toward
  `max_pages`. A redirect it follows isn't marked seen.

The scheduler finds downloads by id in a map of waiters, so neither
`CrawlRequest` nor the checkpoint changed. Every way a request ends
(fetched, failed, dropped by robots.txt or TDMRep, blocked) goes through
one function, which hands a download's outcome to its waiter and counts
it in `fetched`, `bytes` and the rest as any response is counted.

Pages' priorities are kept below a download's, so a download is first
in its host's queue. A host is listed in a second ready queue,
`ready_downloads`, whenever a download is first in its queue, and while
pages are held the scheduler visits only that queue: the state the crawl
is in for as long as a stage waits, so its work stays with the hosts
that have downloads, not with every host the crawl knows.

`_Run.download` gives a request id from the same count as the crawl's
own and awaits the engine. The stage reaches the run through `_attach`,
which the run calls for `netweir.files` and `netweir.images` before it
opens.

A file goes out as a page's script asks for one (`Kind::Fetch`, a GET
with `Sec-Fetch-Dest: empty`); an image as an `<img>` does, through a
new `Kind::Image` and an `[image]` shape in each profile:
`Sec-Fetch-Dest: image`, `Sec-Fetch-Mode: no-cors`, the browser's image
Accept and its image priority, with the item's page as Referer (the
page a rule extracted the item from, too). Never a navigation.

Chrome's and Firefox's shapes are written from recordings, kept beside
the profiles as `*.image.capture.json` and `*.image.http1.capture.json`,
and the fingerprint test sends the same request and compares it, as it
does navigations and forms. They were recorded from Google Chrome 155
and Firefox 157.0.1, a version on from the profiles, so the test takes
the headers that name a version (User-Agent, client hints, and Chrome's
Accept, to which 155 added `image/jxl`) from the profile and holds the
rest, order and HTTP/2 priority included, to the recording. Chrome sent
`priority: u=2, i` at HTTP/2 weight 182 in five runs of six; the sixth,
an image alone on a new connection, went at `u=1, i`. Firefox sends
`Priority: u=4, i` over both protocols and weight 13 over HTTP/2.

## Dedupe and expiry

Within a crawl, each URL's download is one asyncio task, kept by URL;
every item that names the URL awaits the same task, so a URL in a
thousand items is fetched once, and a failure isn't retried by the next
item either. Each item gets its own copy of the entry.

Across runs, a stored file younger than `expires` is `"uptodate"`. The
age is the file's mtime locally and fsspec's `modified()` in a remote
store; a filesystem that doesn't implement it gets every file fetched
again. A URL with no usable extension is found by globbing
`full/<sha1>*`, since its extension came from the Content-Type.

An up-to-date file's checksum is worked out by reading it in a local
store. A remote one isn't downloaded for it, or every run would download
every file it skips: the checksum is the store's own record, read from
`info()` (S3's ETag when it's a plain MD5, which it isn't after a
multipart upload; Google Cloud Storage's `md5Hash`), and None where
there's none. Known limit: an S3 object encrypted with SSE-KMS has an
ETag that isn't its MD5, so its checksum is wrong, as in Scrapy. A sidecar file could keep it, but would put files in the
store that Scrapy's layout doesn't have.

## Images

Pillow is an optional dependency (`netweir[images]`), imported when
`netweir.images` is made, with an error that names the extra. Each image
is opened, checked against `min_size`, converted to RGB (transparency
laid over white, as a JPEG can't hold it), saved as a JPEG, and
thumbnailed with `Image.thumbnail`, which keeps the aspect ratio and
never enlarges. All of it runs in `asyncio.to_thread`: it's CPU work,
and the crawl's event loop shouldn't wait on it.

Memory sets the limits. A 64 MB body can decode to several hundred MB,
and conversion and thumbnails copy it. Pillow itself only refuses above
about 179 million pixels. So `max_pixels` (25 million by default) is
checked against the header before anything is decoded, and a bigger
image fails (above Pillow's own limit, about 179 million pixels, a
larger `max_pixels` has no effect; a multi-frame image is judged and
stored by its first frame); and two images are converted at once, by a semaphore made
for each crawl. Plain files are stored sixteen at a time.

Pillow 12 has wheels for every Python Netweir supports, free-threaded
3.14 included.

## Safety

A stored file's path is `full/` or `thumbs/<name>/`, then the SHA1 of the
URL, then an extension from a fixed table of Netweir's (`_EXTENSIONS` in
`_files.py`, about fifty, each with its type). Not Python's mimetypes:
its table changes between versions (3.10 knows neither `.webp` nor
`.avif`, 3.14 both, and `.php` too) and reads the system's files, so the
same URL would be named differently on two machines and a store filled
on one would look empty to the other. Script extensions aren't in it. A
thumbnail's name must be letters, digits, `_` and `-`. Nothing from the
URL or the response reaches the path any other way. A `file://` store
is turned into a path by `url2pathname`, so `file:///C:/x` is `C:\x`
on Windows. Bodies are capped by `max_response_size`, and the
fetcher refuses a redirect to anything but http(s).

## Checkpoints, workers and the shared queue

Downloads are never in the checkpoint. An item whose files were
downloading when the crawl stopped wasn't delivered, so its page wasn't
acknowledged; the resumed crawl fetches the page again, the stage asks
for the files again, and what was already stored is `"uptodate"`.

Pipelines run in the main process with `workers`, so downloads work the
same. With a shared queue, each process downloads for the items it
handles; dedupe is per process, and a shared remote store is what lets
one process's file be another's `"uptodate"`.

## Limits

- Known limit: the crawl loop waits while an item's files download, so
  items are handled one at a time. Pages keep being fetched meanwhile, up
  to the backlog, and one item's files download at once. Handling items
  concurrently would mean the loop's per-callback state (the page being
  handled, item ids, deltafetch's record) per task, and items reaching
  the output out of order.
- Known limit: Safari's `[image]` shape is written from what Safari is
  known to send for an `<img>`, not from a recording as Chrome's and
  Firefox's are. The capture tool serves `/image`, a page with one
  `<img>`, to record one with.
- Known limit: images and a script's requests go on HTTP/2 connections
  of their own, because the HTTP/2 priority is set per client, while a
  browser sends them on the page's connection. A server that compares
  connections to requests can see it.
- Known limit: `max_time` isn't checked while a download is awaited, only
  between events, so a slow file can run the crawl past it by up to its
  timeout and retries. A host that trips the circuit breaker holds its
  download, and the crawl, for `breaker_pause` (300 s by default).
- Known limit: dedupe within a crawl keeps one entry per URL in memory,
  for the length of the crawl.
- No `file_path()`, `get_media_requests()` or `item_completed()` to
  override, and no FTP store: arguments cover the settings people use,
  and fixed names are what keeps paths inside the store.
