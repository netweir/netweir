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
0600), since anyone holding it is logged in as you.

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

A named jar is yours: a block never empties it. A blocked site still gets
a new session (another proxy, and a fresh jar for requests without a
named one), and requests in a named jar go out through whatever proxy the
rotation picks. If a site ties a login to an address, give the jar's
requests a proxy of their own with `Request(proxy=...)`.

A browser request in a named jar opens its Chrome page with the jar's
cookies, and when the page closes, after the callback, Chrome's cookies
go back into the jar. So a login done by hand in Chrome inside a callback
carries on over HTTP:

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

### Resuming, workers and several machines

- A crawl with a checkpoint saves its jars as it goes, and a resumed crawl
  is still logged in. Requests still waiting keep their own cookies. The
  checkpoint file is made readable by you alone, as it holds the cookies.
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
