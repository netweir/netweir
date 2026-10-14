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

`Spider.middlewares` is a list of objects (or classes; see "from_crawler"
below). Each may define any of three methods, sync or async, with the
meaning Scrapy's downloader middlewares give them, because a ported
middleware that means one thing in Scrapy and another here would fail
silently:

- `process_request(request, spider)` runs before a request is queued.
  `None` goes on with the request (a middleware usually changes it in
  place and returns nothing), and so does the request itself. A `Page`
  answers the request without a fetch. Another `Request` is scheduled in
  its place: it starts the chain again from the first middleware, as
  Scrapy reschedules it. `IgnoreRequest` goes to `process_exception`.
- `process_response(request, page, spider)` runs before the callback gets
  the page. `None` or the page goes on with it, another `Page` answers the
  request instead (its `request` is set to the crawl's, so `depth` and
  `meta` carry on and `max_depth` still counts), a `Request` is scheduled
  in its place, and `IgnoreRequest` drops the page and calls the
  request's errback with it.
- `process_exception(request, error, spider)` runs when a request failed
  (`FetchError`) or a `process_request` raised `IgnoreRequest`. A
  `Request` is scheduled instead of calling the errback, a `Page` answers
  the request, `None` passes the error to the next middleware and after
  the last to the errback, and `IgnoreRequest` skips the middlewares left
  and goes to the errback in the error's place.

A `Page` from `process_request` or `process_exception` goes through every
middleware's `process_response`, then on to the callback, as Scrapy sends
a response from either through `process_response`. It is never fetched,
but the engine still screens the request as if it would be: a duplicate,
an offsite request, one past `max_depth`, or one deltafetch has seen give
no page, so a middleware that serves pages from a store can't send the
spider round in circles. A served page is a page for every limit that
counts them: `max_pages` and `max_pages_per_domain` count it as they count
a fetch, and the stats count it as `pages_served` (as Scrapy's
`response_received_count` and `CLOSESPIDER_PAGECOUNT` count a response
from a middleware). Without that, a store whose pages each lead to three
more would never reach `max_pages`. Only robots.txt doesn't apply, as
nothing is fetched. The page's request is the crawl's, as with
`process_response`. A request with no callback on a rules spider has its
served page read by the rules in Rust, as a fetched one is, and the links
they find are queued. A served request isn't in the checkpoint, as the
engine never queued it: a resumed crawl asks the middleware again, and
the items already written aren't written twice.

This is the order Scrapy has, with one difference that matters:
`process_request` runs before the request reaches the engine, not when
the engine is about to fetch it. Scrapy schedules first and runs the
middlewares at download time; here the engine owns the queue and the
fetch both, in Rust, and stopping it before each fetch to call Python
would cost every request a round trip. So a middleware sees a request
before the duplicate filter rather than after, and a header it sets is
saved with the request in a checkpoint.

A Request returned from `process_request` used to carry on through the
middlewares after the one that returned it. It now starts again from the
first, as in Scrapy, which is what a ported middleware that redirects a
request expects: the middlewares before it get to set their headers on
the new one too. A middleware that returns `request` itself is going on
with it, not rescheduling it (Scrapy would reschedule it, and the
duplicate filter would drop it, which nobody means). One that gives a new
request every time it's asked would never end; after 100 in a row for one
request, the request is dropped and counted as a callback error.

`IgnoreRequest` is `netweir.IgnoreRequest`, or any exception class of that
name (Scrapy's), subclasses included. It ends at the request's errback, as
in Scrapy: from `process_request` through `process_exception` first, from
`process_response` or `process_exception` straight there. Without an
errback, it's dropped quietly. Drops are counted as `requests_dropped`
(from `process_request` and `process_exception`) and `responses_dropped`
(from `process_response`); a request given instead of another keeps its
depth.

`process_request` runs in list order and `process_response` and
`process_exception` in reverse, as Scrapy's downloader middlewares do: the
first middleware is the one nearest the engine, which here is also the
one nearest the spider's code.

Any other exception, or a result of the wrong type, counts as a callback
error (so `fail_fast` and `max_errors` apply). In `process_request` and
`process_response` it drops what was being handled; in
`process_exception` the error goes on to the next middleware. Scrapy
passes any exception from `process_request` to `process_exception`; here
only `IgnoreRequest` goes there, because a bug in a middleware is a bug to
count and stop on, not a failed download for another middleware to retry.

What they see: every request the spider's code makes (start requests,
what callbacks yield, and what signal handlers schedule) and every page
that comes back to a callback. Not:

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

What a middleware hands back that isn't the page in hand (a page it
answered a request with, an errback to call) waits until the callback
running now has finished, so one callback never runs inside another.

## Spider middlewares

`Spider.spider_middlewares` is a second list, for Scrapy's spider
middlewares, which sit between the engine and the callbacks. It's a list
of its own because Scrapy's are a setting of their own
(`SPIDER_MIDDLEWARES`), and a ported project lists them in the order of
their numbers there, lowest first: the first is nearest the engine, the
last nearest the spider.

