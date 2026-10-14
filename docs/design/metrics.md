# Watching a crawl while it runs

Until now a crawl's stats arrived at the end: `run()` returned them,
`netweir crawl --stats` wrote them to a file, and `netweir serve` showed
them once a job had finished. A crawl that runs for six hours was a black
box for six hours. This is how a running crawl shows what it's doing:
in its log, to its own code, and to whatever watches it from outside.

## Where the numbers already are

Every stat lives in one of three places, all in the main process:

- the engine's counters (`fetched`, `failed`, `bytes`, `retries` and the
  rest, plus the gauges `queued` and `in_flight`), behind the engine's
  one mutex, copied out whole by `Crawler.stats()`;
- `_Run.counts`, a dict the event loop updates (`items`,
  `callback_errors`, `files_*`...);
- nothing else. With workers, callbacks run in other processes, but
  every count is made in the main process from what they send back.

So a live snapshot is `{**engine.stats(), **counts}`: one lock taken for
the time it takes to copy thirty integers, and a dict copy under the GIL.
The engine's hot path is untouched; nothing is pushed per event. Every
consumer below pulls a snapshot when it wants one, at most a few times a
minute in practice. No Rust changes.

## `spider.stats`, live

`spider.stats` becomes a `netweir.Stats` the moment a crawl starts, not
a dict filled in at the end. It's a read-only dict (so `stats["items"]`,
`stats.get(...)`, `dict(stats)` and `{**stats}` work as before) whose
every read is a fresh snapshot while the crawl runs, and the final stats
once it has ended. It also has Scrapy's stats collector methods, so code
written against `crawler.stats` carries over by changing the receiver:
`get_value`, `get_stats`, `set_value`, `inc_value`, `max_value` and
`min_value`.

The writing methods are for a spider's own stats. Netweir's own keys
can't be written: an `inc_value("items")` would make the stats lie about
what the crawl did, and silently ignoring it would be worse. Custom stats
go everywhere the built-in ones go: the returned dict, the `--stats`
file, the end-of-crawl log line, checks, and the metrics endpoint.

There's no `crawler` object in Netweir (pipelines and middlewares are
objects you make, not classes `from_crawler` builds), so `spider.stats`
is the one place to reach. If a `crawler` comes later, its `stats` is
this same object.

With workers, each worker's spider gets a `Stats` that records every
write a callback makes and sends the list back with the callback's
results; the main process replays it in order. `inc_value` deltas add
up, and `set_value`, `max_value` and `min_value` apply in the order the
results arrive. Reads in a worker see only that worker's own writes: the
totals are in the main process.

After the run, the object holds the final stats, including
`checks_failed`, and a write from `closed()` or a `spider_closed`
handler still lands in them.

`spider.stats` was a dict in 0.3.1, and code dumps it to JSON, pickles
it and copies it. So `Stats` is a dict subclass: read-only, its methods
live, and its dict storage filled with the final stats when the crawl
ends, so `json.dumps` (which reads a dict's storage, not its methods)
sees them. While the crawl runs that storage is empty; `get_stats()` is
the plain dict to dump then. Pickle and copy go through `__reduce__`,
which gives a plain dict of a snapshot, never the lock or the engine.
Writes made after the last batch (a pipeline's close, a check) are added
to the final stats as they're frozen.

## The progress line

Scrapy's LogStats logs "Crawled X pages (at Y pages/min), scraped Z items
(at W items/min)" every 60 seconds. Netweir does the same at INFO on the
`netweir` logger, every `stats_interval` seconds (60 by default, None for
never), and adds what a Netweir user asks next: errors (failed fetches
plus callback errors), the queue, requests in flight, and the bytes
received.

```
INFO crawled 1520 pages (at 412 pages/min), scraped 1490 items (at 405 items/min); 3 errors, 2210 queued, 64 in flight, 41.2 MiB received
```

The rates are per minute over the last interval as it really was, not as
it was meant to be.

It runs on a thread of its own rather than as a task on the crawl's event
loop. A callback that blocks the loop for two minutes is exactly when
you want the line, and a thread still writes it. The thread waits on an
Event, so stopping the crawl stops it at once.

## The metrics endpoint

`metrics_port` (None by default, so off) starts an HTTP server in the
crawl's process, on `metrics_host` (127.0.0.1 by default). Port 0 takes a
free port and logs it. It answers two GETs:

- `/metrics`: Prometheus' text exposition format (version 0.0.4). Each
  counter is `netweir_<stat>_total`, each gauge `netweir_<stat>`, with a
  HELP and a TYPE line, and every sample carries `spider` and `process`
  labels. A spider's own stats, which can be anything, are one family,
  `netweir_custom_stat{stat="..."}`, typed untyped; only numbers are
  exported. `netweir_start_time_seconds` gives the crawl's start as Unix
  time.
- `/stats.json`: the same snapshot as JSON, with `spider`, `process`,
  `elapsed`, `queued` and `in_flight`.

The format is a few dozen lines to write; no client library.

### Why the standard library's server, on a thread

The choices were the standard library's `ThreadingHTTPServer` on a
thread, an asyncio server on the crawl's loop, or a server in the Rust
engine.

- On the crawl's loop, a scrape waits for the loop: a long synchronous
  callback stalls `/metrics`, and Prometheus marks the target down at the
  moment something is wrong. Rejected.
- In Rust, it would need a second HTTP stack beside the fetcher's (the
  engine has a client, not a server), or hand-rolled HTTP parsing, and the
  Python-side counts would have to be pushed into it on every change,
  which is the hot-path cost this design avoids. Rejected.
- A thread answers while the loop is busy, needs nothing installed, and
  `netweir serve` already runs on the same server class, with its
  per-address and total connection caps, its request deadline, and its
  refusal of requests a web page sends. The metrics server reuses those.

Reusing it turned up a cost: serve's request deadline was a timer thread
per connection, and each left memory behind. Scraped 4,000 times, the
endpoint grew by 7 MB (GIL build) where a plain stdlib server stayed
flat. A slow trickle of scrapes for days would add up, so the deadlines
now share one watchdog thread per server, for `netweir serve` too, and
the same 4,000 scrapes leave memory flat. An action that raises is
logged, and the watchdog carries on.

Measured on `bench/crawl.py`'s netweir crawl (10,000 pages, release
build, free-threaded CPython 3.14, four interleaved runs of each):

