# A cache for writing spiders

Writing a spider means running it dozens of times: fix a selector, run it
again, find the next one. Each run fetches the same pages from the same
site, waits out the same politeness delays, and puts the same load on
someone else's server. With `cache` set to a directory, a response Netweir
already has is served from there instead: no request goes out, no delay is
waited, and the callback gets the same page it got before.

It's for development. It isn't an HTTP cache in the RFC 9111 sense, and it
ignores `Cache-Control` and `Expires` on purpose: while a spider is being
written, the point is that the page stays put.

## Settings

- `cache`: a directory, or `None` (the default) for no cache. Responses
  are kept in `<dir>/cache.sqlite3`, beside anything else the user keeps
  there.
- `cache_expiry`: seconds after which a cached response is stale, fetched
  again and replaced; `None` (the default) for never. It must be positive.

Clearing the cache is deleting the directory. There is no eviction yet: the
file grows with every new page, which for a spider in development is a few
hundred pages, not a site. That is said in the docs.

## The key

A response is kept under the fingerprint the duplicate filter already
uses: method, canonical URL and body, not headers. Two searches with
different terms are two entries; `?b=2&a=1` and `?a=1&b=2` are one, and so
are two URLs that differ only in a fragment or a `utm_` parameter. A hit
comes back with the URL that was asked for, not the one that was kept.

It's the fingerprint of the request as it was submitted, not of where it
ended up. Netweir follows redirects a hop at a time, each hop queued like a
new request on its own host, so the request carries its original
fingerprint through the hops, and the entry holds the final response, its
URL included. A hit then needs no hops at all, and `page.url` is the final
URL as before.

A hop does more than lead somewhere, and a hit has to do the same. The
entry keeps, for each redirect, the URL that answered, the Set-Cookie
values it carried and the fingerprint of the request it led to. On a hit
each hop's cookies, and then the final response's, go into the jar the
live fetch would have used (its host's own session after a block, or the
crawl's), in order; and each hop's fingerprint is marked seen and saved to
the checkpoint, exactly as following it would have. A login that redirects
leaves the crawl logged in, cached or not, and the duplicate filter drops
the same requests either way.

## What is kept

Only answers worth repeating: a 2xx or 3xx that ended the request, and a
4xx that is the site's answer about the page (a 404 will be the same next
run). Never:

- a block, so a block is never replayed: the next run asks the site again.
  That includes what may be a block nobody has a signature for yet: a 401,
  a 403 or a 451;
- a 407, which is the crawl's own proxy talking, not the site;
- a 304, which only answers the conditional request that got it;
- a throttle (429, or 503 with `Retry-After`) or a 402;
- a 5xx, even after the retries are spent;
- a failure (no response at all);
- a page TDMRep reserved, which never reached the callback anyway;
- a page fetched in Chrome. Its callback gets a live page to click and type
  in, which no stored response can stand in for, so browser requests skip
  the cache in both directions.

## Where a hit is served

When a request is admitted: once it has passed everything an admitted
request passes (offsite, depth, traps, the duplicate filter,
`max_pages_per_domain`) and is saved to the checkpoint, the engine looks
up whether the cache holds a fresh entry for it. A hit goes on a queue of
its own rather than its host's.

That lookup is in memory. Admission runs under the engine's lock, and the
file may be held by another process for as long as the busy timeout, so
nothing under the lock touches it. The cache reads its keys and their
storage times into a map when it opens, and each write adds to it. A dev
cache of a few thousand pages makes that a few hundred kilobytes.

The scheduler serves that queue before it looks at any host, so a hit
never waits for a delay, a concurrency slot, a per-site limit or a robots.txt
or TDMRep fetch. That is the point of it: a run that is all hits makes no
requests at all, robots.txt included.

robots.txt and TDMRep aren't checked again for a cached page. They govern
what is fetched, and a hit fetches nothing. What matters is that the page
was checked under rules at least as strict as this run's, so each entry
records the rules it was fetched under: whether robots.txt was obeyed, as
which agent, and whether TDMRep was. An entry kept by a laxer run (one with
`obey_robots=False`, or `obey_tdmrep=False`, or another `robots_agent`) is
a miss for a stricter one, which fetches the page and checks it properly;
an entry kept by a stricter run is good for a laxer one. What's left is a
site that changes its robots.txt after a page was cached: the page is
still served from disk. For a cache that lives on one machine for the
weeks a spider takes to write, that's acceptable: the site gets no request
either way, and clearing the cache puts every page back through both.

Serving the queue in the scheduler, not straight from `submit`, keeps two
guarantees. Hits respect the backlog limit, so a spider that submits ten
thousand cached start URLs doesn't hold ten thousand pages in memory. And
hits count toward `max_pages` where every request is counted, when it
starts: a cached rerun of a `max_pages=20` crawl gives exactly 20 pages,
and the requests left over stay queued, and in the checkpoint, as they
would without the cache.

The body is read from disk off the scheduler, on a blocking thread, one hit
at a time: the file has one connection anyway, and hits come back in the
order they were queued. If the entry has gone or gone stale in between
(another process replaced it, or the expiry passed), the request goes to
its host's queue and is fetched like any other.

A hit produces the same `Fetched` event a fetch would: the request's id, a
Response with the same URL, status, HTTP version, headers and body. The
callback can't tell. It counts in `fetched` like any page the caller gets,
and in `cache_hits`, but not in `bytes`, which counts what was downloaded;
each response written counts in `cache_stores`.

## The checkpoint

Unchanged. A hit is admitted, saved and acked like any request, so a crawl
with both a cache and a checkpoint resumes the same way.

## The file

SQLite, as the checkpoint: WAL mode, `synchronous=NORMAL`, and a busy
timeout, so a second process that opens the same cache waits its turn
instead of failing or corrupting it. One crawl per cache directory is what
it's for; two at once is safe, merely slower.

The layout is versioned with SQLite's `user_version`. This is format 1. A
future change upgrades older files on open; a file from a newer Netweir is
refused with a message saying so, and the format is read before anything
is written, so the file is left exactly as it was. A file that can't be
opened at all, or isn't a database, is an error that names it and says to
delete the directory.

Writes go straight to the file from the fetch that got the response, on a
blocking thread, before its event reaches the caller: when a callback sees
a page, the page is already on disk, and a crawl killed at any point loses
nothing it handed over.

A read or a write that fails doesn't stop the crawl. A failed read is a
miss and the page is fetched; a stored response that can't be decoded
counts as one. A failed write leaves the page uncached. Either way the
crawl gets one warning, naming the file and the error, the first time it
happens: a full disk would otherwise say so for every page.

Closing the crawl closes the file, so on Windows the directory can be
deleted as soon as the run is over. Closing never waits: a write stuck
behind another process, for as long as the busy timeout, holds the
connection, so closing marks the cache closed and the write closes the file
itself the moment it's done.
