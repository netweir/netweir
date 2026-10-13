# Keeping what a crawl fetched: WARC and HAR

Two kinds of people want the raw exchanges a crawl made, as well as the
items it pulled out of them. Some have to keep an exact record: an
archive, a compliance trail, or pages to parse again next month without
asking the site twice. They write WARC files, the web archive format
(ISO 28500); Scrapy users add scrapy-warcio or similar. Others are
debugging a crawl and want to see what went over the wire in a browser's
network panel, which reads HAR. This adds both, as two settings.

## Settings

- `warc`: a path ending in `.warc.gz` (each record gzipped on its own) or
  `.warc` (plain), or `None` (the default).
- `har`: a path to a HAR file, or `None` (the default).

Either can be set without the other, but not both to the same file:
`Settings` and the engine both refuse two paths to one file, whether
they're one path written twice, a link to the other, or names that differ
only in letter case (one file on macOS's and Windows' disks, so refused
everywhere). The directory has to exist already; `Settings` checks that,
and the extension of `warc`, when it's made. Without a checkpoint, each run starts its files
afresh, as it does its output files; a crawl resumed from a checkpoint adds
to them (see [Crashes and resuming](#crashes-and-resuming)).

The files are opened when the crawl starts, at the same point its output
files are: after every pipeline's `open_spider` has worked. Before that the
engine only checks them (the directory, and that a file it's going to add
to is one Netweir wrote), so a crawl that fails to start leaves them as
they were. Requests resumed from a checkpoint may be fetched in between;
their exchanges wait in the queue until the files are open. Both files are
read and judged before either is changed, so a HAR file that can't be
added to leaves the WARC file as it was too.

Python opens them with `Crawler::start_archive`. A Rust caller that never
calls it still gets its files: they're opened the first time it reads the
crawl's events (`next`), and a file that can't be opened then is a warning
event, its exchanges counted in `archive_errors`, rather than a crawl that
waits for good on a queue nobody empties.

## What goes in

Every response Netweir's HTTP client got during the crawl:

- each page, and each hop of a redirect, since the engine follows
  redirects a hop at a time and each hop is its own request;
- every try of a page that was retried, and every block page, because they
  were fetched: the archive is of the traffic, not of the pages the
  callbacks saw;
- robots.txt, `/.well-known/tdmrep.json` and sitemaps, which the crawl
  fetches for itself;
- pages that never reach Python, such as those a declarative rule follows
  and extracts in Rust.

Left out:

- a request that got no response: a network error, a timeout, or a body
  over `max_response_size`, which is abandoned part way;
- a page served from the [cache](cache.md): nothing was fetched;
- a page fetched in Chrome. Netweir never sees the bytes Chrome got from
  the server, only the page once its scripts have run. A `resource` record
  of that rendered HTML could be written, but it isn't what the site sent,
  and a record that looks like a fetch and isn't one does more harm in an
  archive than a gap does. robots.txt and TDMRep fetches made on Chrome's
  behalf go through the HTTP client, and are recorded.

Capture sits in the fetcher, at the one place every request of the crawl
goes through (`Fetcher::hop_as`): the crawl's own fetches, the robots.txt
and TDMRep gate fetches, and every session and proxy the crawl makes. The
crawl hands its fetch options an `Archive`, and every client made from them
records into it. `netweir.fetch()` and the shell make their own options and
record nothing.

## The WARC file

WARC/1.1, as warcio writes it. A run starts with a `warcinfo` record
(`software: netweir/<version>`, the format and `conformsTo`), then for each
response a `request` record and a `response` record, the request linked to
the response by `WARC-Concurrent-To`. Each record has `WARC-Record-ID` (a
`urn:uuid`), `WARC-Date` (when the request was sent, UTC, to the
microsecond), `WARC-Target-URI`, `WARC-Block-Digest` and
`WARC-Payload-Digest` (both `sha1:` and base 32, warcio's form), and
`WARC-Protocol` (`http/1.0`, `http/1.1` or `h2`, the protocol the response
came in; a request is `http/1.1` or `h2`, as Netweir never sends 1.0). In a
`.warc.gz` every record is a gzip member of its own, so a reader can seek
to any record, and a file can be appended to.

### The request record

The request as Netweir sent it. The header lines are captured from the
HTTP client as it writes them to the connection, in order and with their
capitalisation: what the profile and the caller set, the cookie jar's
Cookie, the Host and Content-Length the client fills in. Nothing is
reconstructed. wreq's emulation sets no header of its own beyond the
profile's: every default header comes from Netweir's profile file, so the
captured lines are the whole request.

To capture them, the fetcher replaces wreq's header-ordering hook for the
request with one that orders the headers exactly as wreq's own does and
writes down what it emits. That only works if the client has no default
order to put back in its place, so the emulation carries none and every
request sets its order itself, as the fetcher already did.

Three things differ from the bytes on the wire:

- A request that went over HTTP/2 is written in HTTP/1.1 form, as WARC
  has no other: a request line (`GET /path?query HTTP/1.1`), then the
  header lines in the order they went into the HEADERS frame, lowercase as
  HTTP/2 sends them. HTTP/2's pseudo-headers (`:method`, `:authority`,
  `:scheme`, `:path`) aren't header lines; the request line and
  `WARC-Target-URI` say the same. `WARC-Protocol: h2` says which it was.
- The request line has the path and query, as a request to the site
  itself has; through a plain-HTTP proxy, the line on the wire had the
  whole URL.
- `Proxy-Authorization` is left out. It's the login for the crawl's own
  proxy, never sent to the site, and it doesn't belong in an archive anyone
  may later share.

The body is the one sent, for a form or any other request with one.

### The response record

The status line has the version the server answered in (`HTTP/1.0` or
`HTTP/1.1`; an HTTP/2 response is written as `HTTP/1.1`, with
`WARC-Protocol: h2`) and the server's own reason phrase.

The headers are what the HTTP client hands Netweir, which isn't quite the
lines the server wrote. The client keeps them in a map: names come out
lowercase, and a name sent more than once has its values together, in the
order they came, where the name first appeared. Two `Set-Cookie` lines with
another header between them are written one after the other, that header
after both. Values are byte for byte, Latin-1 and all. wreq offers no way
to see the lines as they came, so this is the closest the record can be.

The client decompresses a body as it reads it, and Netweir keeps only the
result, so the record holds the decompressed body. The client takes the
`Content-Encoding` and `Content-Length` headers away when it does, so
neither is in the record, and nothing says the body came compressed. The
stored body is dechunked too. So, where the record can say it truthfully:

- `Transfer-Encoding` is left out;
- `Content-Length` is the stored body's length. It's kept as sent when it
  already is, added when the server sent none (chunked, or the client took
  it away), and rewritten in the rare case it was there and differs;
- a status that has no body (1xx, 204, 304) and a HEAD's answer keep their
  headers as sent, `Content-Length` included, and get none added, since
  there's no stored body for one to describe.

The payload digest is of the stored body. A tool that wants the exact
compressed bytes off the wire won't find them here; a tool that wants the
page, and wants to know the record wasn't altered after it was written,
has what it needs, and warcio verifies the digests.

A redirect's body is read the same way whether or not the crawl keeps an
archive, so archiving doesn't change how connections are used: up to
64 KiB of it, for at most two seconds. A short body is read whole and the
connection goes back to the pool for the next request; a longer or slower
one is dropped with its connection, as a browser would drop it, and its
record holds what was read, with `WARC-Truncated: length` and the
`Content-Length` the server sent.

## The HAR file

HAR 1.2: `log.version`, `log.creator` (`netweir` and its version) and one
entry per response, the same responses as the WARC file. An entry has
`startedDateTime`, `time`, the request (method, URL, `httpVersion`,
headers as captured above, `queryString`, `postData` for a body), the
response (status, `statusText`, `httpVersion`, headers, `content`,
`redirectURL`), `cache` and `timings`. `cookies` are empty lists: the
Cookie and Set-Cookie headers are there in full.

- `content.text` is the body as text if it's valid UTF-8, and otherwise
  base64 with `"encoding": "base64"`, so an image or a PDF comes back
  byte for byte. `content.size` is the decompressed length;
  `content.mimeType` is the Content-Type, or empty.
- `postData` has the request body's `mimeType` and `text`, and for an
  `application/x-www-form-urlencoded` body its `params`, decoded. A body
  that isn't UTF-8 is written as text with invalid bytes replaced: HAR has
  no base64 for `postData`. The WARC file has it exactly.
- Header values that aren't UTF-8 are written the same way.
- `timings.wait` is from sending the request to its response headers,
  connecting included, since the client doesn't report the steps apart;
  `receive` is the body; `send` is 0, and `blocked`, `dns`, `connect` and
  `ssl` are -1, HAR's "not known". `time` is their sum.
- `bodySize` of the response is -1: only the decompressed size is known.
- The request's `httpVersion` is the one it went out in, `HTTP/1.1` or
  `HTTP/2`, as in the WARC request record; the response's is the one it
  came back in, which may be `HTTP/1.0`.
- A redirect body only partly read (see the response record) has
  `"_truncated": true` in its `content`, a field of Netweir's own, named
  with the underscore HAR keeps for those.

The file is one JSON object written as the crawl goes: the head
(`{"log": {"version": ..., "creator": ..., "entries": [`), then each entry
on a line of its own, then the closing `]}}` when the crawl closes. Each
entry is a single line because newlines in strings are escaped, so a line
is always a whole entry or a torn one.

## Writing without holding up the crawl

Records are written by a thread of their own. A fetch that has a response
hands it over through a channel with room for 64 exchanges and 64 MiB of
bodies (a larger body goes on its own), and awaits room if it's full: a
disk slower than the network slows the crawl down rather than dropping
records or filling memory. Waiting is an await, never a blocked runtime
thread. The room is two semaphores, a place and the bytes, taken before
the exchange goes on an unbounded channel; the channel itself never makes
anyone wait.

That matters when the crawl closes. Closing may run on the runtime's only
thread, where the fetches waiting for room are tasks that can't run until
it returns, so it can't wait for any of them. It shuts both semaphores, so
every fetch still waiting gives up the moment it's next polled, puts a
Close on the channel behind what's queued, and waits for the writer to
write what's ahead of it and finish.

An exchange holds the body the crawl already has, shared, not copied, and
the writer streams each record from it: the digests are worked out over the
buffers first, then the record's head and the body go to the file through a
64 KiB buffer (and, for a `.warc.gz`, a gzip encoder per record). A HAR
entry is written the same way, the body escaped or base64-encoded a piece
at a time. However large the body, the archive adds kilobytes to a crawl's
memory, not another copy of it.

Each record is flushed to the operating system before the next is
started, so a crawl killed with `kill -9` loses at most the exchanges in
the channel. Closing the crawl writes the HAR file's closing `]}}` and
syncs both files to disk. Dropping the engine without closing it does the
same, so the HAR file is ended and both files are let go of.

Fetches still under way when the files close (a crawl interrupted, or
stopped by `fail_fast`) have nowhere to go: their exchanges aren't written,
and each counts in `archive_errors`. The crawl's stats are read before it
closes, so its own stats can't show those; their pages never reached a
callback either.

### Crashes and resuming

What a reader sees after a crash: every record written before it, then
perhaps a record cut off part way. warcio (1.8) reads a `.warc.gz` to the
end of the last whole member and stops there without complaint, as zlib
gives up quietly on a member cut short; on a `.warc` it reads every whole
record and then raises an error at the torn one. A HAR file has no closing
`]}}`, so it isn't valid JSON until something closes it.

A crawl resumed from a checkpoint repairs that before it adds anything. It
reads the file from the start: for a `.warc.gz`, it decompresses each
member, and cuts the file back to the end of the last one that's whole;
for a `.warc`, it walks the records by their `Content-Length` and cuts the
file back to the end of the last one whose block and trailing blank lines
are all there. For a HAR file it takes a closing `]}}` off the end, and
drops the last line if it isn't a whole entry; a file cut off before its
head was finished is started again from nothing. Then it writes a new
`warcinfo` record and carries on. Reading the whole of a large `.warc.gz`
takes a while (about as long as `gunzip` would), but only when resuming.

A file that doesn't start as this writer's files do (`WARC/`, the gzip
magic number, or the HAR head) isn't added to: the crawl refuses to start,
naming the file.

A resumed crawl fetches again the pages whose requests weren't finished,
so those pages are in the archive twice, once from each run. The archive
records what was fetched, and they were fetched twice.

## When writing fails

A full disk or a file that can't be written doesn't stop the crawl. The
first failure is a warning naming the file and the error, and that file
gets nothing more for the rest of the run: a record added after one that
failed part way would sit after a torn one, which readers can't get past.
The stats count `archive_errors`, the responses that weren't written to a
file they should have gone to (once for each file), so a check can fail a
crawl whose archive is incomplete. The warning reaches the crawl with the
next page fetched.

Closing is different: a HAR file that can't be ended, or a file that can't
be synced to disk, is an error when the crawl closes (`OSError` in
Python), as a checkpoint's failed write is. By then there's no crawl left
to keep going, and a file that may not be on disk shouldn't pass quietly.
