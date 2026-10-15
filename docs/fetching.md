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
repeats its key, and a `None` value is left out:

```python
netweir.get("https://example.com/list?sort=new", params={"tag": ["a", "b"], "page": None})
# sends https://example.com/list?sort=new&tag=a&tag=b
```

## Logins

`auth=(user, password)` sends HTTP Basic authentication, up front, with
the first request. It goes to the URL's own origin, and a redirect to
another site, or another port, leaves it behind. A login written into
the URL (`https://ada:secret@intranet.example/`) does the same; `auth`
wins if you give both.

The `Authorization` header goes where the browser sends it: Chrome puts it
straight after `Connection`, Firefox straight before. A bearer token of
your own (`headers={"Authorization": "Bearer ..."}`) goes in the same
place, and wins over `auth`.

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
public sites.

Checking happens on your side, after the server has read the handshake,
so neither changes what a site sees: Netweir's tests hold both handshakes
to the browser's recorded one.

## On a Client

A Client takes `verify`, `max_redirects` and `auth` for every call, and
each call takes `params`, `auth`, `allow_redirects`, `max_redirects` and
`timeout` for itself:

```python
async with netweir.Client(auth=("ada", "secret"), timeout=10) as client:
    page = await client.get("https://intranet.example/", params={"q": "q3"})
    slow = await client.get("https://intranet.example/export", timeout=120)
```

In a crawl, the same options are fields of a [Request](crawling.md#requests)
and [settings](settings.md#fetching).
