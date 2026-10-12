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
  by the rotation itself: the retry goes out through another proxy. (If
  the cookie store can drop one host's cookies, a block drops them, as a
  new session would; otherwise they're kept, and the docs say so.)
- `"per_host"`: each host gets a proxy of its own the first time it's
  fetched, the next healthy one in turn, and keeps it; its session (its
  cookies) goes with it. A block, or the proxy being taken out, moves the
  host to the next healthy proxy with a fresh session.

With `"per_request"` or `"per_host"`, `proxies` must hold at least one
proxy, and `proxy`, if given too, is refused: it's not clear which wins.

## Health

A proxy fails a request when the request through it ends in a connection
error, a timeout, a TLS error, a 407 (the proxy wants a login) or a
block. Any other answer, a 404 or a 500 included, is the site's, and a
success for the proxy. After `proxy_failures` failures in a row (3), the
proxy is benched: left out of rotation for `proxy_cooldown` seconds (60),
twice that the next time it's benched, and so on up to 30 minutes. A
benched proxy comes back on its own when its time is up, and one success
clears its record.

When every proxy is benched, the crawl doesn't stall: requests go to the
one coming back soonest, and a warning says the pool is exhausted (once,
until a proxy is healthy again).

Benching is logged (the proxy's address with any login hidden: `http://
***@host:port`), and counted as `proxies_benched` in the stats. After a
run, `spider.proxy_stats` has, for each proxy, how many requests it
carried, how many failed, and how many times it was benched.

## One request, one proxy

`Request(url, proxy="http://host:port")` sends that request through that
proxy, whatever the rotation; so does `meta={"proxy": ...}`, as in Scrapy.
It counts toward that proxy's health if the proxy is in the pool.

## Chrome

A page fetched in Chrome goes through the proxy its host's session has
(`"per_host"`, `"on_block"`), as today; with `"per_request"`, through the
next healthy proxy when the browser is started. Chrome starts with one
proxy for all its pages, so per-request rotation doesn't apply inside it.
