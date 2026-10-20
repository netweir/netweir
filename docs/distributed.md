# One crawl, many machines

One process can only go so fast. Its bandwidth is one machine's, its IP
address is one address a site can notice, and a crawl of ten million
pages at a polite pace takes weeks. Workers help when your callbacks are
the slow part; when the fetching is, you want more machines.

Give the crawl a Redis server and start the same spider on as many
machines as you have. They share one queue of requests and one record of
what's been seen, so each page is fetched by one of them, once. If you've
used scrapy-redis, it's the same idea.

## Setting it up

You need a Redis server, 6.2 or later, that every machine can reach
(Valkey works too). Then point the spider at it:

```python
class Books(netweir.Spider):
    start_urls = ["https://books.toscrape.com/"]
    settings = netweir.Settings(queue="redis://10.0.0.5:6379/0")

    def parse(self, page):
        ...
```

and run it on each machine:

```
netweir crawl books.py -o items.jsonl
```

Or leave the spider as it is and say so on the command line:

```
netweir crawl books.py -o items.jsonl --queue redis://10.0.0.5:6379/0
```

There's nothing extra to install: the Redis client is part of Netweir. A
password goes in the URL, `redis://:secret@10.0.0.5:6379/0`, and
Netweir's messages hide it. On Linux and macOS a Redis on the same
machine can be reached through its unix socket,
`redis+unix:///run/redis.sock?db=0&pass=secret`; on Windows there are no
unix sockets, and such a URL is refused.

Each crawl gets its own keys in Redis, named after the spider (its
`name`, or its class name). Two different crawls on one server need two
different names; set `queue_key="books-2026-10"` (or `--queue-key`) to
choose one yourself.

## What's shared, and what isn't

Shared between every process: the requests waiting to be fetched, in
priority order, and the duplicate filter. A link found on one machine can
be fetched on another, and a page one machine fetched is a duplicate
everywhere.

Everything else belongs to each process: robots.txt, the delay and
concurrency per site, proxies, cookies, the cache, workers, pipelines
and output files, the stats and every limit.

That last part matters for politeness. Four processes with
`per_domain=8` can have 32 requests in flight to one site, and each
throttles itself without knowing about the other three. If you're
crawling one site from four machines, divide by four: `per_domain=2`, or
a `min_delay` four times as long.

`max_items`, `max_pages`, `max_errors`, `max_time` and `max_memory` stop
the process that reaches them, not the crawl. Requests it had taken and
not finished go straight back to the queue for the others.
`max_pages_per_domain` counts the requests each process takes.

One thing catches people out. When a process takes a request from the
queue and its own settings turn it down (over `max_pages_per_domain`,
outside `allowed_domains`, deeper than `max_depth`, or a path that looks
like a crawler trap), that request is gone for the whole crawl. It isn't
put back for another process to try. Give every process the same
settings and you won't notice; give them different ones and you will.

## Your items

Each process writes its own items, to its own files, so give each one a
different name:

```
netweir crawl books.py -o "items-$(hostname).jsonl" --queue redis://10.0.0.5:6379/0
```

and put them together when the crawl is done. Or write a pipeline that
sends them somewhere every process can reach, such as a database.

The files have to be local. An `s3://` or `gs://` output is only
uploaded when the file closes, so a process killed before then would
lose items whose pages the queue already counts as done; Netweir refuses
the combination rather than let that happen. Write locally and upload
when the crawl is over.

The same goes for `warc` and `har`: each process archives what it
fetched itself, so give each one its own file.

Callbacks and `meta` travel between machines as data, the same rule as a
[checkpoint](self-healing.md#checkpoints): a request's callback has to be
a method of the spider, and its `meta` has to be JSON. Requests carry
callbacks by name and rules by their place in `rules`, so run the same
spider on every machine.

## When the crawl ends

A process ends once the queue has been empty, and no process has been
working on anything, for 30 seconds (`queue_idle`). Why wait at all? A
page being worked on anywhere is held until the links it yields are in
the queue, so "empty and nobody busy" really is the end; the wait is for
processes that start a little after the others, and for the first start
requests to arrive.

Set `queue_idle=None` and the processes never end on their own. They sit
and wait for requests, which suits a crawl you feed as you go.

## Feeding a crawl

Push URLs into a running crawl from anywhere that can reach Redis:

```
netweir queue push redis://10.0.0.5:6379/0 Books https://books.toscrape.com/catalogue/page-2.html
cat urls.txt | netweir queue push redis://10.0.0.5:6379/0 Books -
```

or from Python:

```python
queue = netweir.Queue("redis://10.0.0.5:6379/0", "Books")
queue.push(["https://books.toscrape.com/catalogue/page-2.html"], priority=10)
```

A pushed URL is a start request: it goes to the spider's `parse` (or its
rules), through your middlewares' `process_request`, at depth 0. A URL
the crawl has seen before is left out, unless you pass `--dont-filter`
(`dont_filter=True`). URLs go to Redis a thousand at a time, and `-`
reads standard input as it goes, so a list of ten million is fine.

A mistake in the URL or key (`http://` instead of `redis://`, an empty
key) raises `ValueError`, and the command exits with status 2. A Redis
that can't be reached raises `netweir.QueueError`, which is a
`ConnectionError`; the command prints one line and exits with status 1.

`netweir queue status` shows where things stand, and `netweir queue
clear` deletes the crawl's keys:

```
$ netweir queue status redis://10.0.0.5:6379/0 Books
waiting: 1840
held: 112
processes: 4
seen: 52210
queued: 52398
```

`waiting` is requests in the queue, `held` is requests processes have
taken and not finished, `processes` is how many are taking part, `seen`
is the duplicate filter's size, and `queued` counts every request ever
put in the queue. It's more than `seen` when requests with
`dont_filter` went in again.

