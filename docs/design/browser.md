# netweir browser driver design

Status: draft, October 2026
Author: James Conway

## What it is for

Some pages only exist after JavaScript runs, and some sites won't serve
anything until a browser has passed their challenge. For those, netweir
drives a real Chrome. Everything else keeps going through the HTTP client,
which is a hundred times cheaper per page.

So the driver has two jobs. On its own, it's an async API for loading a
page, acting on it and reading it back, close to what Playwright users
expect. Inside a crawl, it's the step on the recovery ladder between
"slow down" and "give up": a request that keeps getting blocked is fetched
in Chrome, and the cookies Chrome earns go back to the HTTP session so the
next hundred pages don't need it.

## Scope

In this design:

- Launching Chrome (installed, or a path you give) and driving it over the
  Chrome DevTools Protocol (CDP), through a pipe rather than a port
- Pages and isolated browser contexts; navigation that reports the status
  and headers of the document; reading the page back as a netweir `Node`
- Clicking, typing, pressing keys and waiting for elements, with
  Playwright-style actionability checks and real input events
- Running JavaScript in the page, screenshots, cookies
- Stealth from the first commit: none of the CDP calls that pages can
  detect, no automation flags, no headless tells in the user agent
- Crawl integration: per-request browser fetches and browser escalation on
  the recovery ladder, with cookie hand-back (milestone 9)
- Chrome's remaining gaps: proxies that need a login, installing a
  pinned Chrome for Testing, and connecting to a browser that's already
  running (milestone 10)

Not in this design: tracing, PDF, Safari (no BiDi), Camoufox (a third
protocol), and Firefox for now (see below). Reading what a page fetches,
and blocking, changing or answering its requests, came later and has a
design of its own, [network.md](network.md).

## Architecture

A new crate, `netweir-browser`, holds the protocol client and the page
model. It has no Python in it, so the crawl engine and, later, the hosted
service can use it directly. `netweir-py` binds it, and the async methods
return Python awaitables the same way fetching does.

```
Python: Browser, Context, Page  (python/netweir/browser.py, thin)
          |
netweir-py: async bindings
          |
netweir-browser: Browser -> Context -> Page -> Frame
          |   protocol client: one connection, many sessions
          |
       Chrome  (--remote-debugging-pipe)
```

### The connection

Chrome is started with `--remote-debugging-pipe`. It reads commands on
file descriptor 3 and writes replies on 4, each message a JSON object
followed by a NUL byte. On Windows the same pipes are passed as inherited
handles with `--remote-debugging-io-pipes`. A pipe, unlike
`--remote-debugging-port`, opens no port: no other process on the machine
can take control of the browser, and no page can find it by probing
localhost.

One reader thread reads messages and routes them: a reply goes to the
command waiting for its `id`, an event to the session it names. Commands
are written by one writer thread fed from a channel, so an async task never
blocks on the pipe. Targets are attached in flat mode (`sessionId` on each
message) so one connection carries every page.

Commands are sent as a method name and JSON parameters. The driver uses
about forty CDP methods; typing each by hand is less code than a generator
for the 60,000 lines of the whole protocol, and a method that changes
breaks one test, not a build.

If Chrome exits or the pipe closes, every waiting command fails with an
error that says so, and the browser is marked closed.

### Stealth rules

These are rules of the driver, not options, and tests enforce them:

- `Runtime.enable`, `Console.enable` and `Log.enable` are never sent.
  Their side effects are visible to page scripts. The client refuses to
  send them.
- netweir's own helper code (finding elements, actionability checks,
  reading the page) runs in an isolated world made with
  `Page.createIsolatedWorld`. It shares the DOM but not the page's
  JavaScript globals, so the page can't see or patch it. The world name
  is not a recognisable one.
- `page.evaluate()` runs in the page's main world, because that's where
  your code expects the page's variables to be. It uses `Runtime.evaluate`
  with no context id, which needs no `Runtime.enable`.
- Clicks, typing and key presses are `Input.dispatch*` events at real
  coordinates, never `element.click()`.
- Chrome is launched without `--enable-automation`, and with
  `--disable-blink-features=AutomationControlled`, so
  `navigator.webdriver` is false.
- Headless Chrome puts `HeadlessChrome` in its user agent, though not in
  its client hints. Each page's user agent is overridden with the same
  string reading `Chrome`, together with the client-hint metadata Chrome
  itself reports. The override has to carry that metadata: without it,
  Chrome stops sending the high-entropy hints. Dedicated workers inherit
  the override; service workers don't, and take their user agent only
  from the `--user-agent` flag, which leaves the high-entropy hints empty
  wherever no override restores them. So headless Chrome is launched with
  the flag, and each page's override restores the hints. The exact user
  agent and metadata only come from asking Chrome without the flag, so
  the first launch of a given Chrome in a process starts it once to ask
  (reading the metadata from `chrome://version`, the one page that can
  see it before any override). In a service worker, the high-entropy
  hints stay empty: no override reaches it.

### Pages

