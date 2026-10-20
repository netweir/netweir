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
[Browser](browser.md#acting-on-the-page) for those.

| Step | Does | Its `result` |
|---|---|---|
| `click(selector)` | clicks the element with the mouse (taps it on a phone) once it's visible, still, enabled and not covered | |
| `fill(selector, text)` | replaces what's in a field with `text` in one go, the way pasting does | |
| `type(selector, text, delay=None)` | types `text` one key at a time, replacing what's there, with a pause of `delay` seconds between keys (a `(least, most)` pair picks one at random each time; 0.08 to 0.25 by default) | |
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
| `screenshot(full_page=False)` | takes a PNG of the window, or of the whole page | the PNG's bytes |
| `answer_dialogs(accept=True, text=None)` | from here to the last step, answers every `alert()`, `confirm()` and `prompt()`: OK if `accept`, with `text` in a prompt's box, Cancel if not | |
| `call(method)` | runs a function of yours with the page; see [Your own code](#your-own-code) | what it returned |

A `wait_for_navigation` or `wait_for_response` belongs straight after the
step that sets it off. It looks back to the start of that step, so a
navigation or response that has already happened still counts:

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

## Time limits

Every step has a time limit. `timeout=` sets it in seconds, and a step
without one gets the browser's own, which is 30 seconds. That includes
`scroll` with `times=` and `evaluate`, which have no limit when you call
them yourself.

A step waiting for something that never comes uses all of its time limit.
For a banner that only some pages show, a 30-second wait on every other
page adds up fast, so give an optional step a short timeout.

`type` is the exception. Its timeout covers finding the field, and the
typing itself takes as long as the text needs.

The crawl allows a page in Chrome time for the page itself and then time
for its steps: the sum of their limits, plus time for a challenge they
lead to.

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

```python
def parse(self, page):
    shown = page.steps[-1]["result"]  # an evaluate's value
    with open("catalogue.png", "wb") as f:
        f.write(page.steps[0]["result"])  # a screenshot's PNG
```

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
are the ones it had after the steps.

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

## Duplicates and the cache

Steps aren't part of a request's fingerprint. That follows Scrapy, which
leaves `meta` out of the fingerprint and fingerprints what's sent to the
server. scrapy-playwright follows the same rule, since its page methods
live in `meta`. So two requests for the same URL with different steps are
duplicates, and the second is dropped. To fetch a URL again with other
steps, give the request `dont_filter=True`. deltafetch knows a request by
the same fingerprint, so give it `meta["deltafetch_key"]` if each set of
steps should count on its own.

Pages fetched in Chrome are never kept in the development cache or in a
WARC or HAR archive, with steps or without.

## Not looking automated

Steps use the same input as the page methods: real mouse movement over
an element before a click, a tap on a phone, key presses from Chrome's
input pipeline rather than script events, and a wheel that turns 100
pixels at a time. `type` pauses between keys the way a person does. A
dialog's answer waits as long as a person would take to read it, the same
as when you answer it yourself. Nothing in a step list calls
`element.click()` or dispatches a synthetic event.

`select_option` and `set_input_files` are the exceptions, as they are
when you call them yourself: they set the element from a script and send
the `input` and `change` events a person's choice would. See
[Not looking automated](browser.md#not-looking-automated).

## Coming from Zyte API, scrapy-playwright, Scrapling or Crawlee

| You wrote | In Netweir |
|---|---|
| Zyte API `"actions": [{"action": "click", "selector": {"type": "css", "value": "#more"}}]` | `steps=[steps.click("#more")]` |
| Zyte `type`, `select`, `hover`, `keyPress` | `steps.type`, `steps.select_option`, `steps.hover`, `steps.press` |
| Zyte `scrollBottom` | `steps.scroll()`, with `times=` and `idle=` for `maxScrollCount` and `maxScrollDelay` |
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
`setLocation`) and its browser scripts have no steps, so use a call step.
Zyte's `goto` and `reload` have none either: a new page is a new
`Request`, which the crawl schedules, checks against robots.txt and
deduplicates.
