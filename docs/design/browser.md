# Netweir browser driver design

Status: draft, October 2026
Author: James Conway

## What it is for

Some pages only exist after JavaScript runs, and some sites won't serve
anything until a browser has passed their challenge. For those, Netweir
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
  and headers of the document; reading the page back as a Netweir `Node`
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
- The rest of the actions Playwright users reach for: hovering,
  scrolling (endless feeds included), dropdowns, checkboxes, file
  uploads, frames, and a profile kept between runs (milestone 33)

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

### When the program goes first

Chrome is meant to quit when its end of the pipe closes, which happens
however the program driving it ends. It doesn't always: Google Chrome
155, headless, closes its pages and then runs on with nothing to drive
it, for good. Killed test runs left Chromes running for most of a day.
So the driver doesn't rely on it; each Chrome it launches is tied to a
lifeline the driver holds, and is ended when that's let go.

- **macOS and Linux.** Chrome starts in a process group of its own,
  which its helpers join. Right after it starts, the driver starts a
  watcher into the same group: `/bin/sh` reading a pipe whose only
  writer is the driver. Nothing is written, so the read returns only
  when the writer closes, on drop or on the program's death. The shell
  then sends Chrome SIGINT, as Ctrl+C in a terminal used to before Chrome
  had a group of its own, waits up to ten seconds for it to go, and runs
  `kill -9 0`, ending whatever is left of the group, itself included.
  SIGINT because Chrome writes out a profile's unsaved cookies on it,
  and on macOS doesn't on SIGTERM; a test kills a program with a cookie
  just set and finds it in the profile after. While the watcher lives,
  the group does, so neither its number nor Chrome's can have passed to
  anything else. The driver reaps the watcher once it's let go, on a
  thread of its own, so nothing waits on it. Linux's `PR_SET_PDEATHSIG`
  was passed over: it fires when the thread that started Chrome ends,
  not the program, and a runtime's threads come and go. Without
  `/bin/sh` (a distroless image), Chrome runs untied, and the first
  launch says so through the warning hook.
- **Windows.** Chrome starts suspended, goes into a job object that
  kills everything in it when its last handle closes, and is then
  resumed, so every helper it starts is in the job too. The driver holds
  the only handle. A Chrome that can't be put in a job runs untied, with
  a warning; one that can't be resumed is killed and the launch fails.

A Chrome the driver connected to is never tied: it isn't the driver's to
end. Known limit: a child forked without exec after the launch (Python's
`multiprocessing` with the `fork` start method, or `os.fork()`) holds a
copy of the lifeline, so Chrome lasts until that child ends too. A child
that execs another program (`subprocess`, the `spawn` start method)
doesn't: the lifeline is closed on exec.

A browser with a profile to keep that's dropped without `close()` is
asked to quit as `close()` asks (`Browser.close`), without the drop
waiting: a thread waits for Chrome, kills it after ten seconds, and only
then lets go of its lifeline. The command goes to the pipe's writer
thread, so it reaches Chrome even when the runtime has already gone.
A launch with the same profile meanwhile waits for that Chrome to have
gone (ten seconds at most), since a new Chrome that found it would hand
its window over and exit. A temporary profile still goes at once,
Chrome with it, as before.

Nothing is left running to remove the profile of a program that was
killed, so each launch sweeps the temporary directory, on a thread of
its own that holds up neither the launch nor the program's end. One
sweep runs at a time; a launch meanwhile has it go round once more.
Each profile the driver makes has a lock file beside it,
`netweir-chrome-<pid>-<n>-<nanos>.lock`, held for the browser's life: an
`flock` on macOS and Linux, and on Windows the file open sharing only
its deletion. The system lets go of either when the process ends,
however it ends. The lock is taken under another name (`.lock-new`) and
renamed into place before the folder is made, so a folder never exists
with its lock untaken. The sweep removes a folder whose lock it can
take, a `.lock-new` its program ended before naming, and a folder with
no lock at all (from before locks, or on a filesystem with no `flock`,
where the profile goes unlocked rather than the launch failing) once the
process named in it has gone. Process ids alone would be wrong where
containers share a temporary directory but not process ids, and a reused
id would keep a folder forever; the lock is right in both. Only names
the driver makes are looked at, links are never followed, and a profile
the caller gave is never named that way, so it's never touched. A launch
that fails removes the profile it made, at whichever step it fails.

