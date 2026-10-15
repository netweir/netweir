# Fetch policy: timeouts, redirects, logins and certificates

Someone moving from requests, httpx or Scrapy reaches for a handful of
knobs on their first day: a timeout for one slow request, a redirect read
rather than followed, a login for an intranet, `verify=False` for a test
box with a self-signed certificate, `params=` for a query string. Netweir
had a client-wide timeout and nothing else. This adds the rest, per call
and per crawl, without changing what goes on the wire: every one of these
is decided on the client, or puts a header where the browser puts it.

## API

### `netweir.get()`, `request()`, `post()` and `Client`

```python
netweir.get(url, params={"q": "rust", "page": 2})   # merged into the URL's query
netweir.get(url, auth=("ada", "secret"))              # HTTP Basic
netweir.get("https://ada:secret@host/")               # the same, as requests reads it
netweir.get(url, allow_redirects=False)               # the 3xx itself; follow_redirects= too
netweir.get(url, max_redirects=5)                     # more is FetchError("too_many_redirects")
netweir.get(url, timeout=5)                           # seconds, for this call
netweir.get(url, verify=False)                        # no certificate check
netweir.get(url, verify="/etc/ssl/internal-ca.pem")   # trust these CAs instead

client = netweir.Client(verify=..., max_redirects=..., auth=..., timeout=...)
await client.get(url, params=..., auth=..., allow_redirects=..., timeout=...)
```

