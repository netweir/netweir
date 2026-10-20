# Crawling

A spider is a class. Netweir's engine, in Rust, runs the queue, robots.txt,
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
    steps=[steps.click("#more")],    # done in Chrome before the callback (netweir.steps)
    method="GET",               # or POST, PUT...; with one of form=, json=, body=
    referer="https://example.com/",  # the page it comes from; page.follow sets it
    proxy="http://10.0.0.9:8080",    # this one request goes through this proxy
    timeout=10,                 # seconds, in place of the crawl's timeout
    max_redirects=3,            # in place of the crawl's max_redirects
    allow_redirects=False,      # the callback gets the redirect itself
    auth=("ada", "secret"),     # HTTP Basic, to this request's own site
    cookies={"currency": "EUR"},     # into its jar, and sent; meta["cookiejar"] names the jar
)
```

With `allow_redirects=False` the callback gets the 3xx as its page:
`page.status` is 301 or 302, and `page.headers["location"]` says where it
pointed. `auth` goes to the request's own origin (scheme, host and port)
and is dropped if a redirect leaves it, as a browser drops it; for a login
to a whole domain, use the `http_auth` [setting](settings.md#fetching).
A login written into the URL (`https://ada:secret@intranet/`) becomes
`auth`. Scrapy's meta keys work too, when the field isn't set:
`download_timeout`, `dont_redirect`, `redirect_ttl` and `http_user`,
`http_pass` and `http_auth_domain`.

A request's login is saved with it in a [checkpoint](self-healing.md#checkpoints)
and in a [shared queue](distributed.md). On Linux and macOS the
checkpoint's files are readable by you alone; Redis is as safe as you
keep it.

The highest priority a page gets is one below the largest 32-bit
integer, which is kept for file downloads, so priorities from there up
tie and go in the order they were submitted.

`page.depth` is how many links the crawl followed from a start page to get
here. A request the same as one already seen (same method, URL and body,
ignoring fragments, query order and tracking parameters such as `utm_*`) is
dropped unless `dont_filter=True`. Forms, JSON and other bodies are in
[Forms, logins and APIs](forms.md); cookies, several logged-in users in one
crawl and starting a crawl logged in are in [Sessions and cookies](sessions.md).