### Stealth rules

These are rules of the driver, not options, and tests enforce them:

- `Runtime.enable`, `Console.enable` and `Log.enable` are never sent.
  Their side effects are visible to page scripts. The client refuses to
  send them.
- Netweir's own helper code (finding elements, actionability checks,
  reading the page) runs in an isolated world made with
  `Page.createIsolatedWorld`. It shares the DOM but not the page's
  JavaScript globals, so the page can't see or patch it. The world name
  is not a recognisable one.
- `page.evaluate()` runs in the page's main world, because that's where
  your code expects the page's variables to be. It uses `Runtime.evaluate`
  with no context id, which needs no `Runtime.enable`.
- Clicks, typing, key presses, hovering and scrolling are
  `Input.dispatch*` events at real coordinates, never `element.click()`
  or `scrollTo()`. The exceptions, and why, are under
  [Scrolling, forms and frames](#scrolling-forms-and-frames-milestone-33).
- Chrome is launched without `--enable-automation`, and with
  `--disable-blink-features=AutomationControlled`, so
  `navigator.webdriver` is false.
- Chrome starts with its "Preload pages" setting off in the profile and
  `--enable-features=NoStatePrefetchHoldback`, so it fetches nothing
  ahead outside the page's sessions; a person can have the setting off,
  and a page can't tell. Why, and the evidence, are in the network
  design, under "Pages fetched ahead".
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
`parse()` returns it as a Netweir `Node`, so every selector, `find_all`
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
presses and releases the left button. The button goes down only once the
page has sent `mouseover` for the element, the way `hover` waits (see
Hovering): the checks run in the element's own renderer, but Chrome's
browser process decides which frame gets the mouse, by what the page
around a frame from another site last drew, so a click could land on
that page instead and do nothing. (Windows CI once clicked a
`target=_top` link in such a frame and no navigation followed; 800
clicks in freshly loaded frames under load on macOS never missed, so
that one is unexplained, and the test now says what the link, the guard
and the server saw.) `fill` focuses the element, selects
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
fresh temporary profile directory, removed on close, unless it's given
one to keep (milestone 33).

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
  that request in Chrome. The callback gets a Netweir `Page` built from
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
proxy with one is refused with an error. Pages answer the challenge,
and so do frames from other sites, which have the Fetch domain on their
own sessions (milestone 34). Workers have no Fetch domain, but Chrome
keeps the login for the context once given, so they get through without
being asked; a test checks a worker does.

**Installing Chrome.** `netweir install chrome` downloads the Chrome for
Testing build whose major version matches Netweir's newest Chrome
profile, so a crawl's cookie hand-back always has a matching profile. The
build is listed in Google's `latest-versions-per-milestone-with-downloads`
JSON, downloaded with Netweir's own HTTP client and unpacked into
`~/.netweir/chrome/<version>/` (`$NETWEIR_HOME` moves it). Finding Chrome
looks there after `$NETWEIR_CHROME` and before the system installs, newest
version first. `--headless-shell` installs `chrome-headless-shell`
instead, which is lighter but easier to tell from a real browser.

**Chrome's sandbox on Windows.** Chrome runs its network service and
other helpers in an AppContainer, which can read an executable only if
its folder grants read and execute to ALL APPLICATION PACKAGES
(S-1-15-2-1) and ALL RESTRICTED APPLICATION PACKAGES (S-1-15-2-2).
Chrome's installer grants both; an unpacked Chrome for Testing has
neither. Then the network service can't start, and Chrome restarts it
again and again, aborting whatever navigation was under way
(`ERR_ABORTED`). Chrome's stderr, which a failed navigation's error now
carries, said so on a CI runner:

```
ERROR:sandbox\policy\win\sandbox_win.cc:804] Sandbox cannot access executable
C:\Users\runneradmin\.netweir\chrome\154.0.8037.92\chrome-win64\chrome.exe.
Check filesystem permissions are valid. ...: Access is denied. (0x5)
ERROR:content\browser\network_service_instance_impl.cc:653] Network service
crashed or was terminated, restarting service.
```

So `netweir install chrome` runs `icacls <version folder> /grant
*S-1-15-2-1:(OI)(CI)(RX) /grant *S-1-15-2-2:(OI)(CI)(RX)` (by SID, since
the group names are translated), fails the install if that fails, and
leaves a marker file once it's done. Launching a Chrome under Netweir's
own install folder without the marker grants it then, so an install
from before this works without reinstalling. A Chrome anywhere else is
never touched.

**Connecting to a running browser.** `netweir.browser(connect=url)` drives
a Chrome that's already running, or anything else that speaks CDP, over
a WebSocket: a `ws://` URL as Chrome prints it, an `http://host:port`
whose `/json/version` names one, or a `wss://` URL, over BoringSSL (the
TLS library the HTTP client uses) checked against Chrome's root store. Nothing is launched, so there's no
profile to remove; closing the Browser closes the pages it opened and
disconnects, leaving the browser running.

## Scrolling, forms and frames (milestone 33)

These follow the actions above: the same selectors, the same retries
until the checks pass or the timeout runs out, the same
`netweir.BrowserTimeout` naming what never happened. Every method below
is on a page and on a frame.

**Hovering.** `hover(selector)` waits for attached, visible, stable and
receives events (not enabled: a disabled button still shows a tooltip),
then sends one `mouseMoved` to the element's centre. Chrome moves its
own pointer there, so the page gets `mouseover`, `mouseenter` and
`:hover` exactly as it would from a mouse. It returns once the page has
sent `mouseover` for the element, heard by a listener netweir adds in
its isolated world, which page script can't see; `:hover` can be set
without one (Linux CI once had `hover` return with the page's handler
not yet run). A move to where the mouse already is sends nothing, as
when a scroll brought the element back under it, so a move that brings
no `mouseover` within a few frames is made again from just outside the
element, as a person moves off and back on, until the timeout. The page
sees at least one `mouseover`; Chrome may send one of its own when a
scroll puts the element under a still mouse.

**Scrolling.** `scroll(selector=None, *, x=0, y=None, times=None,
idle=2.0, timeout=None)` turns the mouse wheel: a `mouseMoved` to where
the wheel turns, then `Input.dispatchMouseEvent` with type `mouseWheel`
and the deltas, split into events of at most a notch (100 pixels, what
a mouse wheel sends on Windows and Linux) so no single event is a leap no
wheel makes. Chrome scrolls the way it scrolls for a real wheel or
trackpad, through the compositor, with its own smooth scrolling where
that's on, so the page sees `wheel` events followed by `scroll` events.
`scrollTo()` or `scrollIntoView()` would scroll too, but with no `wheel`
before the `scroll`, which is just what a script watching for bots looks
for, and some endless feeds listen for `wheel` alone. After each turn,
the isolated world waits for the scroll to start (up to ten animation
frames) and then to stop (three frames without a change), so the next
step reads where the page really is.

- With a selector and no distance, the element is wheeled into view.
  Each box that scrolls it is asked in turn, innermost first and the
  document last: the first that doesn't show all of the element (or its
  middle, for one too big to show whole) and can still scroll the way it
  needs is wheeled, over its visible middle (the document over a place
  where nothing else scrolls), by as far as brings the element's middle
  to that middle. That repeats until every box shows it. So an element
  at the end of a box that itself runs below the window gets the box
  scrolled to its end, then the page, rather than stopping with only
  its middle on screen. If a turn moves nothing (the scrolling box is
  itself out of view, or blocks the wheel), or nothing can scroll and
  it still isn't shown, it falls back to `scrollIntoView()`, as a click
  already does; a page can tell that one apart.
