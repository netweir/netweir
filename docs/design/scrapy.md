# Netweir as Scrapy's download handler

Some people won't move a working Scrapy project, and shouldn't have to
for the fetching alone. Scrapy lets a project replace how it downloads
(`DOWNLOAD_HANDLERS`); `netweir.scrapy.DownloadHandler` downloads with
Netweir's client, so the requests look like the profile's browser, and
leaves everything else to Scrapy.

## What Scrapy keeps

Scrapy's downloader middlewares do their jobs as with its own handler,
which is why Netweir's client gained three modes for this:

- **One hop at a time.** `Fetcher.send(..., follow=False)` returns a 3xx
  as the response instead of following it, so Scrapy's RedirectMiddleware
  decides. Scrapy then sends the next hop as a new request, with the
  Referer its referrer policy gives it, which would make every hop look
  like a link from the page that redirected. So the handler keeps the
  first request's method, kind, body and referer in `meta`, and sends a
  hop with `chain=` (the redirects so far, from `redirect_reasons` and
  `redirect_urls`): Netweir works the request out as a browser's
  redirect does (`Outgoing::after_redirects`), method, Sec-Fetch-Site and
  header order included. A request counts as a hop only when
  `redirect_urls` ends with the URL that answered with the 3xx, and its
  chain is only the redirects since its first request was sent: a new
  request that copied a redirected response's meta, or a meta refresh,
  starts afresh, as a browser would.
- **No cookie jar.** `Fetcher(cookies=False)`: Scrapy's CookiesMiddleware
  adds the Cookie header, and a second jar would send cookies twice.
- **No system proxy.** `Fetcher(system_proxy=False)`: Scrapy's
  HttpProxyMiddleware has already read the environment's proxies into
  `meta["proxy"]`, and decided against one when it's None; the client
  falling back to the system's would undo that.

Proxies come from `meta["proxy"]`. HttpProxyMiddleware moves a proxy's
login into a `Proxy-Authorization` header, encoded with
`HTTPPROXY_AUTH_ENCODING`; the handler reads it back with that encoding
and puts it into the proxy URL, percent-encoded, which Netweir sends as
UTF-8. A Proxy-Authorization that isn't Basic can't be moved, and is an
error rather than a request sent without it. Clients are kept per proxy,
timeout and size limit, the 64 used most recently.

## What the profile decides

User-Agent and Accept-Encoding always come from the profile: Scrapy's
would give the crawler away. Accept and Accept-Language come from the
profile unless the request has its own, other than Scrapy's
`DEFAULT_REQUEST_HEADERS`. Referer is handed to Netweir as the page the
request comes from, which sets `Sec-Fetch-Site` and `Origin` as the
browser would. A GET without a body is a navigation, unless it asks for
JSON (by Accept, or `X-Requested-With`), when it's a script's request; a
form encoding is a form submission; anything else is a script's request.
`meta["netweir_kind"]` overrides.

## Errors

A failed fetch becomes Scrapy's own exception, so its retry middleware and
errbacks see what they'd see from its own handler: `DownloadTimeoutError`,
`DownloadConnectionRefusedError`, `CannotResolveHostError` (told apart
from a refused connection by the client's message: a known limit),
`ResponseDataLossError` and `DownloadFailedError` are retried;
`DownloadCancelledError` for a body over the size limit and `NotSupported`
for a request that can't be sent aren't. It needs Scrapy 2.15 or later:
coroutine handlers arrived in 2.14, but 2.14's redirect middleware changes
a request's method on some redirects differently from browsers (a 302
after a PUT, a 301 after a POST), and a hop sent as the browser would
send it would then disagree with what Scrapy recorded. On 2.15 and later
they agree. The exceptions above arrived in 2.15 too; the fallbacks to
Python's own OSErrors are for anyone running an older Scrapy anyway.

`download_latency` is set on each response's request, which AutoThrottle
reads; without it, AutoThrottle does nothing.

## Not in this version

`DOWNLOAD_WARNSIZE`, `DOWNLOAD_FAIL_ON_DATALOSS` (a body cut short is
always an error), `bindaddress`, Scrapy's TLS settings
(`DOWNLOADER_CLIENT_TLS_*`: the profile decides TLS), and a response's
`ip_address` and `certificate`. A `NETWEIR_PROFILE` Netweir doesn't know
is logged when Scrapy starts the handler, and every request then fails
saying so: Scrapy carries on after a handler that can't start.
