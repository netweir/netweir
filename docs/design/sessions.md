# Sessions and cookies

Status: draft, October 2026
Author: James Conway

Netweir keeps cookies, but until now it kept them to itself. A Client had
a jar nobody could read or fill; a crawl had one identity, so logging in
as two users meant two crawls; a page logged in through Chrome couldn't
hand its cookies to the HTTP client, nor the other way round, except in
the one case the engine did it alone (a blocked request passed in
Chrome). People coming from Scrapy, requests, httpx and Playwright reach
for the same handful of things on day one, and this adds them:

- Scrapy: `Request(cookies=...)`, `meta["cookiejar"]` for several sessions
  in one crawl, and `meta["dont_merge_cookies"]`.
- requests and httpx: a Client's cookies to read, set, delete and save.
- Playwright: a context's cookies (there already) and its storage state,
  cookies and localStorage, saved to a file and loaded into a new context.
- Both ways between Chrome and the HTTP client: log in with the browser
  and crawl over HTTP with the same cookies, or the reverse.

## One cookie format

Every API takes and gives the dicts `page.cookies()` already gives:

```python
{"name": "sid", "value": "...", "domain": ".shop.example", "path": "/",
 "expires": 1767225600.0, "http_only": True, "secure": True,
 "same_site": "Lax", "partition_key": None}
```

`domain` follows the browsers: a leading dot is a cookie for the domain
and its subdomains; without one, it's the host's alone (host-only).
`expires` is seconds since the epoch, -1 for a session cookie. Dicts in
Playwright's spelling (`httpOnly`, `sameSite`, `partitionKey`) are read
too, so a Playwright context's cookies go straight in. So handing over is
one line either way:

```python
client.cookies.add(await context.cookies())      # browser -> HTTP
await context.set_cookies(client.cookies.all())  # HTTP -> browser
```