- `process_spider_input(response, spider)` runs, in list order, before
  the callback gets the page. It returns None or raises; an exception
  calls the request's errback with it (`errback(request, error)`, whose
  output goes on through `process_spider_output`), or, without an
  errback, goes to `process_spider_exception`.
- `process_spider_output(response, result, spider)` runs, in reverse,
  on what the callback gives, and gives what goes on: an iterable of items
  and requests. An async generator (Scrapy 2.13's way) gets an async
  iterable and passes things on as they come. A plain function or
  generator gets a list, the whole output gathered first, as Scrapy does
  for a sync middleware after an async one. A middleware with
  `process_spider_output_async` as well (Scrapy's "universal" middleware)
  is given the async one.
- `process_spider_exception(response, exception, spider)` runs, in
  reverse, when the callback raised, or a `process_spider_output` nearer
  the spider did, or a `process_spider_input`. None passes it on; an
  iterable is output in place of what was lost, and goes through the
  `process_spider_output` of the middlewares nearer the engine than the
  one that gave it. An exception no middleware takes is the callback's
  error, as before, and isn't offered to them again on its way out.
- `process_start(start)`, an async generator over the spider's `start()`,
  in reverse, so the one nearest the spider wraps it first: Scrapy 2.13's
  replacement for `process_start_requests`. `process_start_requests(
  start_requests, spider)`, Scrapy's older way, works too, given a list
  of the start requests gathered first when `start()` is async (as it is
  by default).

They see every page a callback gets, from a fetch, a rule with a
callback, or a downloader middleware, but not errbacks' calls for failed
requests or `on_block`'s, as in Scrapy. With `workers`, they run in the
main process: `process_spider_input` before the page goes to a worker,
`process_spider_output` and `process_spider_exception` on what comes
back.

## Signals

An object in `middlewares`, `spider_middlewares` or `pipelines` gets a
signal by having a method of its name, sync or async. It's called with
the arguments its parameters name (all of them if it takes `**kwargs`),
as Scrapy's signal dispatch does, so handlers written with Scrapy's
signatures work. A handler can also be connected by hand, as in Scrapy,
with `crawler.signals.connect(handler, signal=netweir.signals.spider_idle)`
(see "from_crawler" below); an object made by `from_crawler` hears only
what it connects, as it would in Scrapy, so a method that happens to share
a signal's name isn't called twice, or at all.

- `engine_started()`: after `spider_opened` and `opened()`, before the
  first start request, as in Scrapy, where the engine starts once the
  spider is open.
- `spider_opened(spider)`: before anything is fetched.
- `spider_idle(spider)`: the engine has nothing queued or in flight. A
  handler may schedule requests with `crawler.engine.crawl(request)`, and
  if any of them is queued the crawl goes on. `DontCloseSpider` keeps it
  open even with nothing to do, and the signal comes again five seconds
  later, as Scrapy's does; `CloseSpider(reason)` ends it with that reason,
  unless a handler has given it something to do.
- `spider_closed(spider, reason)`: after the crawl ends, however it ends;
  `reason` is `spider.finish_reason`, `"error"` when an exception ended
  it, or `"interrupted"` after Ctrl-C. Only what was opened is closed: if
  a `spider_opened` fails, those after it never hear `spider_closed`.
- `engine_stopped()`: last of all, after `closed()`.
- `spider_error(failure, response, spider)`: a callback or errback
  raised, and no spider middleware took the exception. `failure` stands in
  for Twisted's: `failure.value` is the exception, and it has `type`,
  `getTraceback()`, `getErrorMessage()` and `check()`. `exception` is the
  exception too, for a handler that would rather name that.
- `request_scheduled(request, spider)`: the spider's code made a request
  and it's about to go through the middlewares to the engine (again, for
  one a middleware rescheduled). `IgnoreRequest` from a handler drops it,
  as in Scrapy.
- `request_dropped(request, spider)`: a middleware dropped a request, or
  the engine refused it (a duplicate, offsite, past `max_depth` or
  `max_pages_per_domain`, a trap, or seen by deltafetch). That's wider
  than Scrapy, which sends it only when its scheduler refuses a request
  (a duplicate, in practice): a handler counting drops sees every request
  that went nowhere.
- `response_downloaded(response, request, spider)` and
  `request_left_downloader(request, spider)`: a fetch ended, with a page
  (`response` is the Page, before any middleware) or, for
  `request_left_downloader` only, an error.
- `response_received(response, request, spider)`: a page is on its way
  to its callback, after every `process_response`. A page a middleware
  answered a request with gets only this one, as in Scrapy, since it
  never reached the downloader.
- `item_scraped(item, response, spider)`: an item came out of every
  pipeline. `response` is the callback's page, or None for an item from
  a Rust rule or a worker.
- `item_dropped(item, response, exception, spider)`: a pipeline dropped
  an item; `exception` is the `DropItem` it raised, or None.
- `item_error(item, response, spider, failure)`: a pipeline raised
  something other than `DropItem`. The error still counts as a callback
  error, as before; this tells a handler which item it was.

The download signals are honest about where they're sent from. The engine
fetches in Rust and hands Python a batch of finished fetches at a time:
`response_downloaded` and `request_left_downloader` come when Python takes
the fetch from the engine, a moment after it ended, not in the middle of
the download. Scrapy's `request_reached_downloader`, `headers_received`
and `bytes_received` aren't sent at all: the first would come in the same
batch as the response, telling a handler nothing it could act on, and the
others would need the engine to stop between a response's headers and
its body, and between its chunks, to call Python, which is the cost the
Rust engine is there to avoid. A handler that needs a response's size has
`len(response.body)`; one that needs its headers has `response.headers`.
Requests that link rules follow in Rust aren't signalled until their
pages come to a Python callback.

The spider gets `spider_opened` and `spider_closed` too, as `opened()`
and `closed(reason)` methods, which is Scrapy's shortcut for them, and
`spider.crawler` while it runs, as Scrapy's spiders have it, so
`opened()` can connect the spider's own handlers.

The order is Scrapy's: every pipeline's `open_spider`, then
`spider_opened`, then `opened()`, then `engine_started`; at the end, the
exporters are closed first, so the output files are complete, then every
`close_spider`, then `spider_closed`, then `closed()`, then
`engine_stopped`. Whatever opened is closed: a pipeline whose
`open_spider` worked gets `close_spider` even if a later `spider_opened`
failed. A crawl that fails while opening, on a resumed run, records
nothing, so the output files keep the lengths last recorded. With
`workers`, all of this runs in the main process.

An exception in a signal handler is logged and doesn't stop the crawl,
except in `spider_opened`, which runs before anything is fetched or any
output file opened and fails the crawl: a pipeline that can't open its
database should say so before the crawl starts, not after a thousand
pages.

`CloseSpider(reason)` from a callback, an errback, `start()` or a
`spider_idle` handler ends the crawl with that reason as `finish_reason`
(`"cancelled"` with none), as in Scrapy; so does
`crawler.engine.close_spider(spider, reason)`, from anywhere. From a
`spider_opened` handler it ends the crawl before the engine starts, as in
Scrapy 2.18, after the other handlers have been told. It ends it as a
limit does: the batch in hand is left, to be fetched again by a resumed
run. From a pipeline, Scrapy only sends `item_error`; here it ends the
crawl as well, since a pipeline that raises it means it, and a pipeline
error already ends its callback here.

## from_crawler

A class in `middlewares`, `spider_middlewares` or `pipelines`, rather
than an object, is made when the crawl starts: with
`cls.from_crawler(crawler)` if it has one, as Scrapy makes its
components, and with `cls()` if not. `NotConfigured` (Netweir's or
Scrapy's) from either leaves it out, as Scrapy does. So a Scrapy
middleware or pipeline that reads its settings and connects its signals
in `from_crawler` carries over unchanged.

The crawler is small, and only what Netweir can mean honestly:

- `crawler.spider`: the spider.
- `crawler.settings`: `get(name, default)`, `getbool`, `getint`,
  `getfloat`, `getlist`, `getdict`, `[name]` and `in`, reading the
  spider's `custom_settings` dict, which is where a Scrapy spider puts
  its own; Netweir's settings as attributes (`crawler.settings.concurrency`).
  Scrapy's names for its own settings (`CONCURRENT_REQUESTS` and the
  rest) aren't mapped to Netweir's: a middleware that reads one gets its
  default, which is better than a value that means something slightly
  different.
- `crawler.signals`: `connect(receiver, signal)` and `disconnect`. A
  signal is `netweir.signals.<name>`, its name as a string, or Scrapy's
  own object from `scrapy.signals`.
- `crawler.stats`: `get_value`, `set_value`, `inc_value`, `max_value`,
  `min_value` and `get_stats`, as Scrapy's stats collector has them. What
  a component sets is in the stats the crawl returns, beside Netweir's.
- `crawler.engine`: `crawl(request)` to schedule a request from outside a
  callback (from a signal handler, most often `spider_idle`), and
  `close_spider(spider, reason)`.

## Pipeline lifecycles

A pipeline stage can be a function, as before, or an object. An object
with `process_item(item, spider)` is called through that, as Scrapy calls
it. `DropItem` (Netweir's, or any class of that name, subclasses
included) drops the item, as returning `None` does. Any stage may have `open_spider(spider)`,
called before the first request, and `close_spider(spider)`, called after
the last item, sync or async. Exporters keep their own `close()`.

## Not in this version

`request_reached_downloader`, `headers_received` and `bytes_received`,
for the reasons above. Components named by dotted path, as Scrapy's
settings name them: list the class itself. `crawler.signals.send_catch_log`
for a component's own signals, and the rest of Scrapy's crawler
(`crawler.engine.downloader`, the request fingerprinter): a component that
needs them is reaching into Scrapy's engine, which isn't here.
