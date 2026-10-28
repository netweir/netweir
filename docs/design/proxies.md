# Proxy pools: rotation and health

Today `proxies` is a list a site moves through when it blocks a session:
the crawl uses `proxy` (or none) until a host blocks it, and that host's
next session takes the next proxy. People with a pool of proxies want
more than that. Spreading every request over the pool, so no one address
carries a crawl. Keeping one address per site, so a site sees a steady
visitor. And taking a proxy out of the rotation when it stops working,
then trying it again later, rather than sending a third of the crawl into
a dead one. Scrapy users get this from scrapy-rotating-proxies; this
builds it into the engine, where the fetching is.

## Rotation

`proxy_rotation` says how `proxies` are used:

- `"on_block"` (the default, today's behaviour): the crawl's own `proxy`
  until a host blocks a session; then the host's new session takes the
  next proxy in the list.
- `"per_request"`: every request takes the next healthy proxy, round
  robin. Cookies are the crawl's, whichever proxy carries them, as a
  browser behind a rotating gateway would have them. A block is answered
  by the rotation itself: the retry goes out through another proxy. A
  block also drops the blocked host's cookies from the crawl's jar (and
  any its parent domains set), as a new session would: every proxy's
  client shares the one jar, and the jar can remove cookies by domain,
  path and name.
- `"per_host"`: each host gets a proxy of its own the first time a
  request of its takes the rotation (one naming its own proxy, or going
  direct, doesn't, and robots.txt goes the way the first waiting request
  does), the next healthy one in turn, and keeps it; its session (its
  cookies) goes with it. A block, or the proxy being taken out, moves the
  host to the next healthy proxy with a fresh session.

With `"per_request"` or `"per_host"`, `proxies` must hold at least one
proxy, and `proxy`, if given too, is refused: it's not clear which wins.

## Health

A proxy fails a request when the connection to the proxy fails, the
proxy wants a login (a 407, or a CONNECT refused with 407), the proxy
breaks off a tunnel (an early close, a bad answer to CONNECT), or the
answer is a block. Any other answer, a 404 or a 500 included, is the
site's, and a success for the proxy. So are failures the client can't
pin on the proxy: a CONNECT refused with any other status (wreq keeps
no status for it, and it's mostly 502/503/504, the proxy failing to
reach the site), a TLS error with the site inside an open tunnel, and
any timeout (wreq reports a slow proxy and a slow site alike). Leaning
that way keeps dead or slow sites from benching healthy proxies; a proxy
that accepts connections and never answers isn't benched, which costs a
timeout per request through it. The classification reads wreq's error
chain (`client error (ProxyConnect)`, `tunnel error: unsuccessful`),
since its tunnel errors aren't public. After `proxy_failures` failures in a row (3), the
proxy is benched: left out of rotation for `proxy_cooldown` seconds (60),
twice that the next time it's benched, and so on up to 30 minutes. A
benched proxy comes back on its own when its time is up, and one success
clears its record.

When every proxy is benched, the crawl doesn't stall: requests go to the
one coming back soonest, and a warning says the pool is exhausted (once,
until a proxy is healthy again).

A proxy back from the bench goes straight back on it if its next request
fails: the run of failures is only cleared by a success. A 407 from a
proxy the pool picked is retried, like a connection error, since the
next try may go through another. robots.txt and tdmrep.json go through
the pool as a host's pages would; if a pool proxy fails to fetch one,
it's fetched again (only when the proxy was at fault, by the rules
above) through each healthy pool proxy that hasn't failed that origin yet, whatever `retries` says (even 0), and a host with a
session of its own moves to the proxy picked. So while a healthy proxy
is left, a dead one can't disallow a whole site. When none is left, or a
proxy outside the pool fails, the file is unreachable, as a network
error makes it: robots.txt disallows everything for the crawl, tdmrep.json
counts as none. A 407 on either is read the same way, never as the
site's 4xx. Health doesn't read 502, 503 or 504: a gateway's and the
site's look the same.

Benching is logged (the proxy's address with any login hidden: `http://
***@host:port`), and counted as `proxies_benched` in the stats. After a
run, `spider.proxy_stats` has, for each proxy, how many requests it
carried, how many failed, and how many times it was benched.

## One request, one proxy

`Request(url, proxy="http://host:port")` sends that request through that
proxy, whatever the rotation; so does `meta={"proxy": ...}`, as in Scrapy.
As in Scrapy, `meta={"proxy": None}` or `""` means no proxy: the request
goes direct whatever the rotation (the engine takes an empty proxy as
that, and the checkpoint keeps it). A proxy outside the pool gets its own
`proxy_stats` row up to 1,000 of them; the rest share an `"other"` row.
The clients kept for proxies requests name are bounded (least recently
used dropped): the pool's own plus 256 for the crawl's jar, 16 per host
session.
It counts toward that proxy's health if the proxy is in the pool, and
shows in `proxy_stats` either way. The request's proxy is saved in the
checkpoint (format 3 adds the column) and in a shared queue's members,
without its login: as `masked` writes it, with the SHA-256 of the whole
URL beside it (format 6). A resumed crawl takes the login from its own
proxies (the pool, `proxy` and Chrome's): the one whose URL hashes the
same, or else the only one at the same scheme, host and port, whose
password changed. With neither, the request goes as the crawl's other
requests do, and one warning names the proxy. Opening an older
checkpoint hides the logins already in it. A proxy the client can't use
skips the request with a warning, as an unusable URL does.

## Chrome

Chrome starts with one proxy for all its pages, so neither rotation
applies inside it. With `"on_block"` it goes through the crawl's `proxy`,
as before; with `"per_request"` or `"per_host"` (where there's no crawl
proxy), through the next healthy proxy of the pool when the browser is
started. The session a page hands Chrome's cookies to goes through that
proxy too, since the cookies are tied to its address. Giving each host
its own proxy inside Chrome would need a browser context per proxy, and
the proxy logins Chrome answers are per browser today.