`Browser.new_page()` opens a page in a new browser context, so pages share
no cookies or storage unless you ask for a shared one with
`Browser.new_context()`. Each page is a target with its own session, with
`Page`, `Network` and lifecycle events enabled; `Network.enable` is what
reports the document's status and headers, and isn't visible to the page.

`goto(url, wait="load")` navigates and waits for the lifecycle event
(`domcontentloaded`, `load` or `networkidle`) of that navigation, matched
by loader id. It returns a `Response` with the status, final URL and
headers of the main document. A network error (DNS, refused, TLS) raises
`netweir.FetchError` with Chrome's error text. An HTTP error status
doesn't raise; the status is in the response, as with `netweir.get()`.

`content()` returns the HTML as Chrome now has it, doctype included, and
`parse()` returns it as a netweir `Node`, so every selector, `find_all`
and tracked selector works on a rendered page as on a fetched one.

### Actions

`click(selector)`, `fill(selector, text)`, `press(key)` and
`wait_for(selector, state="visible")` take a CSS selector. Each action
retries until its checks pass or its timeout (30 seconds by default)
runs out, then raises `netweir.BrowserTimeout` naming the check that
failed:

| Check | Means | Used by |
|---|---|---|
| attached | the selector finds an element | all |
| visible | a non-empty box and not `visibility:hidden` | click, fill, wait_for |
| stable | the same box in two animation frames | click |
| enabled | not disabled, not in a disabled fieldset | click, fill |
| editable | enabled and not read-only | fill |
| receives events | the element, or one inside it, is what's at the click point | click |

A click scrolls the element into view, moves the mouse to its centre and
presses and releases the left button. `fill` focuses the element, selects
what's in it and inserts the text with `Input.insertText`, which fires the
same input events typing does. `press` sends a key down and up, with the
key's code and text for the common keys.

### Other page methods

- `evaluate(js)`: runs an expression or a function body in the main world,
  awaits it if it's a promise, and returns the result as JSON-compatible
  Python values. A thrown error raises `netweir.BrowserError` with the
  message.
- `screenshot(path=None, full_page=False)`: PNG bytes, also written to
  `path` if given.
- `cookies()` and `set_cookies(cookies)`: the context's cookies as dicts
  with `name`, `value`, `domain`, `path`, `expires`, `http_only`, `secure`
  and `same_site`.
- `url`, `title()`, `close()`.

### Finding Chrome

`netweir.browser(executable=None, headless=True, args=())` launches Chrome.
Without `executable`, it uses `$NETWEIR_CHROME`, then the usual install
paths on each platform (Chrome, then Chrome for Testing, then Chromium).
If none is found, the error lists where it looked. Each browser gets a
fresh temporary profile directory, removed on close.

## Python API

```python
import netweir

async with netweir.browser() as browser:
    page = await browser.new_page()
    response = await page.goto("https://quotes.toscrape.com/js/")
    assert response.status == 200
    await page.click("li.next a")
    await page.wait_for("div.quote")
    root = await page.parse()
    quotes = root.css("span.text::text").getall()
```

`Browser`, `Context` and `Page` are async context managers that close what
they own. Closing the browser closes every page.

## Crawl integration (milestone 9)

- `Request(..., browser=True)` and `page.follow(..., browser=True)` fetch
  that request in Chrome. The callback gets a netweir `Page` built from
  the rendered HTML, with `page.browser` the live browser page for
  actions until the callback returns; then it closes. Callbacks reached
  through a declarative spider's rules get the rendered page but no live
  one: the rules engine reads HTML on worker threads and lets the page go.
- `Settings.browser` is `"off"` (the default), `"on_block"` or
  `"always"`. With `"on_block"`, a request still blocked after its retries
  is fetched once in Chrome before `on_block` is called. A challenge page
  gets up to 20 seconds (or the request timeout, if shorter) to pass and
  navigate on. If Chrome gets through, its cookies are copied into a new
  HTTP session for that host, and following requests go back to the HTTP
  client. If it doesn't, or Chrome fails, `on_block` hears about the
  block as before. A page blocked in Chrome isn't retried there.
- `Settings.browser_pages` caps open pages (default 4). A browser request
  is scheduled like any other, so robots.txt, per-host limits and delays
  apply, and it counts against the host's concurrency while it runs.
- Chrome is started the first time a request needs it, through the
  crawl's `proxy` (its login answered as milestone 10 describes), and
  closed when the crawl ends.
- A rendered page's `Response` has `version` `"browser"`, the document's
  status and headers, and the HTML as UTF-8, with the content type saying
  so and the original `content-encoding` and `content-length` dropped.
- The stats count `browser_fetches` and `browser_unblocked`; a hand-back
  counts in `sessions_replaced`, and sending a blocked request to Chrome
  isn't counted as a retry.
- A browser request starts only once a Chrome page is free, so one waiting
  for a page holds no concurrency slot. The whole browser fetch has a
  limit (twice the request timeout plus the challenge wait), so a Chrome
  that stops answering can't hold the request forever.
