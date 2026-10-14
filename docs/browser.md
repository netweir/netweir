# Browser

Some pages are built by JavaScript after they load, and some sites won't
show anything until a real browser has passed their checks. For those,
Netweir drives Chrome. It's an async API:

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

`page.parse()` gives a Netweir `Node`, so everything in
[Selecting](selecting.md) works on the rendered page.

## Starting Chrome

`netweir.browser()` starts the Chrome at `$NETWEIR_CHROME`, or else the
newest one `netweir install chrome` installed, or else the first of Google
Chrome, Chrome for Testing and Chromium in their usual places. An
installed one comes first even when the system's Chrome is newer, so
after updating Netweir, run `netweir install chrome` again to move up to
the version its newest profile is for.

If you don't have Chrome, or want the one Netweir is tuned for:

```
netweir install chrome
```

downloads the Chrome for Testing build whose version matches Netweir's
newest Chrome profile into `~/.netweir/chrome/` (`$NETWEIR_HOME` moves
it). Chrome for Testing is Chrome without the auto-updates, so a crawl's
cookie hand-back always has a profile that matches. It names itself
Chromium where Google Chrome adds its own name, so its pages present the
brands of the Google Chrome version Netweir has a profile for, as the HTTP
client does. `--headless-shell` installs `chrome-headless-shell` instead,
which is lighter and faster but easier to tell from a real browser; pass
its path as `executable` to use it. A crawl that obeys robots.txt or
TDMRep won't use it (see [In a crawl](#in-a-crawl)).

On Windows, the install also lets Chrome's sandbox read the folder, as
Chrome's own installer does: Chrome runs its network service and other
helpers in a sandbox that can only read folders granted to app packages,
and without that, pages fail to load with `ERR_ABORTED`. A Chrome
installed by an earlier Netweir gets the same access the first time it's
started, so there's no need to install it again. Netweir never changes
the access of a Chrome it didn't install.

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

