# Steps

A page often needs a nudge before it's worth reading. There's a cookie
banner to get rid of, a "Load more" button, a search box, a feed that only
grows while you scroll. You can do all of that in a callback through
`page.browser`, but then every spider repeats the same five lines, and a
rules spider can't do it at all.

Steps are those nudges written as data. Give a request a list of them and
Chrome does them, in order, once the page has loaded. Your callback gets
the page as they left it:

```python
import netweir
from netweir import steps


class Shop(netweir.Spider):
    async def start(self):
        yield netweir.Request(
            "https://shop.example/catalogue",
            steps=[
                steps.click("#accept-cookies", timeout=3, on_error="continue"),
                steps.click("button.load-more"),
                steps.wait_for("li.product:nth-child(40)"),
            ],
        )

    def parse(self, page):
        for href in page.css("li.product a::attr(href)").getall():
            yield page.follow(href, callback="product")
```

A request with steps goes to Chrome; you don't need `browser=True` as
well. `page.css(...)` reads the HTML as it stood after the last step, and
`page.url` is wherever the steps ended up, which for a search form is the
results page.

Each function in `netweir.steps` returns a plain dict, such as
`{"action": "click", "selector": "button.load-more"}`. You can write the
dicts yourself, or keep them in a JSON file. Because a step list is plain
JSON, it goes wherever the request goes: into a checkpoint, through a
shared queue to another machine, back again on resume.

## The steps

