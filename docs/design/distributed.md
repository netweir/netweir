# One crawl on many machines

Status: draft, October 2026
Author: James Conway

## What it is for

A crawl runs in one process. Workers spread its callbacks over a
machine's cores, but the fetching, the queue and the duplicate filter
are that process's, so a crawl of tens of millions of pages is held to
one machine's bandwidth and one IP address's patience. Scrapy users get
past that with scrapy-redis: every process on every machine takes its
requests from one queue in Redis and checks one duplicate filter there,
and new start URLs can be pushed into the queue while the crawl runs.

`Settings(queue="redis://host:6379/0", queue_key="books")` does the same
for Netweir. Start the same spider on as many machines as you like,
pointed at the same Redis and key, and they share the work: each page is
fetched by one of them, once.

## Why Redis, and why in Rust

Redis is what scrapy-redis users already run, it has the sorted sets
and transactions a priority queue with leases needs, and Valkey and
other Redis-compatible servers speak the same protocol. The client is
redis-rs (the `redis` crate, BSD-3-Clause, maintained under the Redis
organisation), built into the extension with its tokio support and its
connection manager, which reconnects on its own after a dropped
connection. Doing the I/O in Rust keeps it off the GIL and inside the
engine: links the rules find in Rust go to Redis without Python seeing
them, as they go to the local queue today. And since it's in the wheel,
there's nothing more to install: no `netweir[redis]` extra, no Python
`redis` package. Besides redis itself it brings ten small crates; one,
xxhash-rust, is under the Boost licence, which is permissive and allowed
for that crate alone in deny.toml.

The cost is the client's size in the extension, and no TLS: redis-rs
does TLS with rustls or native-tls, and Netweir's TLS is BoringSSL, tuned
to look like a browser; a second TLS stack for one feature isn't worth
it yet. `rediss://` is refused with a message saying so. (Known limit.)

Redis 6.2 or later is needed, for `ZRANGESTORE`, `SMISMEMBER` and
`ZADD LT`. Three steps that must not be split, or retried twice, are
short Lua scripts sent with `EVAL`.

## What's shared, what isn't

Shared, in Redis: the requests waiting to be fetched, in priority order;
the fingerprints of every request ever queued (the duplicate filter);
which process holds which requests; and the processes themselves, by
heartbeat.

Each process's own: everything else. Its robots.txt and TDMRep answers,
per-host delays and concurrency, proxies and their health, sessions and
cookies, the cache, its workers, its pipelines and output files, its
stats and limits.

## Data in Redis

Every key starts with the crawl's `queue_key` (default: the spider's
`name`, or its class name):

| key | type | holds |
|---|---|---|
| `KEY:requests` | sorted set | requests waiting; score `-priority` |
| `KEY:seen` | set | 16-byte fingerprints of every request queued |
| `KEY:seq` | string | a counter, for each request's place in line |
| `KEY:queued` | string | requests ever put in `KEY:requests` |
| `KEY:processes` | sorted set | process ids; score: when its lease runs out (ms, Redis's clock) |
| `KEY:held:ID` | sorted set | the requests process `ID` has taken and not finished, scores kept |
| `KEY:taking:ID` | sorted set | scratch for one lease; never outlives its transaction |

A request is one member: 16 hex digits of its number from `KEY:seq`,
then the request as JSON (URL, method, headers, body in hex, priority,
depth, `dont_filter`, proxy, referer and the caller's payload: callback
names, meta and cb_kwargs, or the rule that found it), the same fields a checkpoint
keeps. Members with the same score sort by their bytes, so the hex
number puts equal priorities in the order they were queued, across
every process. Each member is unique, so the same URL queued twice with
`dont_filter` is two members.

## Queueing a request

A process sends what its callbacks and rules queue in batches:

1. `INCRBY KEY:seq n` and `SMISMEMBER KEY:seen fp...`, pipelined: numbers
   for the batch, and which fingerprints are seen already. Those are
   counted as duplicates and go no further (unless `dont_filter`). Each
   request left becomes its member string here, once.
2. One script, for the batch: for each request, `SADD KEY:seen fp`, and if
   that added it (or the request has `dont_filter`), `ZADD NX
   KEY:requests`; then `INCRBY KEY:queued` by how many it queued. It
   answers 1 for each request the batch queued, and also for one whose
   member is already waiting in `KEY:requests`, which only this batch
   could have put there; 0 for a duplicate.

Seen and queued happen together or not at all, so a URL is never seen
without being queued, which would mean no one ever fetched it. Two
processes that find the same new URL at once both get past step 1, but
only one `SADD` adds it: the other's copy is never queued. (All processes
start by queueing the same start URLs at the same moment, so this is the
usual case at the start, not a rare race.)