`$NETWEIR_CHROME_ARGS` adds switches to every Chrome Netweir starts,
separated by spaces, on top of `args`. It's for what a machine needs
whatever the code says. On a macOS virtual machine, such as a cloud Mac or
a CI runner, the virtual GPU now and then hangs on Chrome's work and macOS
stops taking any more from it; Chrome then stops answering, and Netweir's
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
| `args` | `()` | more Chrome command-line switches; with your own `--user-agent=`, pages say that and keep Chrome's behaviour under the flag: high-entropy client hints empty, and Chrome's own brands even where Netweir would present Google Chrome's |
| `timeout` | `30.0` | the default limit, in seconds, for navigations and actions, and for Chrome to answer anything Netweir asks it |
| `proxy` | `None` | an `http://`, `https://` or `socks5://` proxy for everything Chrome fetches; for an HTTP(S) proxy, a username and password in it are given when it asks (Chrome can't log in to a SOCKS proxy) |
| `connect` | `None` | drive a browser that's already running, instead of starting one: a `ws://` DevTools URL, or `http://host:port` |
| `user_data_dir` | `None` | a profile folder to use and keep, so cookies and storage are still there next time |

Use it with `async with`, or `await` it and call `await browser.close()`
yourself. Each browser Netweir starts gets a fresh profile, deleted when it
closes. A program that ends without closing it, even one that crashes or
is killed, still takes Chrome with it within a few seconds. Chrome is
given the time to quit properly first, so a `user_data_dir` keeps
cookies it hadn't saved yet. The temporary profile stays behind, as a
`netweir-chrome-*` folder in the temporary directory, until the next
browser Netweir starts clears it away. A browser you `connect` to is
never ended this way: Netweir didn't start it.

To stay logged in from one run to the next, give Chrome a folder of your
own:

```python
async with netweir.browser(user_data_dir="~/.cache/shop-profile") as browser:
    page = await browser.new_page()
    await page.goto("https://example.com/account")
```

Netweir creates the folder if it isn't there and never deletes it. The
cookies, local storage and cache a page keeps are saved there when the
browser closes, and the next browser started with the same folder starts
with them. With `user_data_dir`, `new_page()` opens pages in the profile
itself, so they share its cookies with each other; `new_context()` still
gives you a context that's thrown away. Only one Chrome can use a profile
at a time, so close the first before starting another with the same
folder. Cookies that only last the session (no expiry date) aren't kept.
Netweir turns off Chrome's "Preload pages" setting (Settings >
Performance) in the folder before Chrome starts, and it stays off there:
it changes that one value in the `Preferences` file of the profile Chrome
opens and leaves the rest as it was (see
[Blocking and changing requests](#blocking-and-changing-requests) for
why). Point `user_data_dir` at your everyday Chrome folder and that
setting is off for your own browsing in that profile too; turn it back
on in Chrome's settings if you want it. A `--user-data-dir=` switch in
`args` (or `-user-data-dir=`, which Chrome reads the same) counts as
`user_data_dir`: that folder gets the same, and is kept. It's used as
given, as Chrome uses it: a relative path is relative to the working
directory, and `~` isn't expanded. A profile name, from
`--profile-directory=` or the folder's Local State, has to be one folder
inside it; anything else (`..`, a path) fails the launch.

With `connect`, Netweir drives a Chrome that's already running, started
with `--remote-debugging-port`, or anything else that speaks the DevTools
protocol. Pass the `ws://` URL Chrome prints, or `http://host:port` and
Netweir asks it for the URL; Chrome only answers that when `host` is an IP
address or `localhost`, so for one in Docker, say, use the `ws://` URL.
Nothing is launched, so `executable`, `args`, `proxy` and `headless`
don't apply, and Netweir can't turn that Chrome's "Preload pages" setting
off: turn it off yourself, or pages fetched ahead go past your routes. Closing the browser, or dropping it, closes the pages
Netweir opened and leaves the browser running. A `wss://` URL works too:
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
does. One failure is tried again before you see it: on Windows 11, Chrome
now and then can't get a local port for a connection and fails with
`net::ERR_NO_BUFFER_SPACE` before sending anything. `goto()` asks again,
up to twice, as a reload would, and raises only if that fails too.

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
| `hover(selector)` | moves the mouse over the element |
| `scroll(...)`, `select_option(...)`, `check(...)`, `uncheck(...)`, `set_input_files(...)` | see [Scrolling, forms and frames](#scrolling-forms-and-frames) |

Selectors are CSS. You don't need to wait before acting: `click` and
`fill` keep trying until the element can take the action, up to the
timeout. A click waits for the element to exist, be visible, stop moving,
be enabled and not be covered by anything else; `fill` waits for a
visible, enabled field that isn't read-only.

If the time runs out, `netweir.BrowserTimeout` (a `TimeoutError`) says
which of those never happened, such as "something else was always at its
centre, covering it". Each method takes `timeout=` in seconds.

## Scrolling, forms and frames

### Endless feeds

A feed that loads more as you scroll is the commonest reason to reach for
a browser at all. Most feeds fetch their items as JSON, so you can skip
the HTML and read that:

```python
page = await browser.new_page()
await page.goto("https://example.com/feed")

posts = []
while True:
    async with page.expect_response("**/api/posts?*") as info:
        loaded = await page.scroll(times=1)
    if not loaded:
        break  # the feed has run out
    posts += (await (await info.value).json())["posts"]
```

`scroll()` turns the mouse wheel, as a person would, down to where the
bottom of the page was, then waits up to `idle` seconds (2 by default) for
the page to grow. With `times=1` it does that once and returns 1 if more
loaded, or 0 if nothing did. Each batch the page fetched is the response
`expect_response` caught on the way.

If you only want the page with everything loaded, `await page.scroll()`
goes round until nothing more comes and returns how many times something
did; then read it with `page.parse()`. A feed that never ends never stops
growing, so give it `times=` to stop after that many rounds.

How long it may take:

- With `times=`, that's the limit, and it isn't cut short by the
  browser's `timeout`. Each step still has its own bound, so nothing
  hangs: each wait for more lasts at most `idle`, and every request to
  Chrome the browser's `timeout`.
- With `timeout=`, that's a limit on the whole call, whatever `times`
  says.
- With neither, the browser's `timeout` (30 seconds unless you changed
  it) is the limit on the whole call, so a feed still growing after that
  raises `netweir.BrowserTimeout`. Its `rounds` is how many rounds had
  loaded, and everything that loaded is still in the page, so you can
  catch it and read on, or read a long feed in rounds with `times=1` as
  above.

If the API is slow, raise `idle`; if the page swaps a "Load more" button
in instead, click it.

The wheel turns a notch (100 pixels) at a time, the way a mouse wheel
sends it, so a long way down is a long run of small turns. Boxes and
frames that scroll by themselves take the wheel before the page does, so
each turn goes where nothing like that is under the mouse, out to the
edges of the window if need be. One that fills the whole window and won't
pass its scroll on to the page stops the scroll there, as it would stop
your own mouse wheel.

`scroll()` does three other things too:

| Call | Does |
|---|---|
| `scroll("#reviews")` | wheels the element into view |
| `scroll(y=600)`, `scroll(x=-200)` | turns the wheel over the middle of the page, by that many pixels (negative is up or left) |
| `scroll("#feed", y=600)` | turns the wheel over the element, which scrolls it if it scrolls by itself |

Scrolling to the bottom moves the page as a whole. A feed that lives in a
box of its own (the page itself doesn't scroll) needs
`scroll("#feed", y=...)` in a loop instead.

### Forms

Here's a whole order form: a dropdown, a list where you can pick several,
a checkbox and a file.

```python
await page.goto("https://example.com/order")
await page.select_option("#size", "m")
await page.select_option("#extras", ["milk", "lemon"])
await page.select_option("#country", label="United Kingdom")
await page.check("#gift-wrap")
await page.set_input_files("#receipt", "receipt.pdf")
await page.click("button[type=submit]")
```

`select_option(selector, value)` picks the option with that `value`
attribute; `label=` picks by the text shown, and `index=` by position
from 0. Each takes one or a list, and a list needs a `<select multiple>`.
It returns the values selected. The page hears `input` and then `change`,
as when a person picks something, and hears nothing if the option was
already the one selected. It waits for the `<select>` to be visible and
enabled and for the option to be there, so options a script adds later
are fine; if the option never turns up, `netweir.BrowserTimeout` says
which one.

`check(selector)` and `uncheck(selector)` click a checkbox or radio
button only if it isn't already the way you want, then make sure the
click worked. You can point them at a `<label>` instead, which is handy
when a page hides the real checkbox behind a styled one. A radio button
can't be unchecked by clicking it, so `uncheck` on one raises
`netweir.BrowserError`; check another in its group.

`set_input_files(selector, files)` takes a path or a list of paths, and a
list needs `<input type=file multiple>`. A missing file raises
`FileNotFoundError` before anything is sent. An empty list clears the
input. With `connect`, the paths are read on the machine Chrome runs on.

`hover(selector)` moves the mouse over the element, for menus and
tooltips that only appear on `:hover` or `mouseover`.

### Frames

Selectors only look inside the document they run in, so an element in an
`<iframe>` can't be reached from the page. Get the frame and act on that:

```python
frame = await page.frame(name="checkout")
await frame.fill("#card-name", "Ada Lovelace")
await frame.click("button.pay")

for frame in await page.frames():
    print(frame.name, frame.url)
```

`page.frames()` lists every frame, the page's own first. `page.frame()`
finds the first with that `name`, a `url` matching a glob or compiled
regex, or both, and gives `None` if there isn't one. A frame has the
methods a page uses to read and act: `content`, `parse`, `title`,
`evaluate`, `click`, `fill`, `hover`, `scroll`, `select_option`, `check`,
`uncheck`, `set_input_files` and `wait_for`. Keys go wherever focus is,
so after `frame.fill(...)`, `page.press("Enter")` presses Enter in the
frame. Frames from other sites, which Chrome runs in processes of their
own, work the same way.

A frame's `name` and `url` are what they were when you asked for it.
Once the frame loads a different page, from this site or another (a
payment or sign-in frame often does), its actions work on the new one;
once the frame is removed they raise `netweir.BrowserError`.

A frame's requests count as the page's, whichever site it's from.
`on_request`, `on_response` and the `wait_for_`/`expect_` methods hear
them, `block()` and `route()` apply to them, and a response's `body()`
works the same. That includes a route you add after the frame has
loaded. So if a payment frame from another site fetches its JSON, you
can still wait for it:

```python
async with page.expect_response("**/api/quote*") as info:
    await frame.click("#refresh")
quote = await (await info.value).json()
```

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

## Reading what the page fetched

Plenty of pages that need JavaScript are a thin shell around an API. The
list you're after arrives as JSON, and the page's script turns it into
HTML. You can skip the HTML and take the JSON:

```python
page = await browser.new_page()
await page.goto("https://example.com/catalogue")
async with page.expect_response("**/api/products*") as info:
    await page.click("button.load-more")
response = await info.value
products = (await response.json())["products"]
```

`expect_response` starts waiting when the `async with` block starts, so
the response can't arrive before you're listening, however fast the
server is. `await info.value` is the first response that matched. If you
just want to wait, `await page.wait_for_response(pattern)` does the same
from the moment it's called.

A pattern is one of three things:

- A glob, matched against the whole URL: `*` is anything but a `/`, `**`
  is anything at all, `{png,jpg}` is either, and every other character,
  `?` included, is itself. So `"**/api/*"` matches
  `https://example.com/api/items?page=2`, and `"**/*.{png,jpg}"` matches
  every PNG and JPEG.
- A compiled regex, searched for in the URL: `re.compile(r"/api/v\d+/")`.
- A function that takes the response and returns true for the one you
  want: `lambda r: r.resource_type == "xhr" and r.status == 200`. It can be
  `async`; a slow one holds nothing else up, but among `async` functions
  the first to say yes wins, which may not be the earliest response.

A response has `url`, `status`, `headers`, `method` (the request's) and
`resource_type`: `"document"`, `"xhr"`, `"fetch"`, `"image"`,
`"stylesheet"`, `"script"`, `"font"`, `"media"` and so on. Its body is
fetched only when you ask: `await response.body()` for bytes,
`await response.text()` for text read as UTF-8, `await response.json()`
for the parsed JSON. They wait for the body to finish arriving. The
response `goto` returns works the same way.

Chrome keeps bodies only for the page that's showing. Once the page moves
on to another document, the bodies from the old one are gone, and asking
for one raises `netweir.BrowserError` saying so. Read what you need
before you navigate. A body Chrome couldn't decode, such as a broken
image, comes back empty.

To see everything rather than wait for one response, add a listener:

```python
def seen(response):
    print(response.status, response.resource_type, response.url)

page.on_response(seen)
await page.goto("https://example.com/")
page.remove_response_listener(seen)
```

A listener can be a plain function or an `async` one, and an exception in
it is logged to the `netweir` logger rather than raised. `on_request`,
`wait_for_request` and `expect_request` do the same for requests as they
go out; a request has `url`, `method`, `headers`, `resource_type` and
`post_data` (the body it sends, as bytes, or `None`).

Nothing piles up behind your back. Netweir only hears Chrome's network
events while a listener or a wait wants them, and keeps none of them:
a response nobody asked for is let go as it arrives.

`wait_for_response` and `expect_response` take `timeout=` in seconds, the
browser's `timeout` by default, and raise `netweir.BrowserTimeout` when
it runs out. If the page closes first, they raise `netweir.BrowserError`.

Use a page's listeners, waits and routes from one event loop at a time.
A page can outlive its loop (two `asyncio.run` calls, or a test framework
that gives each test a loop of its own). When the loop that reads the
page's events or routes ends, every listener and route on the page is
dropped, whichever loop added it, and the page carries on without them:
its requests go on unchanged. Add what you need again in the new loop.
A listener or wait added from another thread's loop does run on that
loop, but its routes would still be served by the first.

## Blocking and changing requests

The quickest win is not fetching what you'll never look at. Images, fonts
and video are most of a page's weight:

```python
page = await browser.new_page()
await page.block()  # images, fonts and media
await page.block(resource_types=(), urls=["**/analytics/**", "**/*.mp4"])
await page.goto("https://example.com/")
```

`block()` takes `resource_types` (`("image", "font", "media")` unless you
say otherwise) and `urls`, a list of globs. Each call adds to what the
page already blocks. A blocked request fails inside Chrome before it's
sent, so the server never sees it, and the page sees a network error as
it would for any failed request. Before a request is sent, Chrome can't
tell `fetch()` from `XMLHttpRequest`, so blocking `"xhr"` or `"fetch"`
blocks both.

For more than yes or no, add a route. Every request whose URL matches
the pattern goes to your handler, which decides what happens to it:

```python
async def handle(route):
    if route.request.resource_type == "xhr":
        await route.fulfill(json={"items": ["a test item"]})
    else:
        headers = dict(route.request.headers) | {"X-Debug": "1"}
        await route.continue_(headers=headers)

await page.route("**/api/**", handle)
```

The handler gets a `Route`, with the request as `route.request`, and calls
one of:

| Method | Does |
|---|---|
| `abort()` | fails the request; the page sees a network error |
| `continue_(url=None, method=None, headers=None, body=None)` | sends it on, changed as asked; `headers` replace all of the request's headers, so start from `route.request.headers` to change one |
| `fulfill(status=200, headers=None, body=None, json=None)` | answers it without it leaving Chrome; `body` is bytes or a str, and `json` is sent as JSON with an `application/json` content type |

Patterns work as they do for waiting, except that a function gets the
URL rather than a response. Routes are asked in turn, so a slow `async`
pattern function holds up the page's other routed requests. When more
than one route matches, the newest wins. `await page.unroute(pattern)` removes the routes with that pattern
(only the one with that handler, if you pass it as well).

A context has `route` and `unroute` too, and they apply to every page in
it, the ones open now and the ones you open later. A page's own routes
come first; if none of them matches, the context's get the request.

Requests are never left hanging. A handler that returns without deciding,
or raises (the exception goes to the `netweir` logger), lets the request
go on unchanged. One that hasn't decided by the browser's `timeout` (30
seconds unless you changed it) loses the request, which goes on unchanged
too; deciding after that raises `netweir.BrowserError`.

Routing has a cost: while a page has routes, every request it makes is
held in Chrome until Netweir has asked your Python code about it, which is
a round trip each. `block()` with resource types only holds requests of
those types, and decides them without asking Python.

A page can ask Chrome to fetch pages before anyone follows a link to
them, with speculation rules (`<script type="speculationrules">`) or
`<link rel=prerender>`. Chrome would do that fetching itself, where no
route sees it, so every Chrome Netweir starts has preloading turned off,
as you can turn it off in Chrome's settings; it fetches none of them, and
a page can't tell. Following such a link is then an ordinary navigation
that your routes see. `<link rel=prefetch>` is the page's own request,
routed like an image.

Some things routes don't see. In a crawl, a page that robots.txt or
TDMRep rules out is refused before any route is asked, and no route can
answer for it. Nor can a route send a page elsewhere to get round them:
`continue_(url=...)` on a page the crawl checked has the new URL checked
too, and one ruled out fails with `ERR_BLOCKED_BY_CLIENT`, exactly as a
refused page does, without being fetched. Requests from frames, other
sites' included, and from the page's workers are heard and routed like
the page's own. Known limit: in a browser you `connect` to with
preloading on, or in `chrome-headless-shell`, which ignores the setting,
what a page's speculation rules ask Chrome to fetch ahead goes out
unrouted. And
if you pass `--disable-features=NoStatePrefetchHoldback`,
`<link rel=prerender>` is fetched unrouted again. What service workers and shared workers fetch for
themselves isn't: Chrome runs those for the browser, not the page. Nor
is the script of a worker started by another worker, though what that
worker fetches afterwards is. A worklet's module (`audioWorklet`,
`CSS.paintWorklet`) is routed and blocked, but listeners don't hear it. A redirect's own response
isn't reported to listeners: you hear about the response the redirects
end at.

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
request is dropped, as over HTTP). Chrome fetches no page ahead of time
for the crawl, so a page's speculation rules can't get one past the
check either. `chrome-headless-shell` would, whatever its settings say,
so a crawl with `obey_robots` or `obey_tdmrep` on refuses to start it:
its browser requests fail with an error saying so. Use Chrome
(`netweir install chrome`, without `--headless-shell`), or turn both
off if you mean to crawl without them. The check stays on while your callback
drives `page.browser`: navigating it somewhere ruled out fails with
Chrome's `ERR_BLOCKED_BY_CLIENT`. `browser_pages` (4 by default) caps how many are
open at once. `browser_block=("image", "font", "media")` has every page
the crawl opens in Chrome skip those, as `page.block()` would.
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
  session switches to Netweir's profile for the installed Chrome's
  version; if Netweir has none, a warning says so. If Chrome can't be
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
Netweir avoids these giveaways:

- `navigator.webdriver` is false.
- Headless Chrome calls itself `HeadlessChrome` in its user agent. Netweir
  sends the normal user agent instead, with the client hints Chrome
  reports for itself, in the request headers, in the page, and in its
  workers and service workers.
- It never turns on the DevTools domains whose side effects page scripts
  can notice, and its own scripts run where the page can't see them.
- Clicks, keys, hovering and scrolling are real input events, not
  JavaScript calls.

A test in the repository loads a page that checks the first two from page
script and the server checks the request headers; the test also fails if
any of the DevTools domains in the third was turned on.

That isn't everything a site can check. `select_option`, and clearing a
file input, change the element from a script and send the `input` and
`change` events it would send; a page that checks `event.isTrusted` can
tell those apart from a person's. In a service worker, the
high-entropy client hints (`getHighEntropyValues`) come back empty, and
headless Chrome on a Linux server without a GPU reports a software renderer
to WebGL. Pass `headless=False` where that matters.