A request with `browser=True` is loaded in Chrome, so the callback sees the
page after its JavaScript ran, and `page.browser` is the live page for
clicking and typing until the callback returns. See
[Browser](browser.md#in-a-crawl). For the clicking and typing every page
needs (a cookie banner, "Load more", a search box), give the request
`steps` instead, and Chrome does them before the callback sees the page.
See [Steps](steps.md).

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

Sitemaps are read in one pass, without building a tree, so one of
hundreds of thousands of URLs costs little more memory than its entries.
They're read in recover mode (see
[Broken and hostile XML](xml.md#broken-and-hostile-xml)), since so many
are a little broken: an undeclared `image:` or `xhtml:` prefix, an
`&nbsp;`, a stray `&`, a control character, a PHP warning after the end
or an attribute given twice is repaired, with one warning for the
sitemap saying what was. One that can't be read is logged and skipped.
Sitemaps are read no further than `max_response_size`, after unzipping,
for no more than 10 seconds each, and with the same limits as any XML:
nothing is fetched, and entities a sitemap declares are expanded only so
far, so a hostile one can't fill memory. Element names are matched
without regard to case or namespace, as sitemaps in the wild vary in
both.

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

Inside a [project](project.md), give a spider's name instead of a file:
`netweir crawl quotes -o quotes.jsonl`. `netweir runspider` is another name
for `netweir crawl`, as in Scrapy.

`-s` sets any [setting](settings.md); `--spider NAME` picks one when the file
defines several; `--queue redis://host:6379/0` shares the crawl with
processes on other machines ([One crawl, many machines](distributed.md));
`-q` shows only warnings and errors. A mistake on the
command line (an unknown setting, a bad value, a path that can't be
written) exits with status 2 and one line saying why.

`-a NAME=VALUE` sets an attribute of the spider, as a string, the way
Scrapy's spider arguments do: with `-a category=fiction`, `self.category`
is `"fiction"`, in worker processes too. `--stats stats.json` writes the
crawl's stats to a file when it ends, however it ends, with
`finish_reason` among them, and keeps it up to date while the crawl runs
([Watching a crawl as it runs](#watching-a-crawl-as-it-runs)).

Ctrl-C stops a crawl cleanly: the items so far are closed off in the
output file, a checkpoint records where it got to, and the command exits
with status 130. SIGTERM, which `kill` and service managers send, does the
same, and so does Ctrl-Break on Windows. After a run in Python,
`spider.stats` holds the stats too, including those of a run that was
interrupted or raised.

To run spiders as jobs that you schedule, watch and cancel over HTTP, as
scrapyd does for Scrapy, see [Running spiders as a service](serve.md).

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

What Netweir won't do is write over a file it can't account for. An output
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
  callbacks yield, so it fetches a start URL every run; Netweir doesn't, so
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
what Netweir sent, header for header in the order they went out, cookies
included, except logins: `Authorization` and `Proxy-Authorization` are
left out, since archives get shared. Read it back with [warcio](https://github.com/webrecorder/warcio):

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
Chrome is left out, because Netweir only sees the page after its scripts
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

By default Netweir reads each site's robots.txt (RFC 9309) and stays out of
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

## Downloading files and images

Sometimes the text isn't the whole job: you want the PDF datasheet each
product page links to, or the cover of every book. List the URLs on the
item and add a stage that fetches them:

```python
import netweir


class Books(netweir.Spider):
    start_urls = ["https://books.toscrape.com/"]
    pipelines = [
        netweir.images("covers/", thumbs={"small": (50, 50)}),
        netweir.export.jsonl("books.jsonl"),
    ]

    def parse(self, page):
        for book in page.css("article.product_pod"):
            yield {
                "title": book.css("h3 a::attr(title)").get(),
                "image_urls": [page.urljoin(book.css("img::attr(src)").get())],
            }
```

Each book then comes out with a note of what was stored for it:

```
{"title": "A Light in the Attic",
 "image_urls": ["https://books.toscrape.com/media/cache/2c/da/2cda...e8b.jpg"],
 "images": [{"url": "https://books.toscrape.com/media/cache/2c/da/2cda...e8b.jpg",
             "path": "full/5f1c0a6a...9d1e.jpg",
             "checksum": "b9d2e6...c41a",
             "status": "downloaded"}]}
```

`netweir.files(store)` does the same for any kind of file, reading
`file_urls` and writing `files`. Those are Scrapy's field names, so items
built for Scrapy's FilesPipeline and ImagesPipeline work as they are;
`urls_field=` and `result_field=` pick others.

Where things go:

- `store` is a directory, or a URL such as `s3://bucket/books` that fsspec
  can write to (install the same packages as for [cloud
  storage](#cloud-storage)). Each file is uploaded the moment it's
  stored, not at the end of the crawl.
- A file is stored at `full/<SHA1 of its URL>.<extension>`. The
  extension comes from the URL if it's one Netweir knows (pdf, jpg, png,
  webp, avif, zip, docx and four dozen like them; not php or asp),
  otherwise from the Content-Type, otherwise there's none. The list is
  Netweir's own, so a URL gets the same name on every Python and every
  machine. Images are always `full/<SHA1>.jpg`, with each thumbnail at
  `thumbs/<name>/<SHA1>.jpg`.
- That's Scrapy's layout, and for most files Scrapy's names too. They
  differ where Scrapy would write the URL differently before hashing it
  (Scrapy percent-encodes it first, so a URL with spaces or non-ASCII
  characters hashes differently), and where the extension would differ:
  Scrapy never takes one from the Content-Type, and asks Python's
  mimetypes, so `.jpeg` there is `.jpg`.
- `checksum` is the MD5 of what was stored, as in Scrapy. For a file
  that's `"uptodate"` in a local store, the file is read to work it out.
  In a cloud store it isn't downloaded again just for that: the checksum
  is what the bucket records (S3's ETag for a file uploaded in one piece,
  Google Cloud Storage's MD5), and `None` where it records nothing
  usable. An S3 object encrypted with SSE-KMS has an ETag that isn't its
  MD5, so the checksum is wrong there, as it is in Scrapy.

`status` says how the file got there. `"downloaded"` means it was
fetched; `"cached"` that it came from the crawl's [cache](#caching-while-you-develop);
`"uptodate"` that the store already had it, written less than `expires`
ago, so nothing was fetched. `expires` is 90 days unless you say
otherwise, in seconds or as a `timedelta`; `None` keeps a file for good.
For a store in the cloud, Netweir asks fsspec when the file was written;
S3, Google Cloud Storage and fsspec's own memory store say, and where a
filesystem doesn't, the file is fetched every run.

The same URL in a thousand items is fetched once a crawl. A download that
fails (a 404, a timeout, robots.txt saying no, a bucket that refuses the
upload) is logged and left out of the item's list, and the item carries
on to the next stage, as in Scrapy. Later items naming the same URL get
the same answer without another try.
The crawl's stats count them: `files_downloaded`, `files_cached`,
`files_uptodate` and `files_failed`, plus `images_too_small` for
`netweir.images`.

### The downloads are the crawl's own

Files aren't fetched by a separate client on the side. They go through
the same engine as the pages, so everything that keeps the crawl polite
and unblocked applies to them too: robots.txt and TDMRep, each site's
delay and `per_domain`, the session's cookies, the proxies and their
rotation, retries, the cache, the WARC and HAR files, and
`max_response_size`. A file is fetched in the cookie jar of the request
whose page gave the item (its `meta["cookiejar"]`, or the crawl's own),
so a file behind a login comes with the login. A URL is still fetched
once a crawl, in the jar of the first item to name it, and stored at
`full/<SHA-1 of the URL>.<ext>`, as Scrapy stores it. With
`netweir.files(store, per_jar=True)` (or `netweir.images`), each named
jar fetches its own copy and keeps it at `full/<SHA-1>.<ext>` of the
jar's name as the crawl names it (`meta["cookiejar"]` as JSON, so
`"ada"` with its quotes), a NUL character and the URL; the crawl's own
jar keeps the URL's SHA-1. A file goes out as a page's own script would ask for
it, and an image as an `<img>` on the page would, with the page as the
Referer:

```
Sec-Fetch-Dest: image
Sec-Fetch-Mode: no-cors
Accept: image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8
```

What a download isn't is a page. It doesn't count toward `max_pages` or
`max_pages_per_domain`, isn't held to `allowed_domains` (covers often
live on another domain, a CDN), isn't parsed, followed or handed to a
callback, isn't remembered by deltafetch, and never goes to Chrome. It
also goes ahead of the site's waiting pages, since an item is waiting
for it.

That last part matters, because the crawl waits too. While an item's
files download, the engine keeps fetching pages, but your callbacks and
pipelines don't run again until that item's files are stored. One
item's files download at the same time, as many as `per_domain` allows.
A slow file holds the crawl for as long as it takes, retries included,
and `max_time` isn't looked at meanwhile. So is a file on a site that
keeps blocking: the [circuit breaker](settings.md) pauses that
site, `breaker_pause` (five minutes by default), and the file waits it
out.

### Images

`netweir.images` needs Pillow:

```
pip install "netweir[images]"
```

Every image is stored as an RGB JPEG; a transparent one is laid over
white first. `thumbs` names the thumbnails you want and the box each one
fits in, keeping the image's shape, and a thumbnail is never bigger than
the image. `min_size=(width, height)` skips anything smaller, says so in
the log and counts it in `images_too_small`:

```python
netweir.images("s3://my-bucket/covers", thumbs={"small": (50, 50), "big": (270, 270)}, min_size=(100, 100))
```

Converting and resizing happen on a thread, not in the crawl's event
loop, two images at a time. An image's pixels take far more memory than
its file: a 64 MB PNG can need several hundred MB to decode. So an image
with more than `max_pixels` pixels (25 million by default, a 5000 × 5000
picture) fails without being decoded; raise it if you need bigger. Pillow
refuses anything above about 179 million pixels itself, so a `max_pixels`
above that changes nothing. An animated GIF or WebP is judged, converted
and stored by its first frame.

### Checkpoints, workers and shared queues

A download is never saved in a [checkpoint](self-healing.md#checkpoints).
If a crawl stops while an item's files are downloading, that item hadn't
been written yet, so the resumed crawl fetches its page again, and the
files with it; whatever was already stored comes back as `"uptodate"`. A
file only appears under its final name once it's complete, so a crawl cut
off part way never leaves half a file behind.

With `workers`, pipelines run in the main process, downloads included,
and nothing changes. With a [shared queue](distributed.md), each process
downloads the files of the items it handles and only skips repeats it
has seen itself. Point every process at the same bucket and a file one
of them stored is `"uptodate"` for the rest; give each a local directory
and each machine keeps its own copies.

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
        changing it, if you like), a Page to answer it without fetching
        anything, or another Request to schedule in its place. Raise
        netweir.IgnoreRequest to drop it."""
        request.headers["Authorization"] = "Bearer ..."

    def process_response(self, request, page, spider):
        """Before the callback gets the page. Return None or the page to go
        on with it, another Page to give the callback instead, or a Request
        to queue in its place. Raise netweir.IgnoreRequest to drop it."""
        if "Please wait" in page.text:
            return netweir.Request(page.url, callback=request.callback, dont_filter=True)

    def process_exception(self, request, error, spider):
        """When a request failed, or process_request dropped it. Return a
        Request to queue instead of calling the errback, a Page to answer
        the request with, or None to carry on to the errback."""


class Books(netweir.Spider):
    middlewares = [Middleware()]
```

A request's `headers` reach `process_request` as a dict (an empty one if
it had none), or as the list of pairs it was made with. `process_request`
runs in the list's order, and `process_response` and
`process_exception` in reverse, as in Scrapy: the first middleware is the
one nearest your spider. Each gets what the one before passed on.

Say you keep pages on disk and want to crawl them without touching the
site: return them from `process_request`, and nothing is fetched. The
page still goes through every middleware's `process_response` and on to
its callback, with the request's `depth` and `meta`. The crawl still
checks the request as if it were going to fetch it, so a duplicate, an
offsite request or one past `max_depth` gets no page, and a store of
pages that link to each other can't send the spider round in circles.
Each served page counts toward `max_pages` and `max_pages_per_domain`
as a fetched one does, and in the stats as `pages_served`. On a rules
spider, the rules read a served page of a request with no callback, as
they read a fetched one. A served request isn't kept in a checkpoint, so
a resumed crawl asks the middleware for it again.

A Request that `process_request` returns is scheduled in place of the one
it was given, and goes through the middlewares again from the first, as
in Scrapy. A middleware that returns the request it was given carries on
with it.

`IgnoreRequest` ends at the request's errback, as in Scrapy, which gets
the exception as `error`; without an errback, the request is dropped
quietly. From `process_request` it goes through `process_exception`
first, which can still save the request; from `process_response` or
`process_exception` it goes straight to the errback. A request or page
that's dropped is counted in the stats as `requests_dropped` or
`responses_dropped`. A page given in place of another answers the same
request, so it keeps the request's `depth` and `meta`, and a request
given in place of another keeps its depth.

An exception other than `IgnoreRequest` in `process_request` or
`process_response`, or a result that isn't a Request, a Page or None, is
counted and logged as a callback's error would be, and drops the request
or page. One in `process_exception` is counted and logged, and the error
goes on to the next middleware. With `fail_fast=True`, any of them stops
the crawl.

Middlewares see the requests your code makes, start requests and those a
callback or signal handler yields, and the pages that come back to your
callbacks. They don't see:

- requests that link rules follow, which Rust queues itself, and pages a
  `Follow` without a callback or an `Item` rule handles in Rust. The
  links a rule finds on a page are queued as the page arrives, so
  dropping a page a rule also reads doesn't stop them being followed.
- block pages, which go to `on_block` once every retry is used up.

With `workers`, middlewares run in the main process, before a page goes
to a worker.

### Spider middlewares

Scrapy's other kind of middleware sits between the crawl and your
callbacks, and sees what goes in and what comes out. List them in
`spider_middlewares`, in the order of their numbers in Scrapy's
`SPIDER_MIDDLEWARES`, lowest first:

```python
class OnlyInStock:
    async def process_spider_output(self, response, result, spider):
        """What the callback gave, on its way out: yield what goes on."""
        async for out in result:
            if isinstance(out, dict) and out.get("stock") == 0:
                continue
            yield out

    def process_spider_exception(self, response, exception, spider):
        """The callback raised: return items and requests to give instead,
        or None to leave the error as it is."""
        spider.logger.warning("%s: %s", response.url, exception)
        return []


class Books(netweir.Spider):
    spider_middlewares = [OnlyInStock()]
```

`process_spider_input(response, spider)` runs before the callback and
returns None or raises; an exception goes to the request's errback, or
without one to `process_spider_exception`. `process_spider_output` can be
an async generator, which gets the output as it comes, or a plain
generator, which gets it as a list once the callback has finished.
`process_start(start)`, an async generator, sees `start()`'s requests
before they're queued, and Scrapy's older `process_start_requests(
start_requests, spider)` works too.

`process_spider_input` runs in the list's order and the rest in reverse,
as in Scrapy: the first is nearest the crawl, the last nearest your
callback. An exception goes to the `process_spider_exception` of the
middlewares nearer the crawl than where it was raised, and what one of
them returns goes on through the `process_spider_output` of the
middlewares nearer the crawl than it. An exception none of them takes is
the callback's error, counted and logged as before.

They see what callbacks get, whether a page was fetched, sent to a
callback by a rule, or given by a middleware, but not errbacks for failed
requests or `on_block`, as in Scrapy. With `workers`, they run in the
main process, around the callback that runs in the worker.

## Signals

An object in `middlewares`, `spider_middlewares` or `pipelines` hears
about the crawl by having methods with these names, sync or async. Each
is given the arguments its parameters name, as Scrapy gives them, so take
the ones you need:

| method | when | arguments |
|---|---|---|
| `spider_opened` | before anything is fetched | `spider` |
| `engine_started` | after `spider_opened`, before the start requests | |
| `request_scheduled` | your code made a request; raise `IgnoreRequest` to drop it | `request`, `spider` |
| `request_dropped` | a middleware dropped a request, or the crawl refused it (a duplicate, offsite, too deep) | `request`, `spider` |
| `response_downloaded` | a page was fetched, before any middleware | `response`, `request`, `spider` |
| `request_left_downloader` | a fetch ended, with a page or an error | `request`, `spider` |
| `response_received` | a page is on its way to its callback, after the middlewares | `response`, `request`, `spider` |
| `item_scraped` | an item came out of every pipeline | `item`, `response`, `spider` |
| `item_dropped` | a pipeline dropped an item | `item`, `response`, `exception`, `spider` |
| `item_error` | a pipeline raised something other than `DropItem` | `item`, `response`, `failure`, `spider` |
| `spider_error` | a callback or errback raised | `failure`, `response`, `spider` |
| `spider_idle` | nothing is queued or being fetched | `spider` |
| `spider_closed` | once the crawl has ended | `spider`, `reason` |
| `engine_stopped` | last of all | |

`response` is the page, or None where there isn't one: for an item a
link rule made in Rust, one a callback yielded in a worker, or one an
errback or `on_block` yielded. `exception` is the `DropItem` a pipeline
raised, or None if it returned None. `failure` is the exception wrapped
as Scrapy's handlers expect it: `failure.value` is the exception (and
`exception`, for a handler that names it, is the same).

Scrapy's `request_reached_downloader`, `headers_received` and
`bytes_received` aren't sent. The fetching happens in Rust, which hands
Python whole responses, a batch at a time: stopping between a response's
headers and its body, or between its chunks, to call Python would cost
every page what the Rust engine is there to save. `response.headers` and
`len(response.body)` are there once the page arrives.

They come in Scrapy's order: every pipeline's `open_spider`, then
`spider_opened`, then `engine_started`; at the end, once the output files
are complete, every `close_spider`, then `spider_closed`, then
`engine_stopped`. The spider itself can define `opened()` and
`closed(reason)`, called after `spider_opened` and `spider_closed`.
`reason` is `spider.finish_reason` (`"finished"`, or the limit that
stopped the crawl), `"error"` when an exception ended it, or
`"interrupted"` after Ctrl-C (or SIGTERM or Ctrl-Break, under `netweir
crawl`).

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

### When the crawl runs out

`spider_idle` comes when there's nothing left to fetch, and it's the
place to find more: a spider that reads URLs from somewhere else can
schedule the next lot there.

```python
class Feed(netweir.Spider):
    def opened(self):
        self.crawler.signals.connect(self.idle, signal=netweir.signals.spider_idle)

    def idle(self, spider):
        for url in next_urls():
            self.crawler.engine.crawl(netweir.Request(url))
```

If anything it schedules is queued, the crawl carries on. Raise
`netweir.DontCloseSpider` to keep it open with nothing to do (the signal
comes again five seconds later), or `netweir.CloseSpider("reason")` to
end it with that reason. `CloseSpider` works from a callback, an errback
or `start()` too, and `spider.finish_reason` is then its reason
(`"cancelled"` if it gave none).

With `workers`, signals and `opened()`/`closed()` run in the main
process. A worker makes its own copy of the spider, which `opened()`
never ran on, so set up what callbacks need in `__init__` instead.

### from_crawler

List a class rather than an object, in `middlewares`,
`spider_middlewares` or `pipelines`, and the crawl makes it when it
starts: with `from_crawler(crawler)` if the class has one, as Scrapy
does, and with no arguments if not. Raising `NotConfigured` (Netweir's
or Scrapy's) leaves it out. So a Scrapy component like this carries over
unchanged:

```python
from scrapy import signals


class Timing:
    @classmethod
    def from_crawler(cls, crawler):
        made = cls(crawler.settings.getint("TIMING_EVERY", 100))
        crawler.signals.connect(made.spider_closed, signal=signals.spider_closed)
        made.stats = crawler.stats
        return made

    def __init__(self, every):
        self.every = every

    def process_response(self, request, response, spider):
        self.stats.inc_value("timing/responses")
        return response

    def spider_closed(self, spider, reason):
        spider.logger.info("done: %s", reason)
```

The crawler has:

- `crawler.spider`;
- `crawler.settings`, whose `get()`, `getbool()`, `getint()`,
  `getfloat()`, `getlist()` and `getdict()` read the spider's
  `custom_settings` dict, and whose attributes are Netweir's settings
  (`crawler.settings.concurrency`). Scrapy's own setting names
  (`CONCURRENT_REQUESTS` and the rest) aren't translated, so reading one
  gives the default you pass;
- `crawler.signals.connect(handler, signal)` and `disconnect`, for
  `netweir.signals.<name>`, a signal's name as a string, or Scrapy's
  `scrapy.signals` objects;
- `crawler.stats`, which is `spider.stats` (see
  [Watching a crawl](#reading-the-stats-from-your-spider)): `get_value`,
  `set_value`, `inc_value`, `max_value`, `min_value` and `get_stats`;
  what you set is in the stats `run()` returns;
- `crawler.engine.crawl(request)` and
  `crawler.engine.close_spider(spider, reason)`.

An object `from_crawler` made hears only the signals it connects, as in
Scrapy, not every method that shares a signal's name. The spider has the
same crawler as `spider.crawler` while the crawl runs, in the main
process.

## Coming from Scrapy

`startproject`, `genspider`, `list`, `parse`, `check` and `version` are
there, contracts included; see [Projects](project.md).

Pipelines, middlewares, spider middlewares and signal handlers written
for Scrapy carry over: `process_item`, `open_spider`, `close_spider`,
`DropItem`, every downloader and spider middleware method, `IgnoreRequest`,
`CloseSpider`, `DontCloseSpider`, `NotConfigured`, `from_crawler`,
`spider.logger` and `request.replace()` all work. The differences:

- They're listed as classes or objects (`pipelines = [Database]`), not as
  dotted paths in settings, in the order of their numbers there. There's
  no `spider.settings.get()`; settings are attributes, as
  `spider.settings.concurrency`, and `crawler.settings.get()` reads only
  the spider's `custom_settings`.
- `process_request` runs when your code makes the request, before the
  duplicate filter, rather than just before the fetch: the engine keeps
  the queue in Rust. An exception other than `IgnoreRequest` there is
  counted as an error, not passed to `process_exception`.
- `response` in a handler or middleware is a Netweir `Page`, and an
  errback gets `(request, error)` rather than a Twisted failure; signal
  handlers get a `failure` with `value`, as Scrapy's do.
- `request_reached_downloader`, `headers_received` and `bytes_received`
  aren't sent (see [Signals](#signals)), and `request_dropped` is sent
  for every request that went nowhere (a middleware's drop, offsite, too
  deep), not only for a duplicate.
- `CloseSpider` from a pipeline ends the crawl; Scrapy only sends
  `item_error`.
- `crawler.stats` and `spider.stats` are the same collector, live
  while the crawl runs. A key that's one of Netweir's own stats
  (`fetched`, `items` and the rest) raises ValueError when you set it, so
  name yours differently. LogStats is the progress line
  (`stats_interval` for `LOGSTATS_INTERVAL`), and
  [`metrics_port`](#from-outside-a-metrics-endpoint-for-prometheus) does
  what scrapy-prometheus-exporter does, with nothing to install.
- There's no telnet console. It gives anyone who connects a Python prompt
  inside the crawl, behind a password sent in clear text, and it's mostly
  used to read the stats or pause the engine. Read the stats from the
  metrics endpoint instead. To pause, stop the crawl with Ctrl-C and a
  [checkpoint](self-healing.md#checkpoints), then run the same command
  to carry on. To try selectors on a page, use
  [`netweir shell`](shell.md).

Most of the downloader settings have a [setting](settings.md) of the
same meaning: `DOWNLOAD_TIMEOUT` is `timeout`, `REDIRECT_MAX_TIMES` is
`max_redirects`, `RETRY_TIMES` is `retries`, `RETRY_HTTP_CODES` is
`retry_http_codes`, and `HTTPAUTH_USER`, `HTTPAUTH_PASS` and
`HTTPAUTH_DOMAIN` are `http_auth=(user, password)` and
`http_auth_domain`. The request meta keys `download_timeout`,
`dont_redirect`, `redirect_ttl`, `http_user`, `http_pass`,
`http_auth_domain` and `proxy` do what they do in Scrapy. One difference:
`HTTPAUTH_DOMAIN=None`, which sends the login to every site, has no
equivalent, and `http_auth` without a domain is an error.

XMLFeedSpider and CSVFeedSpider are `netweir.XMLFeedSpider` and
`netweir.CSVFeedSpider`, with the same attributes and methods; see
[XML, feeds and CSV](xml.md). Selectors on XML, `register_namespace()`
and `remove_namespaces()` included, work as parsel's.

Item loaders carry over with a change of import, to
[`netweir.loader`](loaders.md): `ItemLoader`, `MapCompose`, `TakeFirst`
and the rest of the processors, nested loaders and context all work as in
Scrapy, on dicts, dataclasses, attrs classes and Scrapy Items. The
[differences](loaders.md#coming-from-scrapy) are few and deliberate.

FilesPipeline and ImagesPipeline become [`netweir.files` and
`netweir.images`](#downloading-files-and-images): the same fields, the same
results and the same layout in the store. Their settings are arguments:
`FILES_STORE` and `IMAGES_STORE` are `store`, `FILES_EXPIRES` (days) is
`expires` (seconds, or a `timedelta`), `IMAGES_THUMBS` is `thumbs`, and
`IMAGES_MIN_WIDTH` and `IMAGES_MIN_HEIGHT` are `min_size`. A store is a
directory or anything fsspec reaches, not FTP. There's no `file_path()`
or `get_media_requests()` to override: the names are always the SHA1 of
the URL, which is what keeps them inside the store. Scrapy's offsite
middleware now drops a file or image on a domain outside
`allowed_domains`; Netweir downloads it, since covers so often sit on a
CDN. Filter `file_urls` in your callback if you want Scrapy's
behaviour.

### Or keep Scrapy, and fetch with Netweir

Not ready to move a project over? Keep it on Scrapy and let Netweir do the
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
- retries: Netweir's errors arrive as Scrapy's own (`DownloadTimeoutError`,
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
| `max_error_rate` | failed fetches and callback errors, out of pages fetched or served by a middleware plus failed fetches, are more than this fraction |
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

## Watching a crawl as it runs

A crawl that runs for hours shouldn't keep you guessing until it ends.
Every 60 seconds it logs where it's got to:

```
INFO crawled 1520 pages (at 412 pages/min), scraped 1490 items (at 405 items/min); 3 errors, 2210 queued, 64 in flight, 41.2 MiB received
```

The rates are since the line before. Errors are failed fetches plus
exceptions in callbacks and pipelines. `stats_interval` changes how often
the line comes, and `stats_interval=None` stops it.

### Reading the stats from your spider

`spider.stats` is live. Read it in a callback, a pipeline or a signal
handler and you get the numbers as they stand at that moment; once the
crawl ends, it holds the final stats. It reads like a dict
(`self.stats["items"]`, `dict(self.stats)`) and has Scrapy's stats
collector methods. `get_stats()` gives you every number from one moment
as a plain dict, where `dict(self.stats)` reads each one as it goes;
hand `get_stats()` to `json.dumps` while the crawl runs. Once it has
ended, `spider.stats` is a dict holding the final stats, as it was before
it went live, so `json.dumps`, `pickle` and `copy` work on it directly.
It's read-only: setting a key raises `TypeError`. You can keep counts of
your own beside Netweir's:

```python
class Shop(netweir.Spider):
    def parse(self, page):
        if page.css(".out-of-stock"):
            self.stats.inc_value("out_of_stock")
        self.stats.max_value("biggest_page", len(page.body))
        ...
```

`get_value(key, default)`, `set_value`, `inc_value(key, count=1,
start=0)`, `max_value` and `min_value` work as Scrapy's do, and take its
`spider=` argument too. Your stats end
up wherever Netweir's do: the dict `run()` returns, `--stats`, the last
log line, checks, and the endpoint below. Netweir's own names (`items`,
`fetched` and the rest) are refused, so a typo can't quietly overwrite
the real count.

With workers, a callback's writes travel back with its results and land
in the main process, so the totals cover every worker. A read inside a
worker only sees that worker's own writes.

### From outside: a metrics endpoint for Prometheus

Set `metrics_port` and the crawl serves its stats over HTTP while it
runs:

```
netweir crawl shop.py -o items.jsonl -s metrics_port=9410
curl http://127.0.0.1:9410/metrics
curl http://127.0.0.1:9410/stats.json
```

`/metrics` is Prometheus' text format. Each count is a counter called
`netweir_<stat>_total` (`netweir_fetched_total`, `netweir_items_total`,
`netweir_callback_errors_total`...), the queue and the requests in flight
are the gauges `netweir_queued` and `netweir_in_flight`, and
`netweir_start_time_seconds` is when the crawl started. Your own numeric
stats are one family, `netweir_custom_stat{stat="out_of_stock"}`. Every
sample is labelled with `spider` and `process` (host name and process
id). Point a Prometheus job at it:

```yaml
scrape_configs:
  - job_name: netweir
    static_configs:
      - targets: ["127.0.0.1:9410"]
```

`/stats.json` gives the same numbers as JSON, with how long the crawl has
been going. `metrics_port=0` takes any free port and logs which.

It listens on 127.0.0.1, so only this machine can read it. To let
Prometheus on another machine in, set `metrics_host` (`"0.0.0.0"`, say).
There's no password, and the crawl logs a warning saying so: anyone who
can reach the port can read the counts and the spider's name, though no
URLs, items or settings. Keep the port behind a firewall, or behind a
proxy that asks for credentials. Requests from web pages are refused,
as `netweir serve` refuses them. On an address other than 127.0.0.1, a
request has to name the endpoint by an IP address or by one of the names
in `metrics_allowed_hosts` (`("metrics.example.com",)`, say): a web page
can't then reach it under a name of its own pointed at your address. If the port is taken, the crawl stops
before it fetches anything, rather than running unwatched.

`netweir crawl --stats FILE` rewrites the file every 5 seconds while the
crawl runs, with `finish_reason` set to null until it ends. That's how
[`netweir serve`](serve.md#watching-and-fetching) shows a running job's
stats.

### Workers, shared queues and checkpoints

With workers, there's one set of stats: everything is counted in the
main process. With a [shared queue](distributed.md), each process has its
own stats, its own log line and its own endpoint, so give each process on
one machine its own port. The `process` label keeps them apart, and
Prometheus adds them up:

```
sum by (spider) (rate(netweir_fetched_total[5m]))
```

A crawl resumed from a checkpoint counts from zero again, as its stats
always have. Prometheus sees that as a restart, and `rate()` copes.

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
since resuming adds to the output file; nor can one sharing a
[queue](distributed.md#your-items), since a process killed before the
upload would lose items the queue has as done. Write to a local file and
upload it when the crawl is done.

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
