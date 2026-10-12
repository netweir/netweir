# Crawling

A spider is a class. netweir's engine, in Rust, runs the queue, robots.txt,
throttling and deduplication; your code says what to take from each page
and which links to follow.

## Spiders

```python
import netweir


class Quotes(netweir.Spider):
    start_urls = ["https://quotes.toscrape.com/"]
    settings = netweir.Settings(concurrency=32)

    async def parse(self, page):
        for quote in page.css("div.quote"):
            yield {
                "text": quote.css(".text::text").get(),
                "author": quote.css(".author::text").get(),
            }
        for request in page.follow_all(page.css("li.next a::attr(href)")):
            yield request
```

A callback receives a `Page` and yields (or returns) items and requests:

- An **item** is a dict or a dataclass instance.
- A **request** is `page.follow(href)`, which resolves `href` against the
  page (and its `<base href>`), or a `netweir.Request`.

A callback can be an async generator, a generator, a coroutine or a plain
function. Override `start()` instead of setting `start_urls` when the first
requests need more than a URL; it can be an async generator, a generator or
return a list.

`allowed_domains` keeps a crawl on the sites you mean it to visit:

```python
class Shop(netweir.Spider):
    start_urls = ["https://example.com/"]
    allowed_domains = ["example.com"]  # and www.example.com, shop.example.com...
```

A request to any other site is dropped, counted as `offsite` in the stats,
and logged once per site. A domain covers its subdomains, and one with a
port (`"example.com:8443"`) allows only that port. A request with
`dont_filter=True` goes anywhere. Put domains in the list, not URLs; a URL
is an error.

## Requests

```python
netweir.Request(
    "https://example.com/item/1",
    callback="item",            # a method name, or a function
    errback="lost",             # called with (request, error) if no response arrives
    meta={"category": "tools"}, # comes back as page.meta
    priority=5,                 # higher first, among one site's requests
    headers={"Accept-Language": "de-DE"},
    dont_filter=False,          # True fetches it even if seen before
    browser=False,              # True fetches it in Chrome
    method="GET",               # or POST, PUT...; with one of form=, json=, body=
    referer="https://example.com/",  # the page it comes from; page.follow sets it
    proxy="http://10.0.0.9:8080",    # this one request goes through this proxy
)
```

`page.depth` is how many links the crawl followed from a start page to get
here. A request the same as one already seen (same method, URL and body,
ignoring fragments, query order and tracking parameters such as `utm_*`) is
dropped unless `dont_filter=True`. Forms, JSON and other bodies are in
[Forms, logins and APIs](forms.md).