A reply can be lost after Redis ran the script: the connection drops, or
the answer comes too late. The writer then sends the same batch again,
with the same member strings from step 1. The second run finds every
request seen and queues nothing more, and `NX` keeps a `dont_filter`
request to the one copy. Requests still waiting count as pushed on the
second run, not as duplicates, so the stats come out the same as without
the lost reply; one already leased by then counts as a duplicate.
Known limit: if a `dont_filter` copy was leased (or leased and released)
between the two runs, the second run queues it again, and it's fetched
twice if another process leases that copy before the holder's release
takes it out. An earlier draft went through the held set in two transactions,
and a retry after a lost reply then left requests stuck there for good,
with the crawl never ending.

Each process also keeps the fingerprints it has queued, as a crawl does
today, so a link it has seen costs no round trip.

## Leasing

A process takes requests when it has fewer than `concurrency` waiting
locally, up to `concurrency` at a time, in one transaction:

```
MULTI
ZADD KEY:processes <now + lease> ID
ZRANGESTORE KEY:taking:ID KEY:requests 0 n-1
ZREMRANGEBYRANK KEY:requests 0 n-1
ZUNIONSTORE KEY:held:ID 2 KEY:held:ID KEY:taking:ID AGGREGATE MIN
ZRANGE KEY:taking:ID 0 -1
DEL KEY:taking:ID
EXEC
```

The top `n` requests move from the queue to the process's held set at
once: there's no moment when a request is in neither, so no crash can
lose one, and two processes can't take the same one. Registering the
process in the same transaction means no held set exists without a
process entry to expire.

The request then goes through the engine as a checkpointed request does
on resume: it's queued locally under a row number, and when the caller
has finished with its events (its callback ran, and the requests and
items it yielded were handed on), `ZREM KEY:held:ID member` and `ZREM
KEY:requests member` release it. The second is for a request that was
given back while this process still had it (below): finishing it takes
the copy in the queue away again if no one has taken it yet.

Releasing waits for the requests the page queued: both go to the same
writer in order, and a batch's pushes are sent before its releases. So a
page is never finished in Redis before what it led to is queued there,
the same rule the checkpoint keeps.

