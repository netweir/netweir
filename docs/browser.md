# Browser

Some pages are built by JavaScript after they load, and some sites won't
show anything until a real browser has passed their checks. For those,
netweir drives Chrome. It's an async API:

```python
import netweir

async with netweir.browser() as browser:
    page = await browser.new_page()
    response = await page.goto("https://quotes.toscrape.com/js/")
    await page.click("li.next a")
    await page.wait_for("div.quote")
    root = await page.parse()
    quotes = root.css("span.text::text").getall()
```

`page.parse()` gives a netweir `Node`, so everything in
[Selecting](selecting.md) works on the rendered page.

## Starting Chrome

`netweir.browser()` starts the Chrome at `$NETWEIR_CHROME`, or else the
newest one `netweir install chrome` installed, or else the first of Google
Chrome, Chrome for Testing and Chromium in their usual places. An
installed one comes first even when the system's Chrome is newer, so
after updating netweir, run `netweir install chrome` again to move up to
the version its newest profile is for.

If you don't have Chrome, or want the one netweir is tuned for:

```
netweir install chrome
```

downloads the Chrome for Testing build whose version matches netweir's
newest Chrome profile into `~/.netweir/chrome/` (`$NETWEIR_HOME` moves
it). Chrome for Testing is Chrome without the auto-updates, so a crawl's
cookie hand-back always has a profile that matches. It names itself
Chromium where Google Chrome adds its own name, so its pages present the
brands of the Google Chrome version netweir has a profile for, as the HTTP
client does. `--headless-shell` installs `chrome-headless-shell` instead,
which is lighter and faster but easier to tell from a real browser; pass
its path as `executable` to use it.

On Ubuntu 23.10 and later, Chrome for Testing may not start at all:
Ubuntu stops the user namespaces its sandbox needs unless an AppArmor
profile allows them, and only the Google Chrome package installs one. If
Chrome won't start, the error ends with what Chrome said, such as "No
usable sandbox". Allowing user namespaces keeps the sandbox on:

```
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

That lasts until the machine restarts. Passing `--no-sandbox` in `args`
also works, but leaves pages less contained than Chrome means them to be.

`$NETWEIR_CHROME_ARGS` adds switches to every Chrome netweir starts,
separated by spaces, on top of `args`. It's for what a machine needs
whatever the code says. On a macOS virtual machine, such as a cloud Mac or
a CI runner, the virtual GPU now and then hangs on Chrome's work and macOS
stops taking any more from it; Chrome then stops answering, and netweir's
error says it "didn't answer". Software rendering avoids the GPU:

```
export NETWEIR_CHROME_ARGS=--disable-gpu
```

A page can tell software rendering from a real GPU through WebGL, so leave
it off where the GPU works.

The options:

| Option | Default | Does |
|---|---|---|
| `executable` | `None` | the Chrome to start, instead of searching |
| `headless` | `True` | `False` opens a window |
| `args` | `()` | more Chrome command-line switches; with your own `--user-agent=`, pages say that and keep Chrome's behaviour under the flag: high-entropy client hints empty, and Chrome's own brands even where netweir would present Google Chrome's |
| `timeout` | `30.0` | the default limit, in seconds, for navigations and actions, and for Chrome to answer anything netweir asks it |
| `proxy` | `None` | an `http://`, `https://` or `socks5://` proxy for everything Chrome fetches; for an HTTP(S) proxy, a username and password in it are given when it asks (Chrome can't log in to a SOCKS proxy) |
| `connect` | `None` | drive a browser that's already running, instead of starting one: a `ws://` DevTools URL, or `http://host:port` |

Use it with `async with`, or `await` it and call `await browser.close()`
yourself. Each browser netweir starts gets a fresh profile, deleted when it
closes. A program that exits without closing it still ends Chrome, but
leaves the profile behind, as a `netweir-chrome-*` folder in the temporary
directory.

With `connect`, netweir drives a Chrome that's already running, started
with `--remote-debugging-port`, or anything else that speaks the DevTools
protocol. Pass the `ws://` URL Chrome prints, or `http://host:port` and
netweir asks it for the URL; Chrome only answers that when `host` is an IP
address or `localhost`, so for one in Docker, say, use the `ws://` URL.
Nothing is launched, so `executable`, `args`, `proxy` and `headless`
don't apply. Closing the browser, or dropping it, closes the pages
netweir opened and leaves the browser running. A `wss://` URL works too:
its certificate is checked against Chrome's own root store (not against
certificates installed on the machine, such as a company's).

## Pages and contexts

`browser.new_page()` opens a page with cookies and storage of its own, so
two pages never see each other's logins. For pages that should share them,
open a context:

```python
async with await browser.new_context() as context:
    first = await context.new_page()
    second = await context.new_page()
```

Closing a context closes its pages; closing the browser closes everything.

## Navigating

`await page.goto(url, wait="load")` loads the page and waits for its
`load` event; `wait="domcontentloaded"` returns sooner, and
`wait="networkidle"` waits until nothing has been requested for half a
second. It returns a `BrowserResponse` with the `status`, final `url` and
`headers` of the document. A 404 is returned like any page; a page that
couldn't be reached at all raises `netweir.FetchError`, as `netweir.get()`
does.

`page.url` is where the page is now, `await page.title()` its title and
`await page.content()` its HTML as Chrome currently has it.

When an action loads another page, `await page.wait_for_navigation()` waits
for it and returns its response. It waits for a navigation that starts
after it's called, so start it before the click:

```python
moved = asyncio.ensure_future(page.wait_for_navigation())
await page.click("a.next")
response = await moved
```

## Acting on the page