## When a process dies

Kill one with SIGKILL, pull its power cable, lose its network: the
requests it had taken aren't lost. Each process sends a heartbeat every
20 seconds, and when one has been silent for a minute (`queue_lease`),
the next process to notice puts its requests back in the queue, where
another fetches them. The stats of the process that did it count them as
`leases_reclaimed`.

A minute is a guess at "long enough that it can't just be slow". A
shorter lease finds dead processes sooner, and takes a process that was
only stuck, say in a debugger, for dead sooner too. Its pages then get
fetched twice. Nothing is ever lost, but in rare cases like that a page
can be fetched twice.

Two other things go with a process that dies: items it hadn't written to
its file yet, and with Parquet output the whole file, since a Parquet
file is only written at the end. JSON Lines, JSON, XML and CSV files are
flushed before a process tells Redis its pages are finished, so whatever
it said it finished is in its file.

If a process can't reach Redis, it keeps trying, at most a second
apart. Once a minute (`queue_lease`) has gone by since its last
heartbeat got through, it stops with `netweir.QueueError`: by then the
others have taken back what it held. It never waits more than a third
of a lease for any one answer, reconnecting included, so a Redis that
hangs is found out as fast as one that's gone. And if Redis answers its
heartbeats but every attempt to queue something fails for a minute, it
stops the same way rather than keep trying for ever.

Something in the queue that isn't a request Netweir can use (written
there by hand, say, or by another program) is dropped with a warning
and counted in the stats as `queue_bad`, so it can't stop one process
after another.

## Running it again

The queue in Redis is the crawl's memory, the way a checkpoint is for
one process. Stop every process, start them again, and they carry on
where they were. The start URLs are duplicates by then, so nothing is
fetched twice.

It's also why a second crawl under the same name fetches only what's
new: the duplicate filter remembers every request it has queued, for as
long as you keep the keys. To crawl everything again, clear them:

```
netweir queue clear redis://10.0.0.5:6379/0 Books
```

or give the crawl a new `queue_key`.

## What doesn't mix

- `checkpoint`: the queue in Redis already is the crawl's saved state,
  so a crawl can't have both.
- `deltafetch`: its record lives on one machine, where the other
  processes can't see it. The queue has a memory of its own: a later
  crawl under the same key fetches nothing it queued before, listing
  pages included. To look at the listings again for new links, push them
  with `--dont-filter`; the item pages they link to that were fetched
  before stay skipped.
- Output to cloud storage, as above.
- `rediss://`: there's no TLS to Redis yet. Keep the server on a private
  network, or reach it through a TLS tunnel.
- Redis Cluster and Sentinel: one server, or a proxy in front of one.
- Unix sockets on Windows: use `redis://host:port` there.
- Processes on different versions. A request's timeout, redirect limit
  and login (`timeout=`, `allow_redirects=`, `auth=` and Scrapy's meta
  keys for them) travel with it in the queue, and a process from 0.3.1 or
  earlier ignores them: it would send the request without its login.
  The same goes for a request's [steps](steps.md): a 0.3.1 process
  ignores them and renders the page without them, with no warning, so a
  callback expecting the page after a "Load more" gets the page before
  it. Upgrade every process before your requests use them.

One thing no setting fixes: a callback that hangs for ever (waiting on
something that never answers, say). Its process is alive, so it keeps
sending heartbeats, and the requests it holds never go back to the queue;
with `queue_idle` set, no process ends while they're held. Stop the
stuck process. Once its lease runs out, a minute later, its requests go
back and another process finishes them.

Each process's stats have four more numbers: `pushed`, the requests it
sent to the queue; `leased`, the requests it took from it;
`leases_reclaimed`, the requests of dead processes it put back; and
`queue_bad`, entries it dropped because they weren't requests.

## Keeping Redis safe

Whoever can write to that Redis can steer every process in the crawl.
A request in the queue says which URL to fetch, and that can be an
address inside your network (169.254.169.254, the cloud metadata
service, for one). It also says which proxy to use, which headers and
body to send, and which spider method to call with the page. And the
traffic between Netweir and Redis is plain TCP, readable by anyone on
the path, password included.

Requests given a login (`auth=`, or `http_user` in meta) carry it in the
queue, readable by anyone who can read Redis. The crawl's own
`http_auth` setting never goes there.

A request's [steps](steps.md) travel in the queue too, and they act in
Chrome with the crawl's sessions:

- A step list can run JavaScript (`evaluate`) on any page it has the
  crawl fetch, with that site's cookies, and read or send whatever that
  page can see. Netweir allows it, so whoever can write to the queue can
  run script on your crawl's pages.
- `set_input_files` would upload files from the machine running Chrome.
  From the queue it may only upload from inside
  `Settings.browser_upload_dir`, and without that setting a request that
  uploads is dropped. Every request in a shared crawl comes back out of
  the queue, your spider's own included, so with a queue any upload at
  all needs `browser_upload_dir`.
- A callback, errback or call step named in the queue must be a method
  of your spider's own classes, or `parse`, `on_block` or `_sitemap`.
  Dunders, and the rest of netweir.Spider (`closed`, `crawl`, `run`...),
  are refused.

So give Redis a password (`requirepass`, and `redis://:password@host`
here), keep it on a network only your crawlers can reach, and don't
share the server with anything you don't trust.

The duplicate filter grows by about 80 bytes for every request, for as
long as the keys are kept: a hundred million requests is around 8 GB of
Redis memory.

How it works inside, from the layout of the keys to what happens at
each step when a process dies, is in
[design/distributed.md](design/distributed.md).
