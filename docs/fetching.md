# Fetching pages

`netweir.get()` and `netweir.Client` take the options you'd reach for in
requests or httpx, under the same names. None of them changes what the
site sees beyond what you ask for: the request still goes out as your
profile's browser sends it, TLS handshake and header order included.

```python
import netweir

page = netweir.get("https://example.com/search", params={"q": "rust", "page": 2})
page = netweir.get("https://intranet.example/report", auth=("ada", "secret"))
page = netweir.get("https://example.com/old", allow_redirects=False)
page = netweir.get("https://slow.example/", timeout=60)
page = netweir.get("https://test-box.local/", verify=False)
page = netweir.get("https://wiki.corp/", verify="/etc/ssl/corp-ca.pem")
```

## Query strings

`params` is a dict or a list of pairs. They go after the URL's own query,
URL-encoded as a form encodes them (a space becomes `+`); a list value
repeats its key, and a `None` value is left out, in a list too. A string
or bytes is taken as a query already encoded, and added as it is:

```python
netweir.get("https://example.com/list?sort=new", params={"tag": ["a", "b"], "page": None})
# sends https://example.com/list?sort=new&tag=a&tag=b
```

## Logins

`auth=(user, password)` sends HTTP Basic authentication, up front, with
the first request. It goes to the URL's own origin, and a redirect to
another site, or another port, leaves it behind for good: a later
redirect back doesn't pick it up again, as the Fetch standard has it.
Your own `Authorization` header goes the same way. A login written into
the URL (`https://ada:secret@intranet.example/`) does the same; `auth`
wins if you give both.

A login a site writes into a redirect's `Location` isn't sent, and is
taken out of the URL the request goes on to, so neither the page's URL
nor an archive shows it. A browser uses one only to answer a 401 asking
for it, which Netweir doesn't do. A script's request redirected to such
a URL on another origin than its page's fails, as `fetch()` does.