- With a distance, the wheel turns by `x` and `y` pixels, over the
  element if there's a selector (after bringing it into view), or over
  the page.
- Over the page means where a wheel scrolls the document: the middle of
  the viewport, unless what's there (or one of its ancestors) scrolls by
  itself, or is a frame, which would take the wheel first. Then it's the
  first point of a 7 by 7 grid, out to 2% from the edges, where nothing
  does. It's picked again for every turn, since scrolling brings new
  things under the mouse. A turn counts as moving something if the page,
  a box under the wheel, or a frame under it moved (a frame from the
  page's origin is read; one from another can't be, so it's assumed to
  have moved). A turn that moved nothing is tried at the next three
  places before the page is taken to have stopped, and 40 turns in a row
  that move something but not the page (a box or frame that won't pass
  the scroll on) stop it too.
- With neither, it scrolls to the bottom. Each round wheels down, in
  turns of whole notches about a viewport long (the last only as many as
  cover what's left), until the page is at or past where the bottom was
  when the round began, then polls the
  document's `scrollHeight` until it grows or `idle` runs out. No turn is
  a fraction of a notch: how far a notch scrolls is Chrome's to decide
  per platform, and on Windows it isn't the delta. When turns were cut
  to land exactly, Windows CI stopped 56 px short of the bottom with the
  box in the way never touched; whole notches, as a mouse wheel sends,
  don't depend on that arithmetic.
  The bottom is measured against the height the page scrolls in
  (`scrollingElement.clientHeight`, which leaves out a horizontal
  scrollbar). On Windows CI the last notch, with 8 px to go, moved
  nothing, though a script then took the page the rest of the way. So
  however a trip ends (a turn that moves nothing, the old bottom
  reached, the stall limit), a page left less than a notch short of as
  far as it really goes (`scrollHeight − clientHeight`, read then) is
  scrolled there by script, `scrollTo(x, scrollHeight)`, once per trip,
  after any scroll still running has finished: a page that loads more at
  its very bottom has to see the page get there. Each trip records what
  every turn did and how it ended, which the tests print when the page
  isn't at the bottom. It's the one place scrolling to the bottom
  scrolls by script, which a page can tell apart, as with click's
  fallback. A page that doesn't move for that either has stopped. Growth
  starts another round; none ends it. Stopping at the old bottom makes
  each round one trip, so `times=1` is exactly one
  batch from a feed that loads as you near the end, and the response
  that batch came in can be caught with `expect_response`. It returns
  how many rounds the page grew in. There's no other sign a feed has
  ended than nothing more arriving, so `idle` is a wait by design; it
  isn't used to wait for anything that has a signal.
- `timeout` is a deadline for the whole call, not each step: into view,
  a wheel turn, or scrolling to the bottom. Without one, the browser's
  timeout is the deadline, except when scrolling to the bottom with
  `times`, which is a limit of its own: then there's no overall
  deadline, and each step is bounded by itself (every CDP call by the
  browser's timeout, each wait for more by `idle`, the trip down by the
  stall limit and the end of the page). Scrolling to the bottom past its
  deadline raises `BrowserTimeout` with `rounds`, how many rounds had
  loaded.

Known limit: scrolling to the bottom moves the document. A page whose
document doesn't scroll, with its feed in a box of its own, needs
`scroll(box, y=...)` in a loop. Known limit: a box or frame that fills
the whole viewport and keeps its scroll to itself
(`overscroll-behavior: contain`, or a cross-origin frame at its end)
leaves no place for the wheel, and scrolling stops there. Known limit: a wheel turned over an
element that cancels `wheel` events moves nothing, and into-view falls
back as above.

**Dropdowns.** `select_option(selector, value=None, *, label=None,
index=None)` waits for a visible, enabled `<select>` that has the
options asked for (by `value`, by `label` or text, or by index; one or a
list), then sets `selected` on each option from the isolated world and
dispatches `input` (bubbling, composed) and then `change` (bubbling),
the order Chrome uses for a person's choice. Nothing is dispatched when
the selection doesn't change, as with a person picking what's already
picked. It returns the values selected. A disabled option, or one in a
disabled `<optgroup>`, is waited for as if missing, since nobody can
pick it; the timeout names it. Asking for more than one option of a
`<select>` without `multiple`, or using it on anything else, fails at
once.

Known limit: those events are dispatched by script, so
`event.isTrusted` is false. The trusted alternatives aren't portable: a
dropdown's popup is a separate widget that `Input` events can't reach,
and keyboard selection differs by platform (on macOS the arrow keys open
the popup instead of changing the value). Playwright and Puppeteer
dispatch these events the same way.

**Checkboxes.** `check(selector)` and `uncheck(selector)` read the state
first (an `<input type=checkbox>` or `radio`, a `<label>` standing in for
its control, or `role=checkbox`, `radio`, `switch` and the menu item
variants with `aria-checked`), and if it differs, wait for the control
(not the label) to be enabled, click with the full click checks, and
read it again. A click that didn't change it raises
`netweir.BrowserError`; so does unchecking a radio button, which a click
can't do. A box that never becomes visible is usually a hidden real one
behind a styled one, so that timeout says to check its `<label>`.

**File inputs.** `set_input_files(selector, files)` checks each path is
a file (a directory is refused as one) and makes it absolute (Chrome needs that), waits for an enabled
`<input type=file>`, takes it as a remote object from the isolated world
(`Runtime.evaluate` without `returnByValue`), and sends
`DOM.setFileInputFiles` with that object id. Chrome sets the files as
its file chooser would and fires trusted `input` and `change` events.
`DOM.setFileInputFiles` doesn't need `DOM.enable`, and nothing in it
reaches page script; `DOM.enable` is never sent. An empty list empties
the input from the isolated world (`value = ""`, then `input` and
`change`), since Chrome won't set no files; those two events aren't
trusted.

**Frames.** A `Frame` is a frame of a page, reached through a CDP
session, with its own isolated world. Every selector method above, and
`content`, `title` and `wait_for`, now runs in a frame; a page's own
methods run in its main frame.

- Frames from the page's own process are listed by `Page.getFrameTree`
  on the page's session. Frames from other sites run in processes of
  their own (out-of-process iframes), which the page's session can't
  reach. Each page sends `Target.setAutoAttach` with `flatten` and a
  filter of every kind but the browser and tabs, so each such frame, and
  each dedicated worker, gets a session of its own as it's created (any
  other kind, a worklet say, is let go and left); its session does the
  same for the frames and workers inside it. Since
  milestone 34 Chrome holds each one until its session has the page's
  network setup, then lets it go; the network design note says why that
  is invisible to the page, and what happens if setup stalls. Workers
  aren't frames: `frames()` asks only the frames' sessions for their
  trees.