A request with `browser=True` is loaded in Chrome, so the callback sees the
page after its JavaScript ran, and `page.browser` is the live page for
clicking and typing until the callback returns. See
[Browser](browser.md#in-a-crawl).

## Sitemaps

Most sites list their pages in sitemaps, which is often the quickest way to
reach every product or article. Point `sitemap_urls` at a sitemap, a
sitemap index, or the site's robots.txt (whose `Sitemap:` lines name them),
and say which callback each kind of page goes to:

```python
class Shop(netweir.Spider):
    sitemap_urls = ["https://example.com/robots.txt"]
    sitemap_rules = [
        ("/product/", "product"),  # the first regex a URL matches wins
        ("/blog/", "post"),
    ]
    sitemap_follow = ["/sitemap-products"]  # which sitemaps of an index to read

    def product(self, page):
        yield {"name": page.css("h1::text").get()}

    def post(self, page):
        ...
```

Pages no rule matches are skipped; with the default rules every page goes
to `parse`. `sitemap_follow` defaults to every sitemap an index lists.
Sitemaps can be XML (`urlset` or `sitemapindex`), gzipped (`.xml.gz`), or
plain text with one URL per line. `sitemap_alternate_links = True` also
crawls the other-language versions a sitemap gives for a page
(`xhtml:link rel="alternate"`).

To choose entries by what the sitemap says about them, override
`sitemap_filter`. Each entry is a dict with `loc`, `lastmod`,
`changefreq`, `priority` (None where the sitemap doesn't say) and
`alternates`:

```python
class Recent(netweir.Spider):
    sitemap_urls = ["https://example.com/sitemap.xml"]

    def sitemap_filter(self, entries):
        for entry in entries:
            if (entry["lastmod"] or "") >= "2026-10-01":
                yield entry
```

A sitemap that isn't valid, or ends part way through, is logged and
skipped. Sitemaps are read no further than `max_response_size`, after
unzipping, and XML entities a sitemap declares are never expanded, so a
hostile one can't fill memory.

`sitemap_urls` and `start_urls` can be used together.

## Running

```python
stats = Quotes().run(output="quotes.jsonl")
```

`run()` crawls to the end and returns the stats: pages fetched and failed,
items, duplicates, pages robots.txt kept out, retries, blocks and more.
`await spider.crawl(output=...)` does the same inside an event loop you
already have.

From the shell:

```
netweir crawl quotes.py -o quotes.jsonl -s concurrency=16 -s max_depth=3
```

`-s` sets any [setting](settings.md); `--spider NAME` picks one when the file
defines several; `-q` shows only warnings and errors. A mistake on the
command line (an unknown setting, a bad value, a path that can't be
written) exits with status 2 and one line saying why.

### Stopping early

A crawl that should end before it runs out of pages takes a limit:

```
netweir crawl quotes.py -o quotes.jsonl -s max_items=500 -s max_time=600
```

`max_items` stops it once that many items are written, exactly that many.
`max_pages` sends that many requests, a retry counting as one, and stops
once their pages have been through the callbacks. `max_errors` stops it
once callbacks and pipelines have raised that many exceptions,
`max_time` after that many seconds, and `max_memory` once the crawl has
used that many MiB of memory; requests still in flight when one of those
four stops it are dropped, not waited for.

`max_memory` counts the most memory the crawl has held at once, its
peak, as the operating system reports it, and adds each worker process's
peak when there are `workers`. It's read about once a second, waiting
pages included, so a crawl can pass the limit by what it takes in that
second before it stops. A worker reports its peak each time a callback
returns, so with `workers` one callback's memory can go over too. Leave
room. `memory_warning` logs a warning, once, at a lower mark, to
see a crawl's growth before it's stopped:

```
netweir crawl books.py -o books.jsonl -s memory_warning=1500 -s max_memory=2000
```

After a run, `spider.finish_reason` says why it ended: `"finished"` when
there was nothing left to fetch, or the name of the limit.

With a [checkpoint](self-healing.md#checkpoints), running a stopped crawl again carries on
from where it stopped, and the limits count afresh: `max_items=500` gives
the next 500.

## Caching while you develop

You'll run a spider you're writing thirty times before it's right. Without
a cache, that's thirty crawls of the same pages: thirty waits for every
polite delay, and thirty rounds of load on a site that never asked for any.
Give the crawl a directory and it keeps what it fetched:

```
netweir crawl quotes.py -s cache=.cache
```

or `settings = netweir.Settings(cache=".cache")` on the spider. Run it
again and every page it already has comes off your disk. No request goes
out, not even for robots.txt, and nothing waits. Your callback gets the
same page as before: URL, status, headers and body. A page that redirected
comes back as where it ended up, with no hops.

What's kept is what you'd want back. A block, a 401, 403 or 451, a 429, a
server error or a request that failed is never kept, so the next run asks
the site again rather than replaying the bad answer. Pages fetched in
Chrome aren't kept either: their callback gets a live page, and a stored
one can't stand in for that. Cookies a cached page set, on the way through
a redirect too, still reach the crawl's session, so a cached login keeps
you logged in.

A page is looked up by its method, URL and body, never its headers: two
POSTs with different bodies are two entries, but the same URL fetched with
two different `Accept-Language` headers is one. A page cached by a run that
ignored robots.txt or TDMRep isn't served to one that obeys them; that run
fetches it again.

Pages from the cache count in the stats as `cache_hits` (and in `fetched`,
since your callback got them, but not in `bytes`, since nothing was
downloaded), and toward `max_pages`, so a cached rerun of a 20-page crawl
still stops at 20. A checkpoint works with it as without.

A kept page stays until you clear it. Set `cache_expiry=3600` to fetch
anything older than an hour again. To start afresh, delete the directory.
Nothing is ever evicted, so the cache grows with every new page: fine for
the few hundred pages a spider in progress sees, not something to leave on
for a crawl of the whole site.

## Skipping what you already have

Say you crawl a shop's catalogue every morning. Yesterday's run wrote ten
thousand products, and today maybe forty are new. Without help, today's
run fetches all ten thousand again to find the forty. Give it somewhere to
remember what it found:

```python
class Shop(netweir.Spider):
    settings = netweir.Settings(deltafetch="state/shop")
```

or `-s deltafetch=state/shop`. Now a request whose page gave an item in an
earlier run is dropped before it's fetched, counted as `skipped_delta`.
Listing pages give links, not items, so they're never remembered: every run
reads them all and finds the forty. It's the idea behind scrapy-deltafetch,
with the differences listed below.

Your output keeps every run's items. With `deltafetch` set, each run adds to
its output file rather than starting it afresh, so `-o shop.jsonl` holds
yesterday's ten thousand and today's forty. That's what makes skipping safe:
the items of a page that's skipped are already in the file. JSON Lines, CSV
(one header, whose columns the next runs keep), JSON and XML files go on
from where the last run left them,
and a Parquet output gets a part per run beside it (`shop.1.parquet`,
`shop.2.parquet`), which pyarrow reads together as one dataset as long
as every run's items have the same fields and types (each part's columns
come from its own items). Batches
(`batch=`) go beside the last run's, numbered on from them (`shop-4.jsonl`
after `shop-3.jsonl`), and a run with nothing new writes no file at all.

If you'd rather keep each run's new items apart, move the file away when a
run is done, to an archive, say: the next run starts the file again and
writes only what's new. Or give each run a file of its own:

```
netweir crawl shop.py -s deltafetch=state/shop -o "shop-$(date +%F).jsonl"
```

What netweir won't do is write over a file it can't account for. An output
that's there but that the record has never seen, while the record already
has pages, is refused: its pages' items may be somewhere else, and starting
it afresh would lose what it holds. So is a file that has changed since the
record last saw it (another run, without `deltafetch`, wrote to it), and
one that has grown since the run that wrote it finished cleanly (you added
lines to it). Move it away, write to another path, or start afresh with
`deltafetch_reset=True`. The one case it can't tell apart: lines added by
hand to a file whose last run crashed look just like the half-written end
the crash left, and are cut off with it. A single file in cloud storage is refused too, as
it can't be added to; write a local file, or batches.

A page is remembered once its callback has finished and at least one of its
items has come out of every pipeline and reached the output file. An item a
pipeline or a validate stage dropped doesn't count, and neither does a page
whose callback raised part way: it may have had more to give, so it's
fetched again next run, and the items it gave before the error are written
a second time. Items from callbacks in worker processes and from
link rules' `Item`s count; items an errback or `on_block` yields don't, as
no page gave them.

If the crawl dies, killed or crashed, no item is lost, and an output that
is a single file (or a Parquet output and its parts) has nothing in it
twice. The record keeps how long each output file was, and a digest of its
end, each time it remembers pages, in the same commit; the next run cuts
the file back to that length, and the pages whose items were past it are
fetched again. A Parquet part gets its name only once it's on record with
its pages. With a checkpoint too, the checkpoint's commit always lands
first, so the two never disagree about what's in the file. Batches are
files of their own and aren't cut back: after a crash, the batch being
written may end in a half-written item, and items in it or in batches
finished since the last record are written again by the next run.

With a new record, or one just reset, an output file that's there is
started afresh: every page is about to be fetched again.

Pages are remembered by method, URL and body, as the duplicate filter
knows them, so `?b=2&a=1` and `?a=1&b=2` are one page, two POSTs with
different bodies are two, and a page that redirects is known by the URL
you asked for. When the same product turns up under several URLs, name it
yourself:

```python
yield page.follow(href, callback=self.product, meta={"deltafetch_key": sku})
```

The key is a str or bytes (`"sku-1"` and `b"sku-1"` are one key). Any
falsy value (`""`, `0`, `None`, an empty list) counts as no key, as in
scrapy-deltafetch, and anything else is an error that drops the request,
counted like a callback's error, whether it came from a callback or from
`start()`. `meta={"deltafetch_enabled": False}`
leaves a request out entirely: it's never skipped, and its page is never
remembered.

A request with `dont_filter=True` is fetched whatever it gave before. To
fetch everything again, set `deltafetch_reset=True` for one run, or delete
the directory; either way the output files start afresh with it, except
the part files earlier runs gave a Parquet output (`shop.1.parquet` and
on), which stay where they are: delete them yourself, or a reader that
takes the folder as one dataset reads their items again. A page
that `max_items` stopped part way is fetched again next run, so the items
it gave before the stop are written a second time.

Nothing is ever forgotten on its own: a product whose price changes
tomorrow keeps its old item. If you need changes, not only new pages,
reset now and then.

The record is in memory while the crawl runs: 16 bytes a page, up to
about 40 with the set that holds them, so some 40 MB for a million
products. Pages found during a run wait in Python until their items are on
disk, at 80 to 100 bytes each; with a Parquet output, that's until the end
of the run.

### Coming from scrapy-deltafetch

- Start requests are skipped too. scrapy-deltafetch only looks at what
  callbacks yield, so it fetches a start URL every run; netweir doesn't, so
  a start URL that is itself a product page is fetched once. Give it
  `dont_filter=True` to fetch it every run.
- A page is remembered once its items have been through the pipelines and
  are in the output, and only if its callback finished. scrapy-deltafetch
  remembers it as soon as the callback yields an item, before any pipeline
  has seen it.
- `dont_filter=True` fetches a remembered page; scrapy-deltafetch skips it
  regardless.
- Output files are added to, run after run, and cut back after a crash.
  Scrapy's feeds append too, but nothing ties them to what the record
  holds.
- Scrapy's `DELTAFETCH_RESET` setting, or `-a deltafetch_reset=1`, is
  `-s deltafetch_reset=true` here.

### Which of the three to use

- a [checkpoint](self-healing.md#checkpoints) carries one crawl on after
  it stops. Run a finished crawl again with the same checkpoint and it
  does nothing at all;
- `deltafetch` is for a crawl you run again and again: every run re-reads
  the listings, and only the pages that gave items are skipped;
- the cache is for writing the spider. Every page comes off your disk, and
  your callbacks run on all of them.

They work together. A checkpoint keeps today's run safe from a crash while
`deltafetch` keeps what every run found. A page is remembered only once the
checkpoint has committed its items, and a request read back from the
checkpoint is fetched even if `deltafetch` has its page, so a crash
between the two loses nothing.
## Keeping what you fetched

Items are what you wanted from a site, but sometimes you need the site
itself: a record you can show someone later, pages to parse again next
month without asking twice, or the exact request that got a strange
answer. Give the crawl a file and it keeps every exchange:

```python
settings = netweir.Settings(warc="crawl.warc.gz", har="crawl.har")
```

or `-s warc=crawl.warc.gz` from the shell. Use either or both.

The WARC file is the archive. It's the format web archives keep (WARC
1.1, each record gzipped on its own; name it `.warc` for no gzip), and
every response the crawl got is in it with the request that asked for it:
each hop of a redirect, every retry and block page, robots.txt and the
sitemaps, and the pages declarative rules followed in Rust. The request is
what netweir sent, header for header in the order they went out, cookies
included. Read it back with [warcio](https://github.com/webrecorder/warcio):

```python
from warcio.archiveiterator import ArchiveIterator

with open("crawl.warc.gz", "rb") as f:
    for record in ArchiveIterator(f, check_digests=True):
        if record.rec_type == "response":
            url = record.rec_headers.get_header("WARC-Target-URI")
            status = record.http_headers.get_statuscode()
            html = record.content_stream().read()
```

Bodies are kept the way your callback saw them, decompressed. So a gzipped
page is stored unzipped, with no `Content-Encoding` header and a
`Content-Length` that matches what's there, and the record's digest is of
those bytes. If you need the compressed bytes off the wire, this isn't
them. Response headers come as the HTTP client keeps them: lowercase
names, values byte for byte, and a header the server repeated (two
`Set-Cookie`s, say) with its values side by side. A redirect's body is
kept up to 64 KiB; past that, the record is marked truncated.

The HAR file is for looking. Open your browser's developer tools, go to the
Network panel and drop the file on it (Chrome and Firefox both import HAR):
you'll see every request the crawl made, its headers, the response and how
long it took. Images and other binary bodies are in there base64, as the
format wants.

Three things aren't in either file. A page served from the
[cache](#caching-while-you-develop) wasn't fetched. A page fetched in
Chrome is left out, because netweir only sees the page after its scripts
ran, never what the server sent. And a request that got no answer (a
timeout, or a body over `max_response_size`) has nothing to record.

Both files are written as the crawl goes, on a thread of their own,
straight from the pages the crawl already holds, so a 50 MB download
doesn't cost another 50 MB to archive. They're opened once your pipelines
have opened, so a crawl that fails to start leaves them as they were.
Without a checkpoint, each run starts them afresh. With one, a resumed crawl adds
to them: if the last run died part way through writing a record, that torn
record is cut off first, so the files stay readable. The HAR file only
becomes valid JSON when the crawl closes it; a resume closes it properly.
If the disk fills up, you get one warning naming the file, the crawl goes
on, and `archive_errors` in the stats counts what didn't get written.
[The design](design/archive.md) has the details.

## Callbacks in worker processes

The engine fetches and parses in Rust, but a spider's callbacks take turns
in one Python process. If yours do real work (heavy parsing, a model,
decoding images), they hold the whole crawl to one core. Spread them over
several:

```python
class Shop(netweir.Spider):
    settings = netweir.Settings(workers=4)
```

or `-s workers=4` from the shell. The engine, your pipelines, exporters
and the checkpoint stay in the main process; only callbacks move. You get
the same items, with the same ids, as with one process, and a checkpointed
crawl resumes the same way. `bench/workers.py` runs a spider whose
callbacks do 10 ms of work each: four workers finish it 3.1 times faster
than one, on a 12-core laptop.

What changes:

- Each worker builds its own spider from its class, so the class must be
  defined at module level, or in the file you gave `netweir crawl`. A
  script that runs a crawl with workers needs the usual
  `if __name__ == "__main__":` guard, as anything using multiprocessing
  does.
- Attributes a callback sets on the spider stay in that worker. To count
  or collect across pages, yield items and do it in a pipeline.
- Callbacks are passed between processes by name, so a request's callback
  should be a method of the spider (the same rule as a checkpoint). One
  that isn't runs in the main process instead; a request yielded in a
  worker must name a method.
- What travels between processes must pickle: items, `meta`, and the
  exception a callback raises. An item that can't go back ends that
  page's results there, counted as an error, as an exception would.
- An async callback runs on an event loop of its own in the worker, one
  per page.
- A spider defined in a notebook or the interactive prompt can't be
  rebuilt by a worker; put it in a file.
- Callbacks of browser requests run in the main process, where their live
  `page.browser` is.

Errors, warnings and tracked selectors work as they do in one process: a
callback's exception is logged and counted, and a relocated selector is
reported once per crawl. With `fail_fast`, the crawl stops as soon as the
error comes back from its worker, and callbacks still running are
stopped, not waited for. A worker that dies (out of memory, say) ends the
crawl with an error naming the page it was on; with a checkpoint, run it
again to carry on.

## Being polite

By default netweir reads each site's robots.txt (RFC 9309) and stays out of
what it disallows, skips pages whose owners reserve text and data mining
rights (TDMRep), and adapts its pace to each site: the delay between
requests moves toward the site's response time, and a 429 or a block slows
it further. Turning robots.txt off (`obey_robots=False`) logs a warning
once per crawl.

## Pipelines

Every item passes through the spider's pipelines, in order:

```python
def in_stock(item):
    return item if item["stock"] > 0 else None   # None drops it


async def with_currency(item):
    return {**item, "currency": "GBP"}


class Books(netweir.Spider):
    pipelines = [in_stock, with_currency, netweir.export.jsonl("books.jsonl")]
```

A stage is any function, sync or async, that returns the item, a changed
item, or `None` to drop it. An exception in a callback or a pipeline is
logged with the page's URL and counted, and the crawl carries on; set
`fail_fast=True` to stop at the first one.

A stage can also be an object with a `process_item(item, spider)` method,
which is how Scrapy writes pipelines, so one from a Scrapy project works
as it is. An object stage may have `open_spider(spider)`, called before
the first request, and `close_spider(spider)`, called once the crawl has
ended, however it ended:

```python
import sqlite3


class Database:
    def open_spider(self, spider):
        self.db = sqlite3.connect("books.db")

    def process_item(self, item, spider):
        self.db.execute("insert into books values (?, ?)", (item["title"], item["price"]))
        return item

    def close_spider(self, spider):
        self.db.commit()
        self.db.close()
```

Raising `netweir.DropItem` (or a subclass) in a stage drops the item, as
returning `None` does; Scrapy's `DropItem` works too. Any of these can be async. If
`open_spider` raises, the crawl stops before
it fetches anything, so a database that's down is found out at once, not
a thousand pages in.

## Middlewares

Code that belongs to every request or every page, rather than to one
callback, goes in a middleware: a header each request needs, a page to
fetch again before its callback sees it, a request to send elsewhere when
one fails. A middleware is an object with any of three methods, sync or
async, which work as Scrapy's downloader middlewares do:

```python
class Middleware:
    def process_request(self, request, spider):
        """Before a request is queued. Return None to go on with it (after
        changing it, if you like), or another Request to go on with
        instead. Raise netweir.IgnoreRequest to drop it."""
        request.headers["Authorization"] = "Bearer ..."

    def process_response(self, request, page, spider):
        """Before the callback gets the page. Return None or the page to go
        on with it, another Page to give the callback instead, or a Request
        to queue in its place. Raise netweir.IgnoreRequest to drop it."""
        if "Please wait" in page.text:
            return netweir.Request(page.url, callback=request.callback, dont_filter=True)

    def process_exception(self, request, error, spider):
        """When a request failed. Return a Request to queue instead of
        calling the errback, or None to carry on to the errback. Raise
        netweir.IgnoreRequest to drop the request without an errback."""


class Books(netweir.Spider):
    middlewares = [Middleware()]
```

A request's `headers` reach `process_request` as a dict (an empty one if
it had none), or as the list of pairs it was made with. `process_request`
runs in the list's order, and `process_response` and
`process_exception` in reverse, as in Scrapy: the first middleware is the
one nearest your spider. Each gets what the one before passed on. A
request or page that's dropped is counted in the stats as
`requests_dropped` or `responses_dropped`. A page given in place of
another answers the same request, so it keeps the request's `depth` and
`meta`, and a request given in place of another keeps its depth.

An exception in `process_request` or `process_response`, or a result
that isn't a Request, a Page or None, is counted and logged as a
callback's error would be, and drops the request or page. One in
`process_exception` is counted and logged, and the error goes on to the
next middleware. With `fail_fast=True`, any of them stops the crawl.

Middlewares see the requests your code makes, start requests and those a
callback yields, and the pages that come back to your callbacks. They
don't see:

- requests that link rules follow, which Rust queues itself, and pages a
  `Follow` without a callback or an `Item` rule handles in Rust. The
  links a rule finds on a page are queued as the page arrives, so
  dropping a page a rule also reads doesn't stop them being followed.
- block pages, which go to `on_block` once every retry is used up.

With `workers`, middlewares run in the main process, before a page goes
to a worker.

## Signals

An object in `middlewares` or `pipelines` hears about the crawl by having
methods with these names, sync or async. Each is given the arguments its
parameters name, as Scrapy gives them, so take the ones you need:

| method | when | arguments |
|---|---|---|
| `spider_opened` | before anything is fetched | `spider` |
| `spider_closed` | once the crawl has ended | `spider`, `reason` |
| `item_scraped` | an item came out of every pipeline | `item`, `response`, `spider` |
| `item_dropped` | a pipeline dropped an item | `item`, `response`, `exception`, `spider` |
| `request_dropped` | a middleware dropped a request | `request`, `spider` |

`response` is the page the item came from, or None for an item that
didn't come from a callback's page: one a link rule made in Rust, one a
callback yielded in a worker, or one an errback or `on_block` yielded. `exception` is the
`DropItem` a pipeline raised, or None if it returned None.

They come in Scrapy's order: every pipeline's `open_spider`, then
`spider_opened`; at the end, once the output files are complete, every
`close_spider`, then `spider_closed`. The spider itself can define
`opened()` and `closed(reason)`, called after the others. `reason` is
`spider.finish_reason` (`"finished"`, or the limit that stopped the crawl),
`"error"` when an exception ended it, or `"interrupted"` after Ctrl-C.

```python
class Counter:
    def spider_opened(self):
        self.items = 0

    def item_scraped(self, item):
        self.items += 1

    def spider_closed(self, reason):
        print(f"{self.items} items, {reason}")
```

An exception in `open_spider`, `spider_opened` or `opened()` stops the
crawl before anything is fetched or any output file is touched; whatever
had already opened is still closed. One in any other signal is logged
and the crawl carries on.

With `workers`, signals and `opened()`/`closed()` run in the main
process. A worker makes its own copy of the spider, which `opened()`
never ran on, so set up what callbacks need in `__init__` instead.

## Coming from Scrapy

Simple pipelines, middlewares and signal handlers written for Scrapy carry
over as they are: `process_item`, `open_spider`, `close_spider`,
`DropItem`, `process_request` setting headers, `IgnoreRequest`,
`spider.logger` and `request.replace()` all work. The differences:

- They're listed as objects (`pipelines = [Database()]`), not as dotted
  paths in settings, and nothing calls `from_crawler`: give an object what
  it needs when you make it. There's no `spider.settings.get()`; settings
  are attributes, as `spider.settings.concurrency`.
- `process_request` can't return a response to skip the fetch, and
  `process_exception` can't return one in place of the error.
- A Request that `process_request` returns carries on through the
  middlewares after it, rather than starting the chain again.
- `IgnoreRequest` from `process_request` or `process_response` drops the
  request there; Scrapy passes it on to `process_exception` and the
  errback.
- Spider middlewares (`process_spider_output` and the rest) and signals
  other than the five above aren't there.

### Or keep Scrapy, and fetch with netweir

Not ready to move a project over? Keep it on Scrapy and let netweir do the
fetching. Two lines in `settings.py`:

```python
DOWNLOAD_HANDLERS = {
    "http": "netweir.scrapy.DownloadHandler",
    "https": "netweir.scrapy.DownloadHandler",
}
NETWEIR_PROFILE = "chrome"  # or "firefox", "safari", or a version
```

Every request then goes out as the profile's browser sends it, from the
TLS handshake to the order of the headers, and Scrapy carries on as
before. Its middlewares still do their jobs:

- redirects: each hop is sent as the browser sends the request a redirect
  makes, so a start URL that redirects still looks typed, and a form that
  answers with a 303 lands with a GET;
- cookies, kept by Scrapy's cookie middleware and sent once;
- retries: netweir's errors arrive as Scrapy's own (`DownloadTimeoutError`,
  `DownloadConnectionRefusedError`, `CannotResolveHostError` and the
  rest), so they're retried as Scrapy's would be. A body over the size
  limit (`DownloadCancelledError`) or a request that can't be sent
  (`NotSupported`) isn't;
- proxies from `meta["proxy"]`, login included (sent as UTF-8), and
  `meta["proxy"] = None` for none at all: the system's proxy is never used
  behind Scrapy's back;
- `download_timeout`, and `DOWNLOAD_MAXSIZE` or `meta["download_maxsize"]`;
- AutoThrottle, which gets each response's `download_latency`.

The browser's own User-Agent and Accept-Encoding come from the profile;
Scrapy's `USER_AGENT` doesn't reach the site. Accept and Accept-Language
come from the profile too unless a request sets its own: a `JsonRequest`,
or a request with `Accept: application/json`, asks as a page's script
does, and gets its JSON. `meta["netweir_kind"]` (`"navigate"`, `"form"`
or `"fetch"`) chooses for a request that needs it. A body arrives already
decompressed. `DOWNLOAD_WARNSIZE` and `DOWNLOAD_FAIL_ON_DATALOSS` don't
apply: a body cut short is always an error. It needs Scrapy 2.15 or
later, on its default asyncio reactor.

## Checking items and crawls

A crawl that finishes isn't always a crawl that worked: a redesign can
leave every price empty, and a block can cut a crawl to a tenth of its
pages, and the crawl still ends with "finished". Two things catch that.

### Validating items

`netweir.validate(schema)` is a pipeline stage that checks each item.
`schema` is a dataclass, or a JSON Schema as a dict (for that,
`pip install jsonschema`):

```python
import dataclasses


@dataclasses.dataclass
class Book:
    title: str
    price: float
    stock: int | None = None


class Books(netweir.Spider):
    pipelines = [clean_price, netweir.validate(Book), netweir.export.jsonl("books.jsonl")]
```

With a dataclass, a field without a default must be there, and not None
unless its type allows None (`int | None`, `Any`), and each value must be
of its field's type: `str`, `int`, `float` (an int
is fine), `bool`, `list`, `dict`, `X | None`, and `list[X]` and
`dict[str, X]`, whose contents are checked too. Other annotations aren't
checked, and nor is a field whose type can't be found (one imported only
for type checkers), which is logged when the stage is made.