The `Authorization` header goes where the browser sends it. Chrome puts it
after `Host` and `Connection` and, when the request has them, after
`Content-Length` (a POST) and `Cache-Control` (a form, and the page a form
redirects to), before everything else; over HTTP/2, which has neither
`Host` nor `Connection`, that makes it the first header after the
pseudo-headers on a GET. Firefox puts it straight before `Connection`. A
bearer token of your own (`headers={"Authorization": "Bearer ..."}`) goes
in the same place, and wins over `auth`. Neither goes into a
[WARC or HAR file](crawling.md#keeping-what-you-fetched): an archive gets
shared, and a login is yours.

Only Basic is sent. A site that asks for Digest answers with a 401, which
you get as the page.

## Redirects

Redirects are followed, as the browser follows them, up to 20.
`allow_redirects=False` (or httpx's `follow_redirects=False`) gives you the
redirect itself:

```python
page = netweir.get("https://example.com/old", allow_redirects=False)
page.status               # 301
page.headers["location"]  # "/new"
```

Cookies it set are kept, as when following. `max_redirects=5` sets a
different limit; one more redirect than that raises
`FetchError(kind="too_many_redirects")`.

Chrome adds a redirect of its own. If an http:// URL's host has an HTTPS
DNS record, Chrome never sends the http request: it answers itself with
a 307 to the same URL on https://, marked `Non-Authoritative-Reason: DNS`.
A Chrome profile does the same, as long as `http3` is on and there's no
proxy, since the record lookup comes with HTTP/3. Firefox and Safari
profiles stay on http://, as Firefox does without DNS over HTTPS.

## Timeouts

`timeout` is in seconds, for each request from connecting to its last
byte; each redirect is a request of its own. A Client's `timeout` is the
default for its calls, and a call's own replaces it, shorter or longer.
Running out raises `FetchError(kind="timeout")`.

## Certificates

Certificates are checked against the roots Chrome trusts. For a host with
a self-signed certificate, `verify=False` skips the check; for hosts your
own CA vouches for, `verify="/path/to/ca.pem"` trusts the certificates in
that PEM file instead of the bundled roots, as requests does with a path.
Put the public roots in the file too if the same client also fetches
public sites. Pages fetched in Chrome (`browser=True`) use Chrome's own
certificate checks, not `verify`.

Checking happens on your side, after the server has read the handshake,
so neither changes what a site sees: Netweir's tests hold both handshakes
to the browser's recorded one.

## HTTP/3

A site that offers HTTP/3 says so in an `Alt-Svc` header, and browsers
take it up: the first request goes over TCP, the ones after it over
QUIC. Netweir does the same with a Firefox profile, through the QUIC
stack Firefox itself runs, and with a Chrome one, through a fork of
Cloudflare's quiche that sends Chrome's first packets. A Safari profile
uses that fork too, set up to send what Safari was recorded sending.
Safari's QUIC is Apple's own and its code isn't public, so that's the
closest match Netweir can make, not Safari's code.

Each browser moves its own way, and so does Netweir. Firefox proves the
alternative with a connection first, then gives that connection 100 ms
before a request goes over TCP instead; if it fails, the site stays on
TCP. Chrome starts the QUIC and TCP connections together from the next
request on and takes whichever is ready first, and from the first request
when the site's HTTPS DNS record offers HTTP/3; if QUIC fails, it's left
alone for 10 seconds, then longer each time, up to half an hour. Safari
stays on the TCP connection it has open and moves only when it opens a
new one, or straight away when the site's HTTPS DNS record offers
HTTP/3. It gives QUIC 250 ms before it starts TCP too, and tries QUIC
again on each new connection.

```python
async with netweir.Client(profile="firefox") as client:
    await client.get("https://cloudflare.com/")  # TCP, learns Alt-Svc
    await client.get("https://cloudflare.com/")  # HTTP/3, once it's up
```

Since it's the next request that moves, HTTP/3 shows up on a Client or a
crawl, which remember what a site offered, more than in one
`netweir.get()`. Chrome and Firefox profiles older than the newest stay
on HTTP/2 and HTTP/1.1. A `proxy`, or a system proxy for https, keeps
every request on TCP, as no browser sends QUIC through one. So does `http3=False`, on `netweir.get()`, a
Client, or in [settings](settings.md#fetching).

On a connection that resumes, Firefox and Chrome profiles send a GET in
0-RTT, before the handshake is done, as both browsers do; a POST waits.
Not everything is the browser's yet. Safari's 0-RTT isn't recorded, so a
Safari profile waits for the handshake, and Netweir tries only the first
address a name resolves to. With Chrome and
Safari, what happens after the first round trips (congestion control,
pacing, ACK timing) is quiche's, not the browser's. Safari's first
packets match its recording field for field, but where that recording
couldn't settle something, such as when Safari gives up on an idle
connection, Netweir has had to choose. [design/http3.md](design/http3.md)
lists each difference, for
[Firefox](design/http3.md#what-doesnt-match-yet),
[Chrome](design/http3.md#what-isnt-chromes) and
[Safari](design/http3.md#what-isnt-safaris).

HTTP/3 comes in the wheels `pip install netweir` installs. A build from
the source distribution leaves it out unless it's asked for
([CONTRIBUTING.md](https://github.com/netweir/netweir/blob/main/CONTRIBUTING.md)
says how), and then `http3` changes nothing.

## On a Client

A Client takes `verify`, `max_redirects`, `auth` and `http3` for every call, and
each call takes `params`, `auth`, `allow_redirects`, `max_redirects` and
`timeout` for itself:

```python
async with netweir.Client(auth=("ada", "secret"), timeout=10) as client:
    page = await client.get("https://intranet.example/", params={"q": "q3"})
    slow = await client.get("https://intranet.example/export", timeout=120)
```

In a crawl, the same options are fields of a [Request](crawling.md#requests)
and [settings](settings.md#fetching).