- A frame that navigates to another site moves to a process of its own,
  and one that comes back moves back, so a `Frame` keeps its id, not its
  session. Each call finds the session now: one attached for that frame
  id, if there is one; else the one it was found in, if that's still
  attached; else the one the frame tree has it in. If Chrome says the
  frame isn't in its session, the tree is asked once more, and a frame
  that isn't there any more raises at once.
- Chrome never answers a command sent to a session that detaches before
  it's done, which a frame moving process does at any moment, so the
  connection fails a detached session's waiting commands at once rather
  than leaving them to time out. A frame that commits in the page's own
  process has any session it had of its own forgotten there and then.
- Each frame's isolated world is made with `Page.createIsolatedWorld`
  for that frame id, on the session that holds it, and kept by session
  and frame. The main frame's is still tied to its document's loader;
  another frame's is made again when Chrome says its context, or its
  session, is gone.
- `evaluate` in a frame has to run in the frame's main world, which
  `Runtime.evaluate` can only name for a session's own top frame
  without `Runtime.enable`. So the frame's `document` is taken as an
  object in the isolated world, `DOM.describeNode` gives its backend
  node id, `DOM.resolveNode` without a context resolves that in the
  main world, and `Runtime.callFunctionOn` runs the code with it. None
  of the three needs a domain enabled.