| Method | Does |
|---|---|
| `click(selector)` | clicks the middle of the element with the mouse |
| `fill(selector, text)` | replaces what's in a field with `text`, as typing would |
| `press(key)` | presses a key: a character, or `"Enter"`, `"Tab"`, `"Escape"`, `"Backspace"`, `"Delete"`, the arrows, `"Home"`, `"End"`, `"PageUp"`, `"PageDown"` |
| `wait_for(selector, state="visible")` | waits until the element is `"attached"`, `"visible"`, `"hidden"` or `"detached"` |

Selectors are CSS. You don't need to wait before acting: `click` and
`fill` keep trying until the element can take the action, up to the
timeout. A click waits for the element to exist, be visible, stop moving,
be enabled and not be covered by anything else; `fill` waits for a
visible, enabled field that isn't read-only.

If the time runs out, `netweir.BrowserTimeout` (a `TimeoutError`) says
which of those never happened, such as "something else was always at its
centre, covering it". Each method takes `timeout=` in seconds.

## Running JavaScript

```python
count = await page.evaluate("document.querySelectorAll('li').length")
data = await page.evaluate("() => fetch('/api/items').then(r => r.json())")
```

`evaluate` runs an expression, or a function, which it calls. A promise
is awaited, and the result comes back as Python values (dicts, lists,
strings, numbers, `True`, `False`, `None`). Values JSON can't hold come
back as text: `NaN`, `Infinity`, `-0` and BigInts as `"NaN"`,
`"Infinity"`, `"-0"` and their digits. An error thrown in the page raises
`netweir.BrowserError` with its message.

`evaluate` has no timeout of its own, so a promise that never settles
waits forever. Wrap it in `asyncio.wait_for` if that can happen.

## Screenshots and cookies

`await page.screenshot("page.png", full_page=True)` saves a PNG and returns
its bytes; leave out the path to get only the bytes.

`await page.cookies()` returns the cookies of the page's context as dicts
with `name`, `value`, `domain`, `path`, `expires`, `http_only`, `secure`
and `same_site`. `await page.set_cookies([...])` takes the same dicts;
`name`, `value` and `domain` are required.

## In a crawl

A spider can send some requests through Chrome and the rest through the
much faster HTTP client:

```python
class Shop(netweir.Spider):
    async def start(self):
        yield netweir.Request("https://example.com/catalogue", browser=True)

    async def parse(self, page):
        await page.browser.click("button.load-more")
        await page.browser.wait_for("li.product:nth-child(40)")
        root = await page.browser.parse()
        for href in root.css("li.product a::attr(href)").getall():
            yield page.follow(href)
```

The page a browser request gives its callback is the one Chrome rendered,
so `page.css(...)` sees what the scripts built. `page.browser` is the live
page until the callback returns, then it closes. Browser requests wait their
turn like any other: robots.txt, each site's limits and its delay all
apply. Chrome follows redirects, and a page's own scripts, by itself; each
page it's about to load in the tab is checked against that site's
robots.txt and TDMRep first, and one they rule out isn't fetched (the
request is dropped, as over HTTP). The check stays on while your callback
drives `page.browser`: navigating it somewhere ruled out fails with
Chrome's `ERR_BLOCKED_BY_CLIENT`. `browser_pages` (4 by default) caps how many are
open at once.
A declarative spider's rules read the rendered HTML, so callbacks the rules
call, and a rules spider's own `parse`, get the rendered page but not
`page.browser`.

A page Chrome answers with a server error (500, 502, 503, 504, 408, 522 or
524) is tried again in Chrome, up to `retries` times, as the HTTP client
would; other error statuses go to the callback as they are.

Two settings make Chrome step in on its own:

- `browser="on_block"`: a request still blocked after its retries is
  loaded once in Chrome, which waits up to 20 seconds for a challenge to
  pass. If it gets through, the site's HTTP session takes Chrome's
  cookies, so the next requests to that site go back to the HTTP client.
  Those cookies are often tied to the browser that earned them, so the
  session switches to netweir's profile for the installed Chrome's
  version; if netweir has none, a warning says so. If Chrome can't be
  started, the request goes to `on_block` as before, and a warning says
  why.
- `browser="always"`: every request goes through Chrome.

Chrome is started the first time the crawl needs it, and again if it
dies. If it can't start, browser requests fail, each saying why, and it's
tried again a minute later.

Chrome goes through the crawl's `proxy`, login included, so the cookies it
earns come from the address the HTTP client uses. It doesn't move through
`proxies` as a blocked HTTP session does: it and the session it hands its
cookies to both use `proxy`. With `proxy_rotation="per_request"` or
`"per_host"` there's no `proxy`, so Chrome takes the next healthy one of
`proxies` when it starts and keeps it for every page; the session it hands
its cookies to goes through that one too.

The stats count `browser_fetches`, pages Chrome fetched, and
`browser_unblocked`, blocked requests it got through. Each hand-back counts
in `sessions_replaced` too.

## Not looking automated

A browser that's being driven usually shows it, and bot protection looks.
netweir avoids these giveaways:

- `navigator.webdriver` is false.
- Headless Chrome calls itself `HeadlessChrome` in its user agent. netweir
  sends the normal user agent instead, with the client hints Chrome
  reports for itself, in the request headers, in the page, and in its
  workers and service workers.
- It never turns on the DevTools domains whose side effects page scripts
  can notice, and its own scripts run where the page can't see them.
- Clicks and keys are real input events, not JavaScript calls.

A test in the repository loads a page that checks the first two from page
script and the server checks the request headers; the test also fails if
any of the DevTools domains in the third was turned on.

That isn't everything a site can check. In a service worker, the
high-entropy client hints (`getHighEntropyValues`) come back empty, and
headless Chrome on a Linux server without a GPU reports a software renderer
to WebGL. Pass `headless=False` where that matters.