An item that fails is dropped and counted as `items_invalid` in the stats
(not as `items_dropped`, though `item_dropped` listeners hear of it). With
`validate(schema, drop=False)` it's kept, and a dict item gets its errors
added to an `_errors` field, such as `["price: required"]`. Either way,
`spider.invalid_fields` counts the errors by field, as `{"price": 12}`,
with list positions and numeric keys as `*` (`offers.*.price`), and the first error for
each field is logged with the page it came from. `items_validated` counts
the items checked, once each however many validate stages there are. The
`_id` a checkpoint adds and `_errors` aren't validated, so a schema that
allows no other properties still works.

### Checks at the end

`checks` run once the crawl has ended, whether it ran out of pages or a
limit stopped it:

```python
class Books(netweir.Spider):
    checks = [
        netweir.Checks(
            min_items=900,
            max_error_rate=0.02,
            coverage={"title": 1.0, "price": 0.95},
            finish_reasons=("finished",),
        )
    ]
```

| check | fails when |
|---|---|
| `min_items` | fewer items than this |
| `max_error_rate` | failed fetches and callback errors, out of pages fetched plus failed fetches, are more than this fraction |
| `max_invalid` | items that failed `validate`, out of those it checked, are more than this fraction |
| `coverage` | a field is there, and not empty, in less than its fraction of items (None, "", [] and {} are empty) |
| `finish_reasons` | the crawl ended for another reason, such as a limit |

