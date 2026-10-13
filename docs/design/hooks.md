# Middlewares, signals and pipeline lifecycles

A spider says what to take from a page. Some code belongs to no page in
particular: a header every request needs, a response that should be
fetched again before a callback sees it, a database connection a pipeline
opens once and closes at the end, a count of items sent to a dashboard as
they arrive. Scrapy puts that code in downloader middlewares, signal
handlers and pipeline methods, and people porting a Scrapy project bring
it with them. Netweir gives each a place, under the same names where
Scrapy's names are the ones people know.

## Middlewares

`Spider.middlewares` is a list of objects. Each may define any of three
methods, sync or async, with the meaning Scrapy's downloader middlewares
give them, because a ported middleware that means one thing in Scrapy and
another here would fail silently:

- `process_request(request, spider)` runs before a request is queued.
  `None` goes on with the request (a middleware usually changes it in
  place and returns nothing), a `Request` goes on in its place, and
  `IgnoreRequest` drops it.
- `process_response(request, page, spider)` runs before the callback gets
  the page. `None` or the page goes on with it, another `Page` answers the
  request instead (its `request` is set to the crawl's, so `depth` and
  `meta` carry on and `max_depth` still counts), a `Request` is queued in
  its place, and `IgnoreRequest` drops it.
- `process_exception(request, error, spider)` runs when a request failed
  (`FetchError`). A `Request` is queued instead of calling the errback,
  `None` passes the error to the next middleware and after the last to the
  errback, and `IgnoreRequest` drops the request with no errback.

`IgnoreRequest` is `netweir.IgnoreRequest`, or any exception class of that
name (Scrapy's), subclasses included. Drops are counted as
`requests_dropped` and `responses_dropped`; a request given instead of
another keeps its depth.

`process_request` runs in list order and `process_response` and
`process_exception` in reverse, as Scrapy's downloader middlewares do: the
first middleware is the one nearest the spider's code.

Any other exception, or a result of the wrong type, counts as a callback
error (so `fail_fast` and `max_errors` apply). In `process_request` and
`process_response` it drops what was being handled; in
`process_exception` the error goes on to the next middleware.

What they see: every request the spider's code makes (start requests and
what callbacks yield) and every page that comes back to a callback. Not:

- pages the link rules handle entirely in Rust (a `Follow` without a
  callback, an `Item` rule) and the requests those rules follow. A rule's
  links are queued as the page arrives, before Python sees it, so
  dropping a page a rule also reads doesn't unqueue them.
- block pages, which go to `on_block`.
- a request already queued when a checkpoint saved it, when the crawl
  resumes: it went through `process_request` the first time.

Without workers, each page goes through `process_response` as its turn
comes, so a limit reached part way through a batch leaves the rest of the
batch untouched, to be fetched again by a resumed run. With `workers`, the
middlewares run in the main process for the whole batch before its pages
go to the workers; a limit reached part way through can leave later pages
of that batch through the middlewares but not their callbacks, as the
workers' callbacks already are.

## Signals

An object in `middlewares` or `pipelines` gets a signal by having a method
of its name, sync or async. It's called with the arguments its parameters
name (all of them if it takes `**kwargs`), as Scrapy's signal dispatch
does, so handlers written with Scrapy's signatures work:

- `spider_opened(spider)`: before anything is fetched.
- `spider_closed(spider, reason)`: after the crawl ends, however it ends;
  `reason` is `spider.finish_reason`, `"error"` when an exception ended
  it, or `"interrupted"` after Ctrl-C. Only what was opened is closed: if
  a `spider_opened` fails, those after it never hear `spider_closed`.
- `item_scraped(item, response, spider)`: an item came out of every
  pipeline. `response` is the callback's page, or None for an item from
  a Rust rule or a worker.
- `item_dropped(item, response, exception, spider)`: a pipeline dropped
  an item; `exception` is the `DropItem` it raised, or None.
- `request_dropped(request, spider)`: a middleware dropped a request.

The spider gets `spider_opened` and `spider_closed` too, as `opened()`
and `closed(reason)` methods, which is Scrapy's shortcut for them.

The order is Scrapy's: every pipeline's `open_spider`, then
`spider_opened`, then `opened()`; at the end, the exporters are closed
first, so the output files are complete, then every `close_spider`, then
`spider_closed`, then `closed()`. Whatever opened is closed: a pipeline
whose `open_spider` worked gets `close_spider` even if a later
`spider_opened` failed. A crawl that fails while opening, on a resumed
run, records nothing, so the output files keep the lengths last recorded.
With `workers`, all of this runs in the main process.

An exception in a signal handler is logged and doesn't stop the crawl,
except in `spider_opened`, which runs before anything is fetched or any
output file opened and fails the crawl: a pipeline that can't open its
database should say so before the crawl starts, not after a thousand
pages.

## Pipeline lifecycles

A pipeline stage can be a function, as before, or an object. An object
with `process_item(item, spider)` is called through that, as Scrapy calls
it. `DropItem` (Netweir's, or any class of that name, subclasses
included) drops the item, as returning `None` does. Any stage may have `open_spider(spider)`,
called before the first request, and `close_spider(spider)`, called after
the last item, sync or async. Exporters keep their own `close()`.

## Not in this version

Spider middlewares (Scrapy's `process_spider_output` and friends), the
remaining Scrapy signals (`engine_started`, `response_received`,
`headers_received` and so on), a response from `process_request` to skip
the fetch, and `from_crawler`: stages and middlewares are objects the
spider lists, made with whatever they need. The three middleware methods and five
signals above cover what people use them for: request headers and
proxies, response checks and retries, lifecycles, and counting.