A request taken but refused when it's queued locally (offsite, too deep,
a trap, over `max_pages_per_domain`: settings the processes should share,
but may not) is released at once, and so gone for the whole crawl. So is
one whose member can't be read (not UTF-8, not Netweir's JSON, a body
that isn't hex; members are kept as their bytes throughout, so one that
isn't UTF-8 is still released by exactly those) or can't be made a
Request in Python, whether by itself or through the middlewares for a
pushed URL (a payload that isn't a JSON
object, a kind of request there's no such thing as): each is logged and
counted as `queue_bad`, so one bad member can't stop every process that
takes it in turn.

A lease's reply can be lost too, after its transaction ran: the requests
are then in the process's held set, and the process doesn't know. It
keeps the members it knows it holds, and after a failed lease, and with
every heartbeat, compares them with `ZRANGE KEY:held:ID`; anything else
goes back to the queue (a script: `ZSCORE`, `ZADD LT KEY:requests`,
`ZREM`). Nothing else changes the held set meanwhile, since the process
does its Redis work one step at a time.

## Heartbeats and dead processes

Every `queue_lease / 3` seconds a process sets its own entry to
`now + queue_lease` and looks for processes whose time has run out. For
each, one script:

```
if ZSCORE KEY:processes DEAD > now: stop, it beat since
for each member, score in ZRANGE KEY:held:DEAD 0 -1 WITHSCORES:
    ZADD KEY:requests LT score member
DEL KEY:held:DEAD
ZREM KEY:processes DEAD
```

Its requests go back in the queue with their priorities, atomically,
at a cost of the held set's size, not the queue's. The deadline is
checked inside the script, so a process that beat between being read as
expired and being reaped keeps what it has. Two processes doing it at
once is harmless: the second finds no entry. They're counted in the
reaper's stats as `leases_reclaimed`.

`now` is Redis's clock (`TIME`), read with each heartbeat and carried
forward with the process's monotonic clock in between, so machines whose
clocks disagree still agree on when a lease runs out.

A process that was only slow (stopped in a debugger, or cut off from
Redis for longer than `queue_lease`) and is reaped while alive carries
on: its next heartbeat or lease registers it again, and the requests it
had are fetched again by whoever takes them. Delivery is at least once,
as with a checkpoint. A process stops with `netweir.QueueError` once
`queue_lease` seconds have gone by since its last heartbeat got through,
rather than go on alone: by then the others have taken it for dead. It
counts from when the last heartbeat that got through was sent, not from
the first failure, since that's when the others' count starts. It also
stops once every step has failed for a whole lease, even if heartbeats
get through meanwhile: a push that never succeeds would otherwise be
tried for ever.

No call waits longer than a third of a lease. The client's own timeouts
don't bound that: they apply to each connection attempt, and a call made
while it reconnects waits for up to three attempts with a backoff
between them, more than a lease in all. So each call is wrapped in a
timeout of its own; a call cut short may still have been applied, which
is the lost reply above. Connecting at the start is bounded the same
way, so a server that never answers is reported within a third of a
lease, not after three attempts.

Heartbeats are sent between the batches of a long backlog of pushes too,
so a live process busy sending doesn't go quiet for a lease. (`queue_lease`
defaults to 60 s; a shorter one finds dead processes sooner and makes a
slow one look dead sooner.)

A process that stops on its own (finished, a limit, Ctrl-C, an
exception) gives back what it holds in the reaper's transaction, with
itself as the dead process, so its requests don't wait out a lease.

## When the crawl ends

Without a queue a crawl ends when nothing is queued or in flight. With
one, "nothing" has to hold for every process. A process ends when, for
`queue_idle` seconds (default 30) without a break:

- it has nothing queued, in flight or waiting to be sent to Redis, and
- `KEY:requests` is empty, every live process's held set is empty, and
  `KEY:queued` hasn't moved (nothing was queued since the last look).
  These are read in one script, so a lease or a reap in the middle of
  reading can't make the queue look empty when it isn't.

If, after that, the process leases something after all (pushed in the
last moment), it's no longer over: it waits for a whole grace of quiet
again, and it never ends while it holds a leased request it hasn't
queued yet.

It looks four times a second. A page being worked on anywhere is held
until the requests it yields are queued, so between "the queue is empty"
and "the crawl is over" there's no gap a process could fall through. The
grace period covers the start: processes started a little apart, or a
first process whose start requests haven't reached Redis yet.

If something is queued between two looks and finished before the next,
a process can end while others are still busy. Nothing is lost: the
others carry on, with one process fewer.

`queue_idle=None` never ends: the processes wait for requests pushed
from outside, until stopped. That's a feeder crawl.

Every process ends with `finish_reason` `"finished"` when the queue ran
dry, or the limit that stopped it.

## Feeding a crawl

`netweir queue push redis://host/0 KEY URL...` (or `-` to read URLs from
standard input) queues URLs as start requests: callback `parse`, depth 0,
filtered by the duplicate filter unless `--dont-filter`. In Python,
`netweir.Queue(url, key).push(urls)`. Both send a thousand at a time,
and the command reads standard input as it goes. A pushed URL is marked
as pushed,
and the process that takes it puts it through the spider's middlewares'
`process_request` first, as a start request of its own would be; a
request a spider queued has been through them already in the process
that queued it.

`netweir queue status` prints how many requests wait, are held and by
how many live processes, how many fingerprints are seen, and how many
requests were ever queued. `netweir queue clear` deletes every key of
the crawl.

