# Sessions and cookies

Netweir keeps cookies the way the browser it looks like keeps them, and
lets you at them: read them, set them, save them to a file and load them
next week, hand them from Chrome to the HTTP client and back, and run
several logged-in users through one crawl.

## One shape for a cookie

Everywhere in Netweir a cookie is a dict, the one `page.cookies()` gives
in Chrome:

```python
{"name": "sid", "value": "...", "domain": ".shop.example", "path": "/",
 "expires": 1767225600.0, "http_only": True, "secure": True,
 "same_site": "Lax", "partition_key": None}
```

`domain` works as in the browsers: `".shop.example"` is sent to
shop.example and every subdomain, `"shop.example"` to that host alone.
`expires` is seconds since the epoch, or -1 for a cookie that lasts the
session. Playwright's spelling (`httpOnly`, `sameSite`, `partitionKey`)
is read too, so cookies from a Playwright context go straight in.

One difference from Playwright: its `add_cookies` reads `"domain":
"shop.example"` as the domain and its subdomains. Netweir keeps that
cookie the host's alone, in `set_cookies` and `cookies.add` alike, as
the dot says in the cookies Chrome itself reports. Write
`".shop.example"` for the domain-wide one.

## A client's cookies

```python
async with netweir.Client() as client:
    await client.post("https://shop.example/login", form={"user": "ada", "pw": "..."})

    client.cookies.get("sid")                      # the value, or None
    client.cookies.get("sid", domain="shop.example")
    client.cookies.set("currency", "EUR", domain="shop.example")
    client.cookies.delete("tracker")
    client.cookies.clear("ads.example")            # a domain and its subdomains
    client.cookies.all()                           # every cookie, oldest first
    client.cookies.add(cookies)                    # dicts from anywhere
    len(client.cookies)

    client.storage_state("state.json")             # save the login
```

Next time, start where you left off:

```python
async with netweir.Client(storage_state="state.json") as client:
    page = await client.get("https://shop.example/orders")
```

`storage_state` takes the path or the dict itself. The file is
Playwright's storage state format, so one Playwright saved loads here, and
the other way round. It's written readable by you alone (permissions
0600), since anyone holding it is logged in as you. On Windows the file
takes its folder's permissions instead, so keep it in a folder only you
can open.

A cookie Chrome would refuse is refused everywhere: a `__Host-` or
`__Secure-` name on a cookie that isn't secure, an `__Http-` or
`__Host-Http-` one that isn't secure and `http_only`, `SameSite=None` or a
partition key without `secure`, a tab or other control character in a
name or value, a cookie without a name whose value has an `=` in it.
Chrome turns down a whole batch of cookies for one of those, so letting
it into the jar would break every page it went to. `cookies.add` and
`cookies.set` raise `ValueError` and add nothing, and so does a request
whose `cookies=` has one. A storage state loads the rest and logs a
warning for each cookie it leaves out.

## Log in with Chrome, crawl over HTTP

Some logins need a real browser: a script that builds the form, a
challenge, a second factor you type in. Do that part in Chrome and the
rest over HTTP, which is many times faster:

```python
async with netweir.browser() as browser:
    page = await browser.new_page()
    await page.goto("https://shop.example/login")
    await page.fill("#user", "ada")
    await page.fill("#password", "...")
    await page.click("button[type=submit]")
    await page.wait_for("#account")
    cookies = await page.cookies()

async with netweir.Client() as client:
    client.cookies.add(cookies)
    orders = await client.get("https://shop.example/orders")
```

Some sites tie a login to the browser it was made in. The client looks
like Chrome down to its TLS handshake when you give it the profile of the
Chrome you logged in with (`netweir.Client(profile="chrome")` is the
newest), which is as close as a site can check.

The other way, a session made over HTTP opened in Chrome:

```python
context = await browser.new_context()
await context.set_cookies(client.cookies.all())
```

## Storage state in Chrome

A context saves its cookies and the localStorage of every site its pages
loaded, as Playwright's `storage_state` does:

```python
state = await context.storage_state(path="state.json")
context = await browser.new_context(storage_state="state.json")
state = await page.storage_state()   # a page opened with browser.new_page()
```

Reading or filling localStorage sends nothing to the sites: Netweir opens
a page of its own for each site, and answers its requests inside Chrome
with an empty page. sessionStorage and IndexedDB aren't saved, as in
Playwright.

## Cookies in a crawl

A request can bring cookies, as in Scrapy:

```python
yield netweir.Request(url, cookies={"currency": "EUR"})
yield netweir.Request(url, cookies=[{"name": "sid", "value": sid, "domain": ".shop.example"}])
```

They go into the crawl's jar just before the request is sent, as if the
page had set them, so later requests to the site carry them too. A cookie
without a domain is the URL's host's alone; without a path, its path is
the URL's directory. That's the URL you asked for: a redirect to another
host doesn't take them along.

One difference from Scrapy: Scrapy reads `"domain": "shop.example"` as
the domain and its subdomains. Netweir follows the browsers, so write
`".shop.example"` for that.