| | pages/s | CPU ms/page | peak RSS |
|---|---|---|---|
| progress line off | 1747-1826 | 0.29-0.31 | 99-100 MB |
| defaults (line every 60 s) | 1774-1814 | 0.30-0.31 | 100-101 MB |
| endpoint on, not scraped | 1721-1787 | 0.31 | 104-106 MB |
| endpoint scraped every 1 s, line every 1 s | 1732-1803 | 0.30-0.32 | 111-116 MB |
| endpoint scraped every 0.1 s | 1737-1787 | 0.32-0.33 | 119-122 MB |

Throughput is within run-to-run noise in every case. The endpoint costs
about 5 MB (the server and its imports); the scraped rows include the
scraping client, which ran inside the crawl's process, and a free-threaded
build's per-thread memory for each connection's thread.

### Security

Off by default, and on 127.0.0.1 by default. There is no authentication,
and the docs say so rather than offer a password check that can't be
backed with TLS. What it serves is counts, the spider's name, and the
host name and process id; no URLs, items or settings. Requests with an
`Origin` header, and, on a loopback address, requests for a host name
that isn't a loopback one (DNS rebinding), are refused, as `netweir
serve` refuses them. On any other address a request must name an IP
address or one of `metrics_allowed_hosts`, which closes the same hole
for an endpoint on a LAN address. (`netweir serve` on such an address
needs a password, or `--insecure-no-auth` and its warning.) Binding another address is allowed, since
Prometheus usually scrapes from another machine, with a warning in the
log that anyone who can reach the port can read the stats: put it behind
a firewall or an authenticating proxy.

A port that's taken stops the crawl before anything is fetched or
written. A monitored crawl that quietly isn't monitored is worse than one
that doesn't start.

## Live stats in `netweir serve`

A job is a `netweir crawl --stats FILE` child. Two ways to see its stats
while it runs:

1. The child runs a metrics endpoint and the service asks it.
2. The child rewrites its stats file every few seconds, and the service
   reads the file.

The file wins. The endpoint needs a port per job, chosen by someone: a
fixed one collides between jobs, and a free one has to be told back to
the service, which is another channel to keep working. A job whose
event loop or thread is stuck answers nothing, while its file still
says what it had got to. And a job killed outright leaves its last
snapshot, so a `failed` or `cancelled` job has the stats it reached,
where before it had none. The file costs one small JSON write every
five seconds.

Each write goes to a temporary file that then replaces the stats file,
so a reader never sees half of one. On Windows a replace fails while a
reader has the file open; a live write that fails is skipped (the next
comes five seconds later), and the final write tries again for a moment.

While the job runs, `jobstats.json` reads that file: the stats so far,
with `finish_reason` null. Once it ends, the final file is kept in the
job table as before.

## Workers, shared queues and checkpoints

- Workers: every built-in count is made in the main process, and custom
  stats are replayed there, so one snapshot covers the crawl.
- A shared queue: each process has its own stats, its own log line and
  its own endpoint (one port each, on one machine). The `process` label
  (host name and pid) keeps their series apart, and
  `sum by (spider) (rate(netweir_fetched_total[5m]))` is the crawl's
  rate. Adding them up inside Netweir would mean writing counters to
  Redis on the hot path, for a sum Prometheus already does.
- Checkpoints: a resumed crawl's stats are that run's, as they always
  were. Counters start again at zero, which Prometheus reads as a
  restart, and `rate()` handles.

## Not doing: a telnet console

Scrapy's telnet console gives a Python prompt inside the running crawl.
It listens on a TCP port with a generated password sent in clear text,
anyone who connects can run any code as the crawl's user, and in
practice it's used for two things: reading the stats, and pausing the
engine. The first is the endpoint above, read-only by construction. The
second is Ctrl-C with a checkpoint: the crawl stops cleanly and the same
command carries on. For poking at a page, `netweir shell` fetches it the
way the crawl would. A remote code prompt is not something to add for
the rare case left over.