A check can also be a function of `(stats, spider)`, sync or async. It
passes by returning None or True, and fails by returning False, a
message, a list of messages, or anything else. One that raises counts as
failed. `checks` can be one check rather than a list.

Checks run once the output files are complete, so a check can read them.
After the run, `spider.check_failures` lists what failed (it's empty if
every check passed), `stats["checks_failed"]` counts it, and each failure
is logged as a warning. They're ready before `closed()` and the
`spider_closed` signal, so a handler can send them on by email or chat.
`netweir crawl` exits with status 1 when a check failed, so a scheduler or
CI job notices.

The stats and counts are the run's own. A crawl resumed from a checkpoint
checks only what that run did, so `min_items` and `coverage` judge part
of the whole; set them for one run's share, or check the output file in
a check of your own.

Every crawl counts its fields, checks or not: `spider.coverage` is
`{field: items it was present and not empty in}`, to set beside
`stats["items"]`.

## Exporters

`run(output=...)`, `-o` and the exporters write items in Rust:

| | File | Notes |
|---|---|---|
| `netweir.export.jsonl(path)` | `.jsonl`, `.jl` | one JSON object per line, keys in the item's order |
| `netweir.export.json(path)` | `.json` | one JSON array, an item a line, as Scrapy writes it |
| `netweir.export.xml(path, root="items", item="item")` | `.xml` | Scrapy's layout: an `<item>` per item inside `<items>` |
| `netweir.export.csv(path, fields=None)` | `.csv` | columns are `fields`, or the first item's keys; others are left out with one warning |
| `netweir.export.parquet(path)` | `.parquet` | column types from the first 1,000 items; values that don't fit are stored as null, with a warning |