A URL, key or lease that can't work raises ValueError, and the command
exits 2, as for any other usage mistake. The URL is checked whole (a
port that isn't a number, say) by the client's own parser, without
connecting, when `Settings` is made, so `netweir crawl --queue` says so in
one line too. A server that can't be reached,
answers with an error, or is lost for longer than a lease raises
`netweir.QueueError`, a `ConnectionError`, and the commands print one
line and exit 1.

There's no separate start-URL list as scrapy-redis has: a pushed URL is
a request like any other, so taking it is the lease above and can't be
lost.

## Starting, stopping and running again

Every process runs the spider's `start()`. The first to queue a start URL
wins; the others' copies are duplicates. That makes the crawl's state in
Redis what a checkpoint is to one process: stop every process, start them
again, and they carry on, the held requests of the ones that died coming
back as their leases run out. To crawl again from scratch, clear the key.

The seen set is the queue's memory: a key remembers every request queued
under it for as long as you keep it, so a second crawl under the same
key fetches only what's new, listing pages included. That's stricter
than deltafetch, which fetches listings every run; pushing the listings
with `dont_filter` gets the same effect. A new key, or a cleared one,
crawls everything again.

## With other settings

Refused, with a message saying why:

- `checkpoint`: the queue in Redis is the crawl's saved state, and a
  checkpoint would be a second one that only one process could resume.
- `deltafetch`: its record is a file on one machine, which the other
  processes would never see; the seen set is the queue's own memory
  (above).
- `queue_lease` that isn't positive, or `queue_idle` that's neither
  positive nor None.
- An empty key.
- A `rediss://` URL (above), or any scheme but `redis://`,
  `redis+unix://` and `unix://`; the last two also on Windows, where the
  client has no unix sockets, with a message saying so.
- Output to cloud storage (`s3://`, `gs://`, any fsspec URL): it's
  uploaded only when the file is closed, so a process killed before then
  would lose items whose pages Redis has as done.

Per process, documented: `concurrency`, `per_domain`, delays and the
throttle, so N processes fetch from one site up to N times as fast;
lower `per_domain` or raise `min_delay` to match. A shared rate limit
would need a round trip to Redis before every request to a host and
would put Redis on every fetch's path; it isn't built. Also per process:
`max_items`, `max_pages`, `max_errors`, `max_time`, `max_memory` (each
stops its own process; a request it had taken and not finished is given
back), `max_pages_per_domain` (counted among the requests each process
leases; one over the limit is released unfetched, so N processes may
fetch up to N times it), robots.txt and TDMRep (each process reads them
once per site),
proxies, the cache, workers.

Items go to each process's own pipelines and files. Write to a file per
process (`-o items-$(hostname).jsonl`) and combine them afterwards, or
send them to a shared store from a pipeline. A process that dies loses
items it had in memory: JSON Lines, JSON, XML and CSV output is flushed
before the requests of each batch are released, so what was released is
on disk; Parquet is held in memory until the file is closed, so a killed
process loses its Parquet file. Items have no `_id` (that comes with a
checkpoint).

Callbacks and meta travel by name and as JSON, as with a checkpoint: a
callback must be a method of the spider, and meta must be JSON.

## Failure modes

| what | what happens |
|---|---|
| a process is killed | its held requests return to the queue `queue_lease` seconds after its last heartbeat; items it hadn't flushed are lost |
| a process stops normally | its held requests return to the queue at once |
| Redis restarts with its data | the connection manager reconnects; commands that failed are tried again, at most a second apart |
| a reply is lost after Redis applied the command | a push is sent again and queues nothing more; a lease's requests go back at the next check |
| Redis is slow, or the client is reconnecting | no call waits more than a third of a lease; a step that fails for a whole lease stops the process |
| a long backlog of pushes | heartbeats go between its batches |
| Redis is gone for longer than `queue_lease` | each process stops with `netweir.QueueError` |
| Redis loses its data | the crawl's state is gone; processes find an empty queue and end after `queue_idle` |
| two processes find the same new URL at once | one queues it; the other drops it |
| a process dies while queueing | the batch was queued whole or not at all; its page goes back with what the process held, and its links are found again |
| a slow process is taken for dead | its requests are fetched twice |
| a member that isn't a request Netweir can use | dropped, logged and counted as `queue_bad` |
| the queue's task panics | the crawl stops with `netweir.QueueError`, not silently |

## Known limits

- No TLS to Redis (`rediss://`), and no Redis Cluster or Sentinel: one
  server or a proxy in front of one.
- No shared rate limit per site.
- No unix sockets on Windows.
- A callback that hangs for ever keeps its process beating, so the
  requests it holds never go back, and with `queue_idle` set no process
  ends while they're held. The way out is to stop the stuck process: its
  lease then runs out and its requests go back.
- The seen set grows by 16 bytes plus Redis's overhead per request,
  about 80 bytes, forever: 100 million requests is around 8 GB. There's
  no Bloom filter option.
- A request's whole JSON is the member, so very large bodies or meta
  make the queue large.
- A request one process refuses locally (offsite, too deep, a trap,
  `max_pages_per_domain`) is gone for the whole crawl, not left for
  another.
- Anyone who can write to the Redis steers every process: the URLs they
  fetch (internal addresses such as 169.254.169.254 included), proxies,
  headers, bodies, and which spider methods are called. The traffic is
  plain TCP. The server needs a password and a trusted network.

## Testing

The queue logic (push, lease, release, heartbeat, reap, give back,
status, clear) runs in Rust against an in-process stand-in for Redis
that implements the commands above, so it's always tested; the same
tests run against a real `redis-server` when one is on the PATH. The
engine's part (leasing, releasing, ending) is tested from Python, with
real processes, against a second stand-in that speaks the Redis protocol
from inside the test process, and again against a real `redis-server`
when there is one:

- Two processes crawl one site: every page fetched once, every item
  written once between them, and both did some of it.
- A process killed with SIGKILL mid-callback: its request is fetched by
  another once its lease runs out, and nothing is lost.
- A process ends once the queue has been empty for `queue_idle`; with
  `queue_idle=None`, one keeps waiting, takes a URL pushed later, and
  passes it through the middlewares.
- Rules made in Rust go through the queue.
- A process stopped by `max_pages` gives back what it held, and a second
  run takes up only the rest.
- Refused combinations and settings, meta that isn't JSON, a Redis that
  can't be reached, cloud outputs; ValueError and `QueueError` where
  each belongs, and the exit codes that go with them.
- A push and a lease whose replies are lost after Redis applied them
  (both stand-ins can apply a command and then drop the connection): the
  crawl still ends, with every item; a retried `dont_filter` push makes
  one copy; a resent batch counts as pushed, not as duplicates.
- A reap that finds the process has beaten since keeps its requests.
- Members that can't be read (one not even UTF-8 among them), or made
  Requests (a pushed one through the middlewares too), are dropped and
  counted, and a body that isn't ASCII hex is an error, not a panic.
- `status` is one round trip.
- A lease whose release of an unreadable member fails gives its good
  requests back at the next heartbeat; a lost lease reply is tested only
  when the lease took something.
- Connecting to, and calling while reconnecting to, a server that never
  answers each end within a third of a lease (not on Windows, which
  resets a full backlog instead of leaving it unanswered).
- A process cut off from Redis (the stand-in stops answering) stops
  within a lease of its last heartbeat; so does one whose reconnects all
  land on a port that never answers; and so does one whose pushes all
  take longer than a call may, though its heartbeats get through.
- A backlog of pushes longer than a lease never leaves a lease without a
  heartbeat.
- Work leased after the queue had gone quiet gets a whole grace again.
- A malformed queue URL is a usage error, found without connecting.
- A panic in the queue's task ends the crawl with a fatal event.
- Passwords in a URL's user part or in `pass`/`password` parameters are
  never shown.
- `netweir queue push`, `status` and `clear`, and pushes in batches of a
  thousand.

## Milestone

**M31, a shared queue.** `queue`, `queue_key`, `queue_lease` and
`queue_idle`; the queue in Rust, the engine's leasing and ending, the
Python side, `netweir crawl --queue`, `netweir queue`, `netweir.Queue`,
the tests above, docs.