- Input always goes to the page's session at page coordinates, and
  Chrome routes it to whichever frame is under the point, its own or
  another process's. A point inside a frame is moved out through each
  `<iframe>` holding it: `DOM.getFrameOwner` on the parent's session
  gives the `<iframe>`, `DOM.resolveNode` puts it in the parent's
  isolated world, and a helper adds its content box's offset (borders
  and padding included), scrolls the parent if the point is out of its
  view, and, for a click or a hover, checks the `<iframe>` is what's at
  that point. Keys go to whatever is focused, so `fill` focuses the
  element inside the frame and sends `Input.insertText` to the page.

Known limit: an `<iframe>` with a CSS transform (scaled or rotated) puts
the point in the wrong place. Known limit: a frame's `name` and `url` are
those of when it was found. Requests from frames in other processes are
heard, blocked and routed through their own sessions since milestone 34
(see [network.md](network.md)).

**A profile to keep.** `user_data_dir` gives Chrome that folder as its
profile, made if missing and never removed, where each launch otherwise
gets a fresh one that's deleted. Cookies set in a context made with
`Target.createBrowserContext` are never written to the profile, so with
a profile to keep, `new_page()` opens its pages in the default context
instead; they share its cookies, which is the point. `new_context()` and
the crawl's guarded pages still make throwaway contexts. Chrome writes
the profile as it closes, so `close()` asks it to quit and waits, as it
always has. Known limit: two Chromes can't share a profile, so a second
launch with a folder that's in use fails.