A storage state is Playwright's file: `{"cookies": [...], "origins":
[{"origin": "https://shop.example", "localStorage": [{"name": ..., "value":
...}]}]}`, cookies in Playwright's spelling, so a file Playwright saved
loads in Netweir and the reverse.

## The jar

wreq's jar can't say which host a host-only cookie belongs to, so a jar
could never be read back whole, and couldn't be saved. Netweir's own jar
replaces it (`netweir_core::cookies::Jar`), written against RFC 6265bis
and what Chrome does where the draft leaves room:

- **Storing.** Domain matching, with the public suffix list: a cookie for
  `com` or `github.io` is refused, as Chrome refuses it, rather than sent
  to every site under it. Host-only and domain cookies of one name are
  two cookies. Max-Age wins over Expires and is stored as a deadline; a
  past one deletes. The default path is the request's directory. A
  `Secure` cookie only from https (or localhost and loopback addresses,
  which Chrome counts as secure), and an http response can't replace
  one. `__Secure-` needs `Secure`; `__Host-` needs `Secure`, no Domain and
  Path=/. `SameSite=None` needs `Secure`. A Set-Cookie without a name
  (`token`, or `=token`) is a cookie with an empty name, as in Chrome,
  sent as its value alone, unless the value starts like a prefixed name.
- **Limits.** Chrome's: a site (a registrable domain, subdomains and
  all) keeps 180 cookies, and one more drops it to 150, insecure cookies
  first and each kind least recently sent or set first. Past 3300 in
  the jar, the least recently used are dropped down to 3000, but only
  those unsent for a month, so a jar whose cookies are all in use may
  hold more, as Chrome's does. Expired cookies go when their site
  changes and when the jar is saved. The jar is written out after its
  lock is let go: a 50,000-cookie jar saved over and over kept a request
  waiting at most 9 ms, against 4.4 s when it was written under the
  lock.
- **Sending.** Domain, path, `Secure` (https, localhost and loopback
  only) and expiry, ordered as each browser orders them: longest path
  first, then oldest first (Chrome, Firefox) or newest first (Safari),
  as the profiles already say. SameSite as Chrome applies it: a
  cross-site request carries no `Strict` cookie; `Lax` ones (and those
  that set no SameSite, which Chrome treats as Lax) only on a top-level
  GET navigation; a cookie with no SameSite under two minutes old also
  on a top-level POST navigation, Chrome's "Lax allowing unsafe". A
  cross-site subresource response can't set a Lax or Strict cookie.
  Typed addresses (no referer) are never cross-site.
- **Partitioned cookies (CHIPS).** Kept under the site the request's top
  level is on: the target's for a navigation, the referer's for a
  script's request or an image. Sent only under the same top-level site.
  A partitioned cookie must be `Secure`.
- **Header shape.** Unchanged: the Cookie header goes where each profile's
  captures put it; over HTTP/1.1 the cookies are one field joined with
  `; `, over HTTP/2 each cookie is a field of its own, which is what
  Chrome's HPACK encoder puts on the wire (it splits the header into
  crumbs before encoding). A Cookie header the caller sets is sent as
  given, and the jar adds nothing to that request.

The jar is applied per request (wreq takes a cookie store per request),
so one client, with its connection pool, can send for several jars.

## The HTTP client

```python
client = netweir.Client(storage_state="state.json")  # or a dict
client.cookies.get("sid", domain="shop.example")     # the value, or None
client.cookies.set("sid", "abc", domain="shop.example", path="/")
client.cookies.delete("sid", domain="shop.example")
client.cookies.clear()                               # or clear("shop.example")
client.cookies.all()                                 # the dicts above
client.cookies.add(cookies)                          # dicts, from anywhere
len(client.cookies)
client.storage_state("state.json")                   # saves; returns the dict
```

`set` makes a host-only cookie for `domain` unless `domain` starts with a
dot, as the dicts do. A `storage_state` the client saves has an empty
`origins`: the HTTP client has no localStorage.

## Crawls

```python
Request(url, cookies={"currency": "EUR"})
Request(url, cookies=[{"name": "sid", "value": "...", "domain": ".shop.example"}])
Request(url, meta={"cookiejar": "ada"})
Request(url, meta={"dont_merge_cookies": True})
```

- `cookies` goes into the request's jar just before it's sent, as if the
  request's URL had set it, and is sent with it and every later request
  the jar matches (Scrapy). A dict of name to value, or a list of the
  dicts above. Without a domain, a cookie is the URL's host's alone;
  without a path, its path is the URL's directory, as Scrapy has it.
  The URL is the one submitted, bound when the request is queued: a
  redirect to another host never carries them there, nor puts them in
  the jar for it (Scrapy's GHSA-mfjm-vh54-3f96). Scrapy's dicts give `domain` without a dot and mean the domain and its
  subdomains; in Netweir that's `".shop.example"`, the browsers' way.
- `meta["cookiejar"]` names a jar: every request naming it shares its
  cookies, and no others do. Any JSON value names one (`1` and `"1"` are
  two jars, as in Scrapy); `None` is the crawl's own. Like the proxy and
  the callbacks, it travels with the request, so a page's
  `page.follow()` keeps the jar only if the callback passes the meta on,
  as in Scrapy. A named jar is part of the request's fingerprint, so
  the duplicate filter, the cache and deltafetch tell one user's request
  from another's for the same page: Bob is never served Ada's cached
  page. The crawl's own jar keeps the fingerprint as it was, so existing
  caches, checkpoints and deltafetch records stay valid.
- `meta["dont_merge_cookies"]` leaves the jar out of this request: none of
  its cookies are sent and nothing the response sets is kept. Unlike
  Scrapy, which then ignores `Request.cookies`, Netweir still sends the
  request's own `cookies`, alone: a request that names cookies and gets
  none sent is a surprise nobody wants.

### Named jars, sessions and proxies

The crawl's own jar is today's: with `proxy_rotation="on_block"` or
`"per_host"`, a host gets a fresh session (proxy and jar) after a block.
A named jar is the user's identity, a login, and Netweir never empties
it: after a block, the host's new session (its proxy and connection)
carries the named jar's cookies as before. Requests in a named jar go
through whatever proxy the rotation picks; a site that ties a login to
an address needs the jar's requests to name a proxy of their own
(`Request(proxy=...)`).

### Named jars in Chrome

A browser request in a named jar opens its page with the jar's cookies,
and when the page closes (after the callback, which may have logged in
through it), what changed in the page's context goes back into the jar:
cookies Chrome set or changed, and the removal of the jar's cookies it
deleted. A cookie Chrome left as it started isn't written back, so a
value the jar took in over HTTP while the page was open stays. A browser request
in the crawl's own jar keeps today's behaviour: a page of its own, with
only the request's own `cookies`, and on a block passed to Chrome, the
cookies Chrome earns go to the host's next session.

### Workers

The engine, and every jar with it, lives in the main process. Workers
run callbacks and send back requests; a request's cookies and jar travel
with it. So jars are consistent across workers without any sharing.

### Several machines (the Redis queue)

Out of scope, and said so in the docs. A request's `cookies` and
`meta["cookiejar"]` travel with it through the queue, but each process
has its own jars, and what a response sets stays in the jar of the
process that fetched it. Keeping one jar across machines would need a
round trip to Redis before every request and after every response, and
an ordering between processes that a jar's semantics (the newest cookie
wins) can't get from a queue. A distributed crawl that logs in should
put the login cookies on its requests.

### Checkpoints

The crawl's own jar and every named jar are saved with the checkpoint,
when the crawl settles a round, if they changed since the last save, and
restored on resume: a resumed crawl is still logged in. A host's session
after a block isn't saved; a resumed crawl starts it again. The format
goes up by one (`jars` table), and older files are upgraded on open.

## Browser contexts

```python
state = await context.storage_state()            # or (path="state.json")
context = await browser.new_context(storage_state="state.json")
state = await page.storage_state()               # a page in a context of its own
```

Cookies come from `Storage.getCookies`. localStorage is kept per origin,
and DevTools can only read or write it in a page of that origin, so
Netweir does what Playwright does: it remembers every origin the
context's pages loaded a document from, and for each opens a page of its
own in the context whose requests never leave Chrome (each is answered
with an empty page by the page's own routes), reads (or fills) the
origin's localStorage there, and closes it. sessionStorage and IndexedDB
aren't saved, as in Playwright. Host-only cookies stay host-only when
set into Chrome (given by URL, not domain), which they didn't before.

## Security

- Cookie values are never logged, nor put in any message or error.
- A storage state file is written with permissions 0600 (only its owner
  can read it) on Unix, through a new file renamed over the old one, so
  it's never readable on the way.
- A checkpoint holds the jars, so its file is made 0600 on Unix too.

## Testing

- The jar: each storing and sending rule above, ordering per profile,
  export and import with host-only cookies kept host-only, snapshots
  round trip.
- Fetcher: a request's SameSite and partition behaviour over a real
  socket; a jar swapped per request on one client.
- Client: get, set, delete, clear, all, add, storage state save and load,
  file mode 0600.
- Crawl: request cookies merge into the jar; two named jars stay apart on
  one site; dont_merge_cookies sends only the request's own and keeps
  nothing; a resumed crawl keeps its jars; a named jar survives a block.
- Browser: storage state with localStorage round trip through a new
  context; host-only cookies stay host-only; a crawl's browser request in
  a named jar takes the jar's cookies and gives them back.

## Milestone

**M48, sessions.** The jar, the Client's cookies and storage state,
`Request.cookies`, named jars and `dont_merge_cookies`, jars in the
checkpoint, storage state for browser contexts, the docs and tests above.
