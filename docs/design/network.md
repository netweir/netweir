# Netweir network capture and routing design

Status: implemented, October 2026
Author: James Conway

## What it is for

A lot of pages that need a browser are a shell around an API: the data
arrives as JSON from the page's own requests, and the script draws it.
Reading that JSON is easier and sturdier than reading the HTML it turns
into. So the first job is letting you wait for, and read, the responses a
page gets.

The second job is controlling what a page fetches. Images, fonts and
video are most of a page's weight and none of what a crawl wants, so you
can stop them before they leave Chrome. And for testing a spider, or
working round a page, a route can change a request on its way out or
answer it with something made up.

## Scope

In this design:

- Listening to a page's requests and responses: `on_request`,
  `on_response`, `wait_for_request`, `wait_for_response`, and
  `expect_request` / `expect_response` as async context managers
- A response's body, fetched only when asked for: `body()`, `text()`,
  `json()`, on any response, the one `goto` returns included
- Routes on a page or a context: abort, continue with changes, or fulfill
- `page.block()` for resource types and URL globs, decided without Python
- `Settings.browser_block`, applied to the pages a crawl opens in Chrome

Not in this design: routing a redirect's later hops by their own response,
intercepting responses (changing a body on its way back), HAR recording
of a page, WebSocket frames, and requests from service workers, workers
and out-of-process frames.

## How Chrome is asked

### Listening

Each page already has the `Network` domain on, since that's how `goto`
reads the document's status and headers. `Network.enable` changes nothing
a page can see. Its events already reach the page's handler on the reader
thread; the handler now does three more things with them:

- `Network.requestWillBeSent` records the request's method by id, until
  `Network.loadingFinished` or `Network.loadingFailed` says it's done. The
  record holds only requests still loading (at most 4,096; past that it
  starts again).
- Each request and each `Network.responseReceived` is sent to the page's
  listeners, if it has any, and dropped otherwise. Listeners are channels;
  one whose receiver has gone is removed at the next send.
- Responses keep their request id, method and resource type, so a body
  can be asked for later.

In Python, a page's `Network` object listens only while a listener or a
waiter wants events, through one channel, and stops listening when the
last one goes. Matching happens in Python: a glob through the same Rust
matcher routes use, a regex with `search`, a callable with what it's given.

`body()` waits until the request has finished loading, then asks
`Network.getResponseBody`. That's the one method this design adds to the
listening side. It reads Chrome's own buffer of what it received; it runs
nothing in the page and the page can't see it. Chrome clears that buffer
when the main frame commits a new document, and caps it, so a body asked
for after a navigation fails; Netweir turns Chrome's "No resource with
given identifier" into a `BrowserError` saying the body isn't available
any more, rather than letting anyone wait for it.

### Routing and blocking

Routing uses the Fetch domain, which the driver already uses for the
robots.txt and TDMRep guard and for proxy logins. `Fetch.enable` with
request-stage patterns pauses matching requests before they're sent;
each is answered with `Fetch.continueRequest`, `Fetch.failRequest` or,
new here, `Fetch.fulfillRequest`. None of the Fetch domain is visible to
page script.

What gets paused is worked out again whenever the page's rules change,
and sent as one `Fetch.enable` with the patterns needed and nothing more:

| Who needs it | Pattern |
|---|---|
| a proxy with a login | every request |
| a route on the page or its context | every request |
| `block(urls=...)` | every request |
| `block(resource_types=...)` | those types only |
| the guard | documents only |

With nothing needing it, the domain is turned off with `Fetch.disable`,
the other method this design adds. It's the Fetch domain's own off
switch, and like the rest of the domain it's invisible to the page.

Every paused request goes through the same steps, in this order:

1. A main-frame document goes to the guard first, if the page has one
   (a crawl's page with `obey_robots` or `obey_tdmrep`). One it refuses is
   failed there and goes no further.
2. The page's blocks, then its context's. A blocked request is failed with
   `BlockedByClient`, without asking Python.
3. The page's routes, then its context's. A request goes to Python as a
   `Route` only if one of them is routing.
4. Anything left goes on unchanged.

So the guard sees a document before any route or block, and a route never
sees, let alone answers, a document the guard refused.

A route could still point an allowed document somewhere else:
`Fetch.continueRequest` with a new `url` sends the request there without
pausing it again, so the guard would never hear of it. So a route for a
main-frame document of a guarded page carries the guard, and
`continue_(url=...)` with a different URL puts the new one to it first.
If it's refused, the request is failed with `BlockedByClient`, as the
guard fails one: the page sees `ERR_BLOCKED_BY_CLIENT`, and the crawl
records the refusal through the guard as it would any other.
`fulfill` needs no check, since nothing is fetched. A proxy's login is
answered from `Fetch.authRequired`, which doesn't pass through these
steps, so it keeps working with routes on.

Steps 2 to 4 run on the reader thread, or on the guard's task once the
guard has answered. Nothing there waits: a route is handed over through a
channel.

### Routes in Python

A page with routes hands every paused request to its `Network`, which
looks for the newest of the page's routes that matches the URL, then the
context's, and runs that handler as a task of its own, so a slow handler
doesn't hold up the others. No match: the request goes on.

A context's routes reach its pages through the context's own channel when
the page has no routes; when it has, the page's `Network` falls back on
the context's list itself. A route on a context changes the Fetch patterns
of every page open in it, and pages opened later start with them.

## The rules for slow and failed handlers

A request must never hang because of a handler. So:

- A handler that returns without deciding: the request goes on unchanged.
- A handler that raises: the exception is logged to the `netweir` logger,
  and the request goes on unchanged.
- A handler that hasn't decided within the browser's `timeout`: a timer
  lets the request go on unchanged. The timer holds the route weakly and
  ends as soon as the route is decided or dropped.
- The event loop a handler runs on ends (a page outlives an `asyncio.run`):
  the page's routes are dropped at once, without asking Chrome, so
  requests go on unchanged instead of waiting out the timer; when the
  loop reading events ends, the page's listeners and waits go too, and
  their channel is closed. Either way that's all of them, whichever loop
  added them. The next loop adds its own. A closed page ends its routes' channel the same way.
- A route dropped undecided, by Python or by Netweir (the handler task
  went away, routing stopped with requests still queued): it goes on
  unchanged when it's dropped.
- Deciding twice, or after the timer: `BrowserError`, saying the request
  was already decided.
- A decision Chrome rejects (a malformed header, say) raises, and the
  request is still waiting, so it goes on unchanged when the handler
  finishes.
- A decision on a page or browser that has closed is quietly nothing.

## Limits

- Known limit: before a request is sent, Chrome reports `fetch()` calls as
  `XHR`. Blocking `"fetch"` or `"xhr"` blocks both, and `route.request`
  says `"xhr"` for both. Responses, which come later, tell them apart.
- While a page has routes, every request it makes takes a round trip to
  Python. Blocking by resource type alone doesn't.
- A redirect's 3xx response isn't reported to listeners; only the response
  it ends at is. Each hop is still paused, and routed, as a request.
- Requests made by service workers, dedicated workers and frames from
  another site run in targets of their own, which the page's session
  doesn't cover. They aren't heard or routed.
- Bodies are Chrome's to keep. Past its buffer's size, or after the page
  has navigated, a body can't be read. A body Chrome couldn't decode (a
  broken image, say) comes back empty.
- A page's listeners and routes are meant for one event loop at a time.
  Events are read, and routes served, on the loop that started reading;
  waits and listeners added from another loop are settled and called on
  their own loop, but when the reading loop ends, every listener, wait
  and route on the page is dropped, whichever loop added it, and the
  page carries on without them.
- Async route patterns are awaited in order, so a slow one holds up the
  page's other routed requests. Async wait predicates run on their own,
  so among them the first to say yes wins, not the earliest response.
- The method of a response whose request record was dropped (more than
  4,096 loading at once) reads as GET.

## Testing

- Rust tests against a local server: a listener hears a page's image and
  an XHR, reads the XHR's JSON, and gets `Error::Gone` for it after a
  navigation; blocked types and URL globs never reach the server; routes
  abort, add a header the server sees, and fulfill a fetch with JSON the
  page shows; an undecided route, dropped or held past the timeout, lets
  the page load; a context's route reaches pages opened before and after
  it; a document a guard refuses stays refused with a route that would
  fulfill everything, and one a route sends to a refused URL fails
  unfetched; a proxy's login works with routes on; closing a page ends its
  routes; a decided route's timer is gone well before the timeout; and
  none of this sends `Runtime.enable`, `Console.enable` or `Log.enable`.
- Python tests for the same through the Python API, plus globs, regexes
  and predicates; `expect_response` around a click; listeners removed;
  a raising handler and an undecided one; deciding twice; `browser_block`
  in a crawl, and a redirect robots.txt refuses staying refused with
  blocking on; a crawl callback's route sending a page where robots.txt
  refuses; a page used from two `asyncio.run` calls; an expectation never
  awaited; and a slow predicate not holding up listeners.