- Chrome follows redirects itself; the URL it ends on counts as seen.
  With `obey_robots` or `obey_tdmrep`, each document the page's main
  frame is about to fetch (every redirect hop, and navigations its
  scripts start) is paused through the Fetch domain and checked against
  its site's robots.txt and TDMRep file, each fetched first through the
  scheduler's own gate if it isn't known; one they rule out is failed
  before it's sent, and the request is dropped for robots or TDM. A
  refused navigation leaves Chrome's error page showing, which is never
  taken for the page. The page Chrome lands on is judged for TDMRep by
  its own site. Frames within the page, and the files it loads, aren't
  checked; nor are a hop's site's request limits and delay, which apply
  to the request as it was scheduled.
- A Chrome that dies is started again for the next browser request; one
  that can't start is tried again a minute later, not for every request
  meanwhile. A page Chrome answers with a server error is retried there,
  and a 429 or a 503 with Retry-After slows the site down and is retried,
  like any other request, except a blocked request's one go in Chrome.
- A connected browser running with a `--user-agent` flag of its own
  reports emptied high-entropy hints; an override would send them as
  empty headers, so its pages are left as Chrome makes them.

Cookie hand-back only helps if the HTTP client looks like the same browser
the cookie was issued to, so the new session uses the HTTP profile that
matches the installed Chrome's major version, and the crawl logs one
warning when there is none.

## Chrome's gaps (milestone 10)

**Proxies with a login.** Chrome takes no credentials on
`--proxy-server`. It's given the proxy without them, and each page enables
the Fetch domain, answering `Fetch.authRequired` for a proxy challenge
with the credentials and leaving a site's own challenge to Chrome. Chrome
won't raise those challenges unless requests are paused too, so every
request a page makes is paused and released at once: a round trip each,
only when the proxy has a login. Page script can't see the Fetch domain.
Answers are sent from the reader thread without waiting for a reply, since
the request is held until they arrive. `netweir.browser(proxy=...)` takes
the proxy, and a crawl's browser requests use the crawl's `proxy`,
credentials and all.

Only HTTP(S) proxies: Chrome's SOCKS client offers no login, so a SOCKS
proxy with one is refused with an error. Only pages answer the
challenge, but Chrome keeps the login for the context once given, so a
page's workers and frames from other sites, which are targets of their
own, get through without being asked; a test checks a worker does.

**Installing Chrome.** `netweir install chrome` downloads the Chrome for
Testing build whose major version matches netweir's newest Chrome
profile, so a crawl's cookie hand-back always has a matching profile. The
build is listed in Google's `latest-versions-per-milestone-with-downloads`
JSON, downloaded with netweir's own HTTP client and unpacked into
`~/.netweir/chrome/<version>/` (`$NETWEIR_HOME` moves it). Finding Chrome
looks there after `$NETWEIR_CHROME` and before the system installs, newest
version first. `--headless-shell` installs `chrome-headless-shell`
instead, which is lighter but easier to tell from a real browser.

**Connecting to a running browser.** `netweir.browser(connect=url)` drives
a Chrome that's already running, or anything else that speaks CDP, over
a WebSocket: a `ws://` URL as Chrome prints it, an `http://host:port`
whose `/json/version` names one, or a `wss://` URL, over BoringSSL (the
TLS library the HTTP client uses) checked against Chrome's root store. Nothing is launched, so there's no
profile to remove; closing the Browser closes the pages it opened and
disconnects, leaving the browser running.

## Firefox, later

Firefox has dropped CDP; its remote protocol is WebDriver BiDi. Under
BiDi, Firefox reports `navigator.webdriver` as true, and no setting turns
that off: it follows whether the remote agent is running. Hiding it from
page script would mean overriding a browser property in JavaScript, which
detection looks for in its own right. So a Firefox driver would render
pages but give itself away to every bot check, and netweir leaves it out
until a patched build (such as Camoufox, which needs a third protocol) is
in scope.

## Testing

- A detection page served by the test server checks, from page script,
  what the stealth rules promise: `navigator.webdriver`, `Headless` in the
  user agent of the page and of a worker, and that the client hints,
  high-entropy ones included, are there and agree; the server checks the
  request headers too. The console trick that used to reveal
  `Runtime.enable` (a getter on a logged error's stack) no longer fires in
  Chrome 154, so that rule rests on the next test.
- The protocol client records every method it sends in tests; a test
  drives a full session (navigate, click, fill, evaluate, screenshot) and
  asserts none of the forbidden methods went out.
- Action tests use local pages that hide, cover, disable, move and delay
  elements, and check each action waits for the right thing and that the
  timeout names the check that failed.
- CI runs the browser tests on Linux, macOS and Windows with the runner's
  installed Chrome. Locally they're skipped, with a reason, if no Chrome
  is found.

## Milestones

**M8, the driver.** `netweir-browser` with launch, the pipe connection,
contexts, pages, navigation, content and parse, actions with actionability,
evaluate, screenshots and cookies; the Python API; the stealth tests; docs.

**M9, crawl integration.** Browser requests, escalation on the recovery
ladder, cookie hand-back, the page pool and its settings and stats.

**M10, Chrome's gaps.** Proxy logins, `netweir install chrome`, and
connecting to a running browser.