Each step uses the page method of the same name, so it behaves the same
way, waits the same way and fails with the same messages. See
[Browser](browser.md#acting-on-the-page) for those. Selectors are CSS, or
XPath after `xpath=`: `steps.click("xpath=//button[text()='More']")`.

| Step | Does | Its `result` |
|---|---|---|
| `click(selector)` | clicks the element with the mouse (taps it on a phone) once it's visible, still, enabled and not covered | |
| `double_click(selector)` | double-clicks it the same way (taps twice on a phone) | |
| `fill(selector, text)` | replaces what's in a field with `text` in one go, the way pasting does | |
| `type(selector, text, delay=None)` | clicks the field and types `text` one key at a time, replacing what's there; see [Typing](#typing) | |
| `press(key)` | presses a key in whatever has focus: `"Enter"`, `"Tab"`, `"Escape"`, a character... | |
| `hover(selector)` | moves the mouse over the element | |
| `wait_for(selector, state="visible")` | waits until the element is `"attached"`, `"visible"`, `"hidden"` or `"detached"` | |
| `wait(seconds)` | waits, whatever the page does | |
| `wait_for_navigation(wait="load")` | waits for the page to move on from the document it showed before the previous step, and for the new one to reach `wait` | `{"url", "status"}` |
| `wait_for_response(url)` | waits for a response whose URL matches the glob `url`, counting any that arrived since the previous step began | `{"url", "status"}` |
| `scroll(...)` | turns the mouse wheel, as `page.scroll()` does: with no arguments, to the bottom over and over while more loads (`times=` caps the rounds, `idle=` is how long to wait for more, 2 s by default); `scroll("#reviews")` brings an element into view; `scroll(y=600)` turns by that many pixels | rounds that loaded more |
| `select_option(selector, value)` | picks options of a `<select>` by `value`, `label=` or `index=`, one or a list | the values selected |
| `check(selector)`, `uncheck(selector)` | ticks or unticks a checkbox or radio button, if it needs it | |
| `set_input_files(selector, files)` | puts files in an `<input type=file>`; the paths are read when the step runs | |
| `evaluate(js)` | runs JavaScript in the page | what it returned |
| `screenshot(full_page=False, quality=None)` | takes a PNG of the window, or of the whole page; with `quality` (1 to 100), a JPEG, often a tenth the size | the image's bytes |
| `answer_dialogs(accept=True, text=None)` | from here to the last step, answers every `alert()`, `confirm()` and `prompt()`: OK if `accept`, with `text` in a prompt's box, Cancel if not | |
| `call(method)` | runs a function of yours with the page; see [Your own code](#your-own-code) | what it returned |

A `wait_for_navigation` or `wait_for_response` belongs straight after the
step that sets it off. It looks back to the start of that step, so a
navigation or response that has already happened still counts. Each
`wait_for_response` takes the response it matched, so two of them for the
same URL wait for two responses:

```python
steps=[
    steps.type("#q", "green tea"),
    steps.press("Enter"),
    steps.wait_for_navigation(),
]
```

A step that moves the page on, such as a link it clicks or a form it
sends, leaves the next step a page that has finished loading.

The steps that find an element take `frame=`, to act inside an
`<iframe>`: `{"name": "consent"}`, `{"url": "**/consent/**"}` (a glob), or
both. A consent manager often puts its banner in a frame that turns up a
moment after the page, so a step waits for its frame within the step's
timeout:

```python
steps.click("button.accept-all", frame={"url": "**/cmp/**"}, timeout=5, on_error="continue")
```

## Typing

`type` goes about it as a person does. It clicks the field (taps it on a
phone) and waits a moment, as you'd wait to see the caret. A floating
label laid over the field takes the click and passes it on, as it does
for a person. If something else covers the field, so no click could
reach it, the field is focused from a script instead. If there's
something in the field, it selects it with Control-A, or Command-A when
the page's platform is a Mac, so the first key replaces it. A page that
swallows those keys (a masked phone-number field, say) has the field's
contents selected from a script instead, so the old text still goes. Then it
presses each key, holding each one down for a person's time (about
85 ms, more often longer than shorter). Capitals and symbols go with
Shift, with the key codes of a US keyboard. A newline is Enter and a tab
is Tab. A character that isn't on that keyboard, such as é, arrives as
text with no key, the way an OS's character picker sends it.

Between one key and the next there's a pause within `delay`: seconds, or
a `(least, most)` pair, 0.08 to 0.25 by default. Pauses come most often
near the middle of the range and now and then near its ends, the way
people's do, rather than spread evenly. `delay=0` leaves out the pause,
but each key is still held.

## Time limits

Every step has a time limit. `timeout=` sets it in seconds, a day at
most, and a step without one gets the browser's own, which is 30 seconds.
That includes `scroll` with `times=` and `evaluate`, which have no limit
when you call them yourself.

A step waiting for something that never comes uses all of its time limit.
For a banner that only some pages show, a 30-second wait on every other
page adds up fast, so give an optional step a short timeout.

`type` has time of its own for the keys. Its timeout covers getting to
the field, and the typing then gets as long as its text needs: each key
held its longest, plus the longest pause after it.

All of a page's steps together get at most `browser_steps_max`: 120
seconds by default, or `None` for no limit. Every page in Chrome holds
one of the crawl's `browser_pages` slots while its steps run. Without a
limit, one `wait(86400)` would hold a slot for a day, and a few of them
would stop every page that needs Chrome. For comparison, Zyte API gives
one wait 15 seconds at most and all of a request's actions 60. The limit
is checked twice:

- When a request (or `browser_steps`, or a rule) is written, steps that
  ask outright for more than the limit are refused. Asking outright means
  their waits, the timeouts they give and the time `type` needs for its
  keys. A request like that is skipped with an error, as a request with
  a bad URL is. Steps that take the browser's 30-second timeout don't
  count here, since a page's steps rarely use all of it.
- While the steps run, a step that reaches the limit fails, and so do
  the steps after it, whatever their `on_error`. The request fails with
  a `FetchError` of kind `"step"` that says `browser_steps_max` was
  reached.

The crawl allows a page in Chrome time for the page itself and then time
for its steps: the sum of their limits, never more than
`browser_steps_max`, plus time for a challenge they lead to.

## When a step fails

`on_error=` says what a failed step does to the steps after it:

- `"fail"`, the default, stops the steps and fails the request. Its
  errback gets a `netweir.FetchError` whose `kind` is `"step"`. The
  message names the step: "step 2 (click) failed: ...". `error.steps` has
  each step's outcome, so a `screenshot` step earlier in the list shows
  what the page looked like. The request isn't retried, since a step may
  already have done something, such as sending a form, that a retry would
  do twice.
- `"continue"` notes the error and goes on to the next step.
- `"stop"` skips the rest and hands the callback the page as it is.

There's no retry inside a step. Every step already waits up to its
timeout for the page to be ready.

## What each step did

`page.steps` has a dict for each step, in order:

```python
{"action": "evaluate", "status": "ok", "error": None, "elapsed": 0.012, "result": 40}
```

`status` is `"ok"`, `"failed"` or `"skipped"`. A skipped step comes after
one that stopped the steps, or belongs to a page the steps never ran on.
`elapsed` is in seconds. `result` is what the table above says, or None.
The list is made the first time you read it and kept, so reading it again
copies nothing.

```python
def parse(self, page):
    shown = page.steps[-1]["result"]  # an evaluate's value
    with open("catalogue.png", "wb") as f:
        f.write(page.steps[0]["result"])  # a screenshot's PNG
```

A screenshot's bytes stay in memory with the page until the callback is
done with it: a full-page PNG of a long page can be several megabytes.
When the page goes to a worker process (a rules spider's pages do), its
steps' results go with it, the images as base64, a third larger again.
`quality=` makes a JPEG instead, which is usually far smaller.

The stats count `browser_steps`, the steps that ran, and
`browser_steps_failed`, the ones among them that failed, whatever their
`on_error`.

## Blocks and challenges

The steps run on the page you asked for, not on a block. A page that's
blocked when it loads is waited on first, as any page in Chrome is: up to
20 seconds for a challenge to pass. If it's still blocked, the steps don't
run. `on_block` gets the page, and its `page.steps` shows every step as
skipped, with "the page was blocked".

A step can also lead to a challenge. Clicking through to a checkout, say,
may land on one. After the last step, the page gets the same wait for a
challenge to pass. If it's blocked after that, `on_block` gets it, steps
and all. That holds even when a step failed: a step that couldn't find
its button on a challenge page failed because of the block, and the block
is what you hear about.

With `browser="on_block"`, a request blocked over HTTP gets one go in
Chrome. That go runs the crawl's `browser_steps`, so a cookie banner is
dismissed there too, and the cookies Chrome hands back to the HTTP client
are the ones it had after the steps. On that go, a crawl-wide step that
would fail the request goes on to the next step instead, as if its
`on_error` were `"continue"`. Its error is still in `page.steps`. The
request wasn't asking for those steps, and a banner one site doesn't
show shouldn't undo getting past the block.

## Steps for every page

`Settings.browser_steps` is a step list for every page the crawl fetches
in Chrome: the requests that ask for it, every request with
`browser="always"`, and blocked ones with `browser="on_block"`.

```python
settings = netweir.Settings(
    browser="always",
    browser_steps=[
        steps.click("#onetrust-accept-btn-handler", timeout=3, on_error="continue"),
    ],
)
```

A request's own `steps` replace the crawl's rather than add to them, the
way a request's `meta["download_timeout"]` replaces Scrapy's
`DOWNLOAD_TIMEOUT`. `steps=[]` runs none. To add to the crawl's, put them
in front of yours:

```python
yield netweir.Request(url, steps=[*self.settings.browser_steps, steps.click("#more")])
```

## In rules

A rule can fetch the pages it leads to in Chrome and run steps on them
first. Its item and links then come from the page after the steps:

```python
class Shops(netweir.Spider):
    start_urls = ["https://shops.example/"]
    rules = (
        netweir.Follow(
            "a.shop",
            extract=Shop,
            steps=[steps.click("button.load-more"), steps.wait_for("li:nth-child(40)")],
        ),
    )
```

`steps=` implies `browser=True`. `netweir.Follow(..., browser=True)` on
its own renders the pages without steps, or with the crawl's
`browser_steps`. See [Declarative spiders](declarative.md#rules).

A call step in a rule's steps gets a `Request` as the rule made it: its
URL, its depth and the rule's callback. A rule's requests have no meta
of their own to pass on.

## Your own code

When a step list can't say it, write a function. It gets the live
`BrowserPage` and the request, can be `async`, and what it returns is the
step's `result`:

```python
class Shop(netweir.Spider):
    async def start(self):
        yield netweir.Request(url, steps=[steps.call(self.log_in), steps.click("#orders")])

    async def log_in(self, page, request):
        await page.fill("#user", request.meta["user"])
        await page.fill("#password", request.meta["password"])
        await page.click("button[type=submit]")
        await page.wait_for("#account")
        return "logged in"
```

`steps.call` takes a spider method, its name (`steps.call("log_in")`), or
any other function. A bare function in the list works too. The call has
the step's timeout, it runs alongside the crawl's callbacks rather than
waiting for one to finish, and an exception it raises fails the step,
which then does what its `on_error` says. What it returns must be JSON,
the same as the other results.

Write it with `async def`. A plain function runs on the crawl's event
loop, so while it runs, nothing else in the crawl does, and a slow one
holds up every callback and every other page's call steps. The crawl
warns once for each plain function it calls.

Where it can go depends on how the request travels:

- A spider method, or its name, goes anywhere a step list does. A
  checkpoint and a shared queue keep its name, and the method is looked
  up on the spider when the step runs, on whichever machine that is.
- Any other function (a lambda, a function from another module) only
  works while the request stays in this process. With a `checkpoint` or a
  shared `queue`, submitting such a request raises `TypeError`, the same
  as a callback that isn't a spider method.
- `browser_steps` and a rule's steps name spider methods only. Settings go
  to worker processes, and rules run in Rust, which holds a name but not a
  function.

Call steps always run in the crawl's own process, even with `workers=`.
The page is open in Chrome there, and a live page can't move to another
process. A callback with `page.browser` runs there for the same reason.

## Workers, checkpoints and shared queues

A request's steps travel with it. A request still waiting when the crawl
stops is saved with its steps, and on resume it runs them. A request taken
from a shared queue on another machine runs them there.

With `workers=`, the steps themselves run in the crawl's process,
alongside Chrome. A callback that reads a page Chrome rendered also runs
there, because `page.browser` is a live page. A rules spider's callbacks
don't get `page.browser`, so they still go to the workers, and
`page.steps` goes with them.

## Duplicates

A request's own steps are part of its fingerprint, as Zyte API's actions
are with scrapy-zyte-api: the same URL with other steps leaves another
page, so it's another request. Only what the steps do counts, not how
long they may take: the same clicks with another `timeout` or `on_error`,
or `type` at another pace, are the same request. A request with no steps
of its own, or `steps=[]`, has its plain fingerprint, whatever the
crawl's `browser_steps`.

The same fingerprint is what the duplicate filter and deltafetch go by,
so both tell a URL with steps apart from the same URL without, in either
order: once a request with steps has been fetched, the same URL without
them is still a page to fetch. Pages fetched in Chrome aren't kept in the
development cache at all, nor in a WARC or HAR archive, with steps or
without.

To have steps left out, as Scrapy leaves `meta` out of its fingerprint
and scrapy-playwright's page methods are left out with it, set
`meta["steps_in_fingerprint"] = False`. The request is then a duplicate
of the same URL without steps.

## Not looking automated

Steps use the same input as the page methods: real mouse movement over
an element before a click, a button held for a moment, a tap on a phone,
key presses from Chrome's input pipeline rather than script events, and a
wheel that turns 100 pixels at a time. `type` clicks into its field and
holds and spaces its keys as described in [Typing](#typing). A dialog's
answer waits as long as a person would take to read it, the same as when
you answer it yourself. No step calls `element.click()` or dispatches a
synthetic event.

The exceptions, which are the same when you call the methods yourself:

- `fill` focuses its field from a script and inserts the whole text at
  once, as pasting does, with no keys pressed. Use `type` where a page
  watches the keys.
- `type` selects what's in the field from a script on a phone, which has
  no Control-A, and on a page that swallows Control-A. It focuses the
  field from a script when something other than its label covers it. A
  character off the US keyboard arrives as text with no key on any
  device.
- `select_option`, and `set_input_files` clearing a field, set the
  element from a script and send the `input` and `change` events a
  person's choice would.

See [Not looking automated](browser.md#not-looking-automated).

## Steps from data

A step list is JSON, and JSON can come from anywhere a request can: a
checkpoint file, or a shared queue in Redis that any of your machines,
or anyone who can reach that Redis, writes to. Most steps only act on a
page the crawl was going to fetch anyway. Two can reach further:

- `set_input_files` uploads files from the machine running Chrome, and
  a step list naming `/etc/passwd` and a form on the attacker's site
  would send them that file. So a request read back from data (the
  shared queue, or a checkpoint) may upload only from inside
  `Settings.browser_upload_dir`. Without that setting, such a request is
  refused: dropped with a warning from the queue, or skipped with an
  error on resume. The paths are checked when the request is read back,
  and again, with links followed and `..` resolved, just before Chrome
  is handed the files, so a file swapped for a link in between gets
  nowhere.

  With a shared queue, every request is read back from data, your
  spider's own included: a request goes into Redis when it's made and
  comes out wherever it's fetched, and nothing on the way out says
  which process made it. So with a queue, any upload at all needs
  `browser_upload_dir`, set to the folder your files are in. Without a
  queue, the steps your spider makes may upload from anywhere, except on
  resuming from a checkpoint.
- `evaluate` runs any JavaScript in the page, with the page's cookies
  and the crawl's session for that site. It's held to the page, as the
  site's own scripts are, so it can't reach your files. But it can read
  or send what the page can see. Netweir allows it from data; treat
  write access to the queue as the power to run script on your crawl's
  pages.

A call step, callback or errback from data must also name a method of
your own spider's classes, private `_names` included, or one of the few
of netweir.Spider's that netweir itself calls as callbacks: `parse`,
`on_block` and `_sitemap`. A dunder (`__init__`) is refused, and so is
the rest of netweir.Spider: a queue entry naming `closed`, `crawl` or
`run` would otherwise call the spider's own machinery mid-crawl. That
holds for your own `closed` or `start` too: overriding a hook doesn't
make it a callback. The warning for a refused request says which rule it broke. See
[Keeping Redis safe](distributed.md#keeping-redis-safe).

## Coming from Zyte API, scrapy-playwright, Scrapling or Crawlee

| You wrote | In Netweir |
|---|---|
| Zyte API `"actions": [{"action": "click", "selector": {"type": "css", "value": "#more"}}]` | `steps=[steps.click("#more")]` |
| Zyte `{"type": "xpath", "value": "//a"}` | `"xpath=//a"`; Zyte's `state` is `wait_for`'s `state` |
| Zyte `doubleClick`, `type`, `select`, `hover`, `keyPress` | `steps.double_click`, `steps.type`, `steps.select_option`, `steps.hover`, `steps.press` |
| Zyte `scrollBottom` | `steps.scroll()`. `times=` is `maxScrollCount`. `idle=` is how long to wait after each turn for the page to grow, which is what `maxScrollDelay` caps. `maxPageHeight` has no counterpart: use `times=`. `scrollStep` has none either, as the wheel turns 100 pixels a notch |
| Zyte `scrollTo` with a selector | `steps.scroll("#reviews")`, which wheels the element into view; for a `top`/`left` in pixels, `steps.scroll(y=...)` turns by that much |
| Zyte `waitForSelector`, `waitForTimeout`, `waitForNavigation`, `waitForResponse` | `steps.wait_for`, `steps.wait`, `steps.wait_for_navigation`, `steps.wait_for_response` (a glob in place of `urlPattern` and `urlMatchingOptions`) |
| Zyte `evaluate` | `steps.evaluate`, and its return value is the step's `result` (Zyte returns none) |
| Zyte `onError: "return"` (its default), `"continue"` | `on_error="stop"`, `"continue"`; Netweir's default is `"fail"` |
| Zyte `response.raw_api_response["actions"]` | `page.steps` |
| Zyte selectors can't reach into iframes | `frame={"name": ...}` or `frame={"url": ...}` |
| scrapy-playwright `meta={"playwright": True, "playwright_page_methods": [PageMethod("click", "#more")]}` | `steps=[steps.click("#more")]` |
| `PageMethod("screenshot", full_page=True)`, read back as `.result` | `steps.screenshot(full_page=True)`, read back as `page.steps[i]["result"]` |
| `PageMethod(some_function)` | `steps.call(self.some_method)`, called with `(page, request)` |
| `playwright_include_page=True`, then `response.meta["playwright_page"]` | `page.browser` in the callback, open until it returns, then closed for you |
| Scrapling `page_action=fn` | `steps=[steps.call(self.fn)]`; a step that fails is reported in `page.steps` (or fails the request) rather than only logged |
| Scrapling `wait_selector="li"` | `steps.wait_for("li")` |
| Crawlee `postNavigationHooks: [async ({ page }) => ...]` | `browser_steps=[steps.call("hook")]`, or a request's own `steps` |
| Crawlee `preNavigationHooks` | what the page starts with: the request's `cookies`, `browser_block`, and `browser_environment` for its screen, language and the rest |

Some of them don't carry over. Zyte's site-specific actions (`searchKeyword`,
`setLocation`), `hide` and its browser scripts have no steps, so use
`evaluate` or a call step. Zyte's `goto` and `reload` have none either: a
new page is a new `Request`, which the crawl schedules, checks against
robots.txt and deduplicates. Zyte's `waitForRequest` has none: wait for
the response instead.