To start a crawl logged in, put the saved cookies on the first request:

```python
import json

async def start(self):
    state = json.load(open("state.json"))
    yield netweir.Request("https://shop.example/orders", cookies=state["cookies"])
```

### Several users in one crawl

`meta["cookiejar"]` names a jar. Requests naming the same jar share
cookies; no others see them. Pass it on to the requests a page leads to,
as in Scrapy:

```python
async def start(self):
    for user in ("ada", "bob"):
        yield netweir.Request(
            "https://shop.example/login", method="POST",
            form={"user": user, "pw": PASSWORDS[user]},
            meta={"cookiejar": user}, callback=self.logged_in,
        )

def logged_in(self, page):
    yield page.follow("/orders", meta={"cookiejar": page.meta["cookiejar"]})
```

Any JSON value names a jar, and `1` and `"1"` are two jars, as in Scrapy.

Each jar's requests are their own to the duplicate filter, the cache and
deltafetch. Bob's request for a page Ada already fetched still goes out,
and never comes back with Ada's page from the cache. Requests without a
jar keep the keys they always had, so an existing cache or checkpoint
stays valid.

A named jar is yours: a block never empties it. A blocked site still gets
a new session (another proxy, and a fresh jar for requests without a
named one), and requests in a named jar go out through whatever proxy the
rotation picks. If a site ties a login to an address, give the jar's
requests a proxy of their own with `Request(proxy=...)`.

A browser request opens its Chrome page with its jar's cookies, a named
jar's or the crawl's own (or, after a block, the site's new session's),
with the request's `cookies=` added, and when the page closes, after the callback, what changed in
Chrome goes back into the jar: the cookies it set or changed, and the
removal of those it deleted, so a logout in Chrome is a logout. A cookie
Chrome left alone isn't put back, so one the jar got over HTTP while the
page was open keeps its newer value. Each such page has a context of its
own, even when the crawl's Chrome keeps a profile (`user_data_dir`), so
one jar's cookies never reach another page or the profile on disk. A
login done by hand in Chrome inside a callback carries on over HTTP:

```python
async def start(self):
    yield netweir.Request(LOGIN, browser=True, meta={"cookiejar": "me"}, callback=self.log_in)

async def log_in(self, page):
    await page.browser.fill("#user", "ada")
    await page.browser.click("button[type=submit]")
    await page.browser.wait_for("#account")
    await page.browser.close()         # the cookies go back to the jar now
    yield netweir.Request(ORDERS, meta={"cookiejar": "me"})
```

### A request on its own

`meta["dont_merge_cookies"] = True` leaves the jar out of a request: none
of the jar's cookies go with it, and nothing its response sets is kept.
Unlike Scrapy, which then ignores `cookies=`, Netweir sends the request's
own `cookies` and nothing else.

A request's own cookies aren't part of what makes it a duplicate, nor of
its cache key or its deltafetch key, as in Scrapy. So a request that
leaves the jar out and brings another user's cookies is, to those, the
same request as the crawl's own for that page: it's dropped as a
duplicate of it, skipped by deltafetch for it, and with the cache on,
may be answered with the page the crawl's jar fetched. For another user,
use a named jar, which keeps all three apart.

### Resuming, workers and several machines

- A crawl with a checkpoint saves its jars as it goes, the sessions a
  block gave a site among them, and a resumed crawl is still logged in.
  A site's session comes back with the profile and proxy it went out
  with, which clearance cookies are bound to; if that proxy is gone from
  the crawl's list, or the profile from this version of Netweir, the
  site starts as a new visitor instead, and a warning says so. Requests still waiting keep their own cookies. The
  checkpoint file is made readable by you alone, as it holds the cookies,
  and so is the cache's, which holds the pages they logged in to (on
  Windows each takes its folder's permissions).
- `netweir.files` and `netweir.images` fetch an item's files in the jar
  of the request whose page gave the item, each URL once a crawl and
  stored as Scrapy stores it; with `per_jar=True`, each named jar gets
  and keeps its own copy (see
  [Downloading files and images](crawling.md#downloading-files-and-images)).
- With `workers`, every jar stays in the main process, where requests are
  sent, so callbacks in any worker see the same session.
- In a crawl shared by several machines, a request's `cookies` and its
  jar's name travel with it, but each process keeps its own jars: a
  cookie a response sets reaches the jar of the process that fetched it,
  not the others. Put a login's cookies on the requests that need them.

## What goes on the wire

Cookies go out as the profile's browser sends them, so they give nothing
away: the Cookie header in the place each browser puts it, longest path
first and then oldest first (Safari: newest first), one header over
HTTP/1.1 and one field per cookie over HTTP/2, as Chrome sends them.
SameSite is honoured as Chrome honours it, so a link from another site
carries no `Strict` cookies, and partitioned cookies stay with the site
they were set under. A Cookie header you set yourself goes as you wrote
it, and the jar adds nothing to that request.

Cookie values never appear in Netweir's logs, errors or the repr of a
request.