## Firefox, later

Firefox has dropped CDP; its remote protocol is WebDriver BiDi. Under
BiDi, Firefox reports `navigator.webdriver` as true, and no setting turns
that off: it follows whether the remote agent is running. Hiding it from
page script would mean overriding a browser property in JavaScript, which
detection looks for in its own right. So a Firefox driver would render
pages but give itself away to every bot check, and Netweir leaves it out
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
- An endless feed served by the test server fetches each page of items
  as the list nears its end; the tests scroll it to the end, one round at
  a time and all at once, and read each batch's JSON with
  `expect_response`. A form test selects, checks and uploads a file,
  submits it, and the server checks the bytes it got. A page with an
  `<iframe>` from its own origin and one from `localhost` (another site,
  so another process) acts on both. A profile is launched twice and
  keeps a cookie.
- A test runs its own binary, with a temporary directory of its own, as
  a program that launches Chrome with a page open, kills it outright,
  and checks Chrome, its helpers and (on macOS and Linux) the watcher are
  gone within seconds, and that the next launch sweeps the profile it
  left. Another kills one with a cookie just set in a profile to keep,
  and finds the cookie there after, and one more does the same by
  dropping the browser. Launches that fail, before and after Chrome is
  first started, leave no profile. A third checks the sweep takes
  folders with a free lock and old ones of gone processes, and leaves
  folders in use, other names and links.
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

**M33, the rest of the actions.** Hover, scroll, select_option,
check and uncheck, set_input_files, frames (in-process and
out-of-process), and `user_data_dir`.

**M34, frames' and workers' requests.** Requests from frames from other
sites and from dedicated workers are heard, blocked and routed like the
page's own; see the network design note.