Dates become ISO 8601 strings, `Decimal`s and integers too big for 64 bits
become strings, and NaN becomes null. Each run writes the file afresh,
except a run that [resumes from a checkpoint](self-healing.md#checkpoints),
which adds to it, a JSON or XML file included: the new items go after the
last one and the file stays valid.

In XML, a dict's keys become elements and a list's values become `<value>`
elements, as in Scrapy, and `None` is an empty element. Booleans are
written `true` and `false`. A key that isn't a valid element name has
each character a name can't hold replaced with `_`, and `_` put in front
if it starts with a digit: `"unit price"` is written `<unit_price>`. The
names keep to the stricter, older XML rules, so every XML parser reads
the file.
Characters XML can't hold at all, such as most control characters, are
left out.

### Cloud storage

Give any exporter, or `-o`, a URL such as `s3://bucket/books.jsonl` or
`gs://bucket/books.parquet`, and the file goes there. It's written on
your disk as the crawl goes and uploaded once it's finished, through
[fsspec](https://filesystem-spec.readthedocs.io/), which you install with
the package for your storage:

```
pip install fsspec s3fs     # Amazon S3
pip install fsspec gcsfs    # Google Cloud Storage
```

Credentials come from where those packages look for them, such as
`AWS_PROFILE` or `GOOGLE_APPLICATION_CREDENTIALS`. A missing package or a
scheme fsspec doesn't know is an error when the exporter is made, before
the crawl starts. If the upload fails, the error names the local file,
which is kept. A crawl with a checkpoint can't write to cloud storage,
since resuming adds to the output file; write to a local file and upload
it when the crawl is done.

### Batches

`batch=N` starts a new file every `N` items. Put `{batch}` (1, 2, 3 ...)
in the path to name each one, and if you like `{time}` too, for the time
the batch began, in UTC (`2026-10-12T03-04-05Z`). Any other braces in the
path are written twice, `{{` and `}}`:

```python
class Books(netweir.Spider):
    pipelines = [netweir.export.jsonl("books-{batch}.jsonl", batch=1000)]
```

```
netweir crawl books.py -o "books-{batch}.jsonl" --batch 1000
```

Each batch is finished, and uploaded if it goes to cloud storage, as soon
as it's full, so the files appear while the crawl runs. A crawl with no
items still writes the first batch, empty. Each run numbers its batches
from 1 again, so files a longer earlier run left with higher numbers stay
beside the new ones; add `{time}`, or clear the folder first, to keep
runs apart. Batches can't be combined with a checkpoint.