- `params` is a mapping or a list of pairs. Values that are lists repeat
  the key; `None` values are left out; the pairs are URL-encoded (spaces as
  `+`, as requests and an HTML form's GET write them) and added after the
  URL's own query, before its fragment. requests and httpx do the same.
- `auth` is `(user, password)`, sent as `Authorization: Basic`, the pair
  encoded as UTF-8 (RFC 7617). A login in the URL is moved into `auth`, so
  it never reaches the Host line or the archive, unless `auth` is given,
  which wins. A `Client`'s `auth` is used by every call that doesn't pass
  its own.
- `allow_redirects=False` (or httpx's `follow_redirects=False`; passing
  both, differently, is an error) returns the 3xx as the page:
  `page.status`, `page.headers["location"]` and its body. Cookies it set
  are kept, as when following.
- `max_redirects` defaults to 20, Chrome's limit. The redirect after the
  last allowed one fails with `FetchError(kind="too_many_redirects")`, as
  requests raises `TooManyRedirects`.
- `timeout` on a call replaces the client's for that call, longer or
  shorter. Like the client's, it covers one request from connecting to the
  last byte; each redirect is a request of its own.
- `verify` is `True` (the bundled roots, which are Chrome's), `False`, or
  the path of a PEM file of CA certificates, which then replaces the
  bundled roots, as requests and httpx do with a path. A file with no
  certificate in it is a `ValueError`.

### A crawl's `Request`

```python
Request(url, timeout=5, max_redirects=3, allow_redirects=False, auth=("ada", "secret"))
```

and the Scrapy meta keys, read when the field isn't set:

| meta key | same as |
|---|---|
| `download_timeout` | `timeout` |
| `dont_redirect` | `allow_redirects=False` |
| `redirect_ttl` | `max_redirects` (as in Scrapy, never above the setting) |
| `http_user`, `http_pass`, `http_auth_domain` | `auth`, for that domain and its subdomains (the request's own host without `http_auth_domain`) |

`auth=` on a Request goes to the request's own origin and is dropped when
a redirect leaves it, as a browser drops a page's Authorization. Netweir
honoured Scrapy's `proxy` and `deltafetch_*` meta keys already; these join
them.

### Settings

```python
Settings(
    timeout=30.0,                      # as before
    verify=True,                       # or False, or a PEM path
    max_redirects=20,                  # Scrapy's REDIRECT_MAX_TIMES
    retry_http_codes=(408, 429, 500, 502, 503, 504, 522, 524),   # RETRY_HTTP_CODES
    http_auth=("ada", "secret"),       # HTTPAUTH_USER / HTTPAUTH_PASS
    http_auth_domain="intranet.example",  # HTTPAUTH_DOMAIN: required with http_auth
)
```

- `retry_http_codes` is the statuses retried, with `retries` tries and the
  backoff as before. Its default is the set Netweir retried already plus
  429, which Netweir retried as throttling; that's Scrapy's default too. A
  status left out isn't retried: a 429 not in the list still slows the
  host down, but the response goes to the callback. A block (a bot
  protection page) is retried whatever its status, as before, and a POST
  still only after a status that says it wasn't processed.
- `http_auth` is sent to `http_auth_domain` and its subdomains, on every
  request and every redirect hop that goes there, and nowhere else: not
  to another host a redirect leads to, and not over plain HTTP once a
  request has started on HTTPS. Scrapy's `HTTPAUTH_DOMAIN=None` (send
  everywhere) has no equivalent: `http_auth` without a domain is a
  `ValueError`. robots.txt and tdmrep.json are fetched with it, as
  Scrapy's robots requests go through its auth middleware.

## On the wire

- **Certificates.** Checking a certificate happens after the server has
  read the ClientHello, on the client. `verify=False` and a CA file change
  which certificates BoringSSL accepts, not what it sends: the cipher list,
  extensions, the trust_anchors extension (which comes from the profile,
  not from the roots) and ALPN are the same. A test fetches from the
  fingerprint server with the check off and with the server's certificate
  as the only CA, and holds both handshakes to the profile's recording.
- **Authorization.** Its place was recorded from Chrome 155 and Firefox
  158 on macOS: the capture server's `/auth` answers 401 until a request
  carries a login, then serves a page whose script loads an image, POSTs
  JSON and submits a form that redirects. The browsers were given the
  login in the URL and sent it on everything after.
  - Chrome sends it straight after Host, Connection, Content-Length and
    Cache-Control, before anything else, in every kind of request: typed,
    form, script, image, and after a redirect. (That is where Chromium's
    `HttpNetworkTransaction::BuildRequestHeaders` adds the auth
    controller's header, before the request's own headers.) Over HTTP/2
    that makes it the first header after the pseudo-headers, unless a body
    or Cache-Control comes first.
  - Firefox sends it straight before Connection when it holds the login
    already. Its retry of the request a 401 answered has it last instead,
    appended to headers already built; Netweir always sends the login
    up front, so it uses the first place.

  Each profile's header lists gain an empty `Authorization` entry there,
  like `Cookie`'s, which only holds the place. A caller's own
  `Authorization` header (a bearer token) goes there too. The tests replay
  the recorded requests with a login and compare every header, in order.

## Interactions

- **Retries** keep the request's policy: the same timeout, redirect limit
  and login.
- **Redirects** in a crawl are queued hop by hop, as before; each hop
  carries the policy, counts toward its limit, and gets the login only
  where the rules above allow it.
- **The cache** keeps and serves responses by request fingerprint, which
  ignores policy. A request that doesn't follow redirects, or has a lower
  limit than the crawl's, bypasses the cache, since a kept answer may have
  followed redirects it wouldn't. Pages fetched with a login are kept like
  any other; the cache is a development tool on your own disk.
- **Deltafetch and the duplicate filter** use the fingerprint as before: a
  timeout or a login doesn't make a request different.
- **Proxies**: the login goes to the site, inside the tunnel; a proxy's
  own login stays in the proxy URL.
- **Robots and TDMRep** files are fetched with the crawl's `http_auth`
  where its domain matches, and the crawl's redirect limit and timeout.
- **Chrome**: a request fetched in Chrome (`browser=True`, or after a
  block) uses the crawl's timeout and Chrome's own redirects; its `auth`,
  `timeout` and redirect settings don't apply there. Known limit.
- **The archive** (WARC, HAR) records Authorization as it was sent, as it
  records cookies.

## Saved state

A Request's `timeout`, `max_redirects`, `allow_redirects` and `auth` are
saved with it in a checkpoint and in the shared queue, so a resumed or
leased request is sent as it was asked for. The checkpoint format goes
from 3 to 4 with a `policy` column (JSON); an older checkpoint is upgraded
with every request's policy empty, which is what they were sent with. A
queue member written before has no `policy` and reads as empty. Worker
processes hand Requests back pickled, fields and all.

Logins are therefore on disk in a checkpoint and in Redis in a queue, as
they are in Scrapy's JOBDIR. The crawl-wide `http_auth` isn't: it's
applied when a request is sent.

## Known limits

- Digest and other challenge schemes aren't supported: Basic is sent up
  front, and a 401 asking for Digest is returned as the page.
- Safari's place for Authorization isn't recorded: Safari can only be
  driven with no other Safari window open. Its profile sends it after its
  own headers until it is. The capture server's `/auth` page is ready for
  it.
- A CA file replaces the bundled roots rather than adding to them, as in
  requests; a crawl that needs both should put both in the file.
