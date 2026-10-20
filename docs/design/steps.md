# Netweir browser steps design

Status: implemented, October 2026
Author: James Conway

## What it is for

A request fetched in Chrome reaches its callback with `page.browser`, the
live page, and the callback can do anything with it. That worked, but it
put the same handful of lines in every spider: dismiss the banner, click
"Load more", wait for the list. Two kinds of spider couldn't do it at all.
A rules spider never gets `page.browser`, because its pages are read in
Rust. A crawl-wide habit, such as dismissing one consent manager on every
page, had nowhere to go but a downloader middleware.

Steps are a list of page actions carried by the request, run by the
engine once the page has loaded and before anyone reads it.

## Prior art

Four designs were read for this: the documentation, and the source where
it's public.

**Zyte API actions.** The closest to what Netweir needed. The
`actions` request field is a JSON list of objects, each with an `action`
key. There are 20 actions: `click`, `doubleClick`, `evaluate`, `goto`,
`hide`, `hover`, `interaction`, `keyPress`, `reload`, `scrollBottom`,
`scrollTo`, `searchKeyword`, `select`, `setLocation`, `type`,
`waitForNavigation`, `waitForRequest`, `waitForResponse`,
`waitForSelector` and `waitForTimeout`. Waits default to a 5 s timeout,
15 s at most, and all actions together get 60 s ("the on-going action is
interrupted, follow-up actions are not executed at all, and you get your
requested output ... as it was rendered at that time"). `onError` is
`"return"` (the default, which stops) or `"continue"`. The response's
`actions` array gives each action an `action`, an `elapsedTime` and a
`status` (`success`, `continued`, `returned`, `notExecuted`), plus an
`error`. `evaluate` returns no value: the documented way to get one out
is to write it into the DOM. Selectors "cannot interact with iframes".
scrapy-zyte-api's fingerprinter keeps `actions` in the fingerprint
("requests with the same target URL but different `actions` are also
considered different requests").

**scrapy-playwright `PageMethod`.** Defined as `PageMethod(method: str |
Callable, *args, **kwargs)`, a call to make on the Playwright page, or a
function called with the page. They go in
`meta["playwright_page_methods"]` and run in order after `page.goto`, with
`wait_for_load_state` after each. A method's return value lands on
`PageMethod.result`. An exception isn't caught, so it fails the download,
and the request's errback hears about it. There's no fingerprinting of its
own: page methods are in `meta`, which Scrapy's default fingerprint leaves
out. A `PageMethod` is a plain object, so a request carrying one pickles
into a `JOBDIR` queue only when its method is a string or a module-level
function.

**Scrapling `page_action`.** One callable, called with the page "after
navigation". It runs after the network-idle wait and before
`wait_selector`. Its return value is ignored, and an exception in it is
logged (`log.error(f"Error executing page_action: {e}")`) and otherwise
swallowed, so the fetch carries on and returns the page.

**Crawlee `preNavigationHooks` and `postNavigationHooks`.** Lists of
functions. A pre-hook gets the crawling context and the `goto` options it
may change, and a post-hook gets the context. They run in order under one
shared `navigationTimeoutSecs`, and a throw goes down the ordinary request
error path, which retries the request. Nothing in Crawlee is declarative.

What that settles:

- The step list is data, as Zyte's is. Of the four, only Zyte's survives
  serialisation without conditions, and Netweir requests already travel as
  JSON (a checkpoint's rows, the shared queue's members, cookies since
  #100).
- Code is the escape hatch, as `PageMethod(callable)` and Crawlee's hooks
  are, and it sits inside the list so it can run between two declarative
  steps.
- Every step has a result, as `PageMethod.result` does. That includes
  `evaluate`, which Zyte can't return.
- A failing step fails the request by default, as scrapy-playwright and
  Crawlee do. Zyte's default, `"return"`, hands back a partial page as if
  it were the page, and Scrapling only logs. Both mean a spider that
  extracts nothing and never says why. Netweir offers Zyte's two choices
  as `on_error="stop"` and `"continue"`.

## The format

```json
[
  {"action": "click", "selector": "#accept", "timeout": 3, "on_error": "continue"},
  {"action": "type", "selector": "#q", "text": "green tea", "delay": [0.08, 0.25]},
  {"action": "press", "key": "Enter"},
  {"action": "wait_for_navigation"},
  {"action": "evaluate", "js": "document.querySelectorAll('li').length"},
  {"action": "call", "method": "log_in"}
]
```

Field names are Netweir's own snake_case ones, the parameter names of the
page methods. They aren't Zyte's camelCase. A field that no step has is
reported first, as "click has no field "selecter"", because a misspelling
says more than the field it was meant to be going missing. A field this
action doesn't take is an error too, since a step that quietly drops a
mistyped `timeout` waits 30 seconds on every page.

`netweir.steps` has one function per action. Each returns the dict, so
there's no class to pickle and nothing to convert: `Request.steps` is a
list of dicts, and a bare function in it becomes `{"action": "call",
"method": fn}`.

The list is parsed once, in Rust (`netweir_browser::steps::parse`), into a
`Step` enum. The parse happens when the `Request`, `Settings` or `Follow`
is made, so an error comes from the line that wrote it, not from a page an
hour into the crawl.

## Where it runs

In the engine, in `render()` (crates/netweir-core/src/crawl.rs), between
the page loading and the HTML being read. Three things depend on it
being there, and none of them would work if Python ran the steps on
`page.browser`:

1. **Rules.** A rules spider's pages are parsed in Rust, and the live page
   is closed before Python sees anything. Steps in Python would never
   reach them.
2. **Blocks.** The engine already classifies a rendered page and waits up
   to 20 s for a challenge to pass. Running the steps inside `render()`
   puts their result through the same classifier, the same challenge
   wait, and the same `Blocked` event, `on_block` hand-off and cookie
   hand-back.
3. **Time.** `browser_fetch` holds every Chrome fetch to a limit, so a
   Chrome that stops answering can't hold a page slot forever. The steps'
   own limits are added to it: `steps::budget()`, the sum of each step's
   timeout, plus one more challenge wait.

The runner, `netweir_browser::steps::run`, is a loop over the existing
`Page` and `Frame` methods. It doesn't reimplement any of them, so a click
step is `Frame::click`, with its actionability checks, mouse movement and
phone taps. The runner only adds what a list needs:

- **Timeouts.** Each step's timeout goes to the method's own `timeout`,
  so the method's message (which check never passed) is the one you see.
  An outer `tokio::time::timeout` with half a second of grace covers
  methods that have none (`press`, `evaluate`, `screenshot`, a call).
  `scroll` with `times=` is held to the timeout too: unbounded, it would
  make the crawl's limit meaningless.
- **Navigation.** The runner keeps the document showing before each step.
  `wait_for_navigation` waits past the one from before the previous step,
  and `Page::wait_for_navigation` returns at once if that already
  happened, so the step can come after the click that navigates. After
  any step that changed the document, the next step waits for the new
  one's `load`, as scrapy-playwright's `wait_for_load_state` does.
- **Responses.** Only a list with a `wait_for_response` listens to the
  network (`Page::listen`), since a page nobody listens to costs nothing.
  Responses are tagged with the step they arrived during, and the wait
  matches those from the previous step on.
- **Dialogs.** `answer_dialogs` takes the page's dialog receiver for the
  rest of the steps, and the defaults come back when the steps end.
  Answers keep the dialog pause, so a step list answers no faster than a
  person.
- **Frames.** `frame={"name", "url"}` polls `Page::frames()` until the
  step's deadline, since a consent manager's frame often loads after the
  page.

A field covered by something that isn't its label is waited out by
`click_as`'s own retry for four fifths of what's left of the step's
time, as a click is, before `type` focuses it from script with the
rest: a loading overlay that lifts in a second got a script focus when
the check was made only once.

`type` is new for this milestone, as `Page::type_text` and Python's
`page.type()`. `fill` inserts the text in one go, the way pasting does.
Zyte's `type` and the stealth rules both wanted real keystrokes, and a
first version that only pressed one key per character didn't look like
a person. A page logging its key events saw every key held 0 to 2 ms,
"A" with no Shift, `.` and `@` with an empty `code` and `keyCode` 0, the
field focused by script with no click, and the first key in the same
millisecond as the focus. Now:

- The field is clicked (tapped on a phone) through `click`'s path, then
  there's a 150 to 400 ms pause. If the field isn't empty, its contents
  are selected with Control-A, or Command-A where the page's
  `navigator.platform` is a Mac, sent as key events with CDP's
  `selectAll` command. On a phone, which has no such keys, they're
  selected from script.
- `Page::press` holds each key for a log-normal time (median 85 ms,
  45 to 180), with the long tail measured key holds have.
  Mouse buttons are held 50 to 120 ms too, where they had been let go in
  the same instant.
- `Key::of` knows the US layout for printable ASCII: `code` and
  `keyCode` for every character, and Shift down and up around shifted
  ones (`modifiers` 8). A character the layout lacks goes through
  `Input.insertText` with no key events, as an OS's character picker
  sends it. An IME composition would fake a keyboard the page can't
  check either way.
- The gap between keys is log-normal around the geometric mean of
  `delay`'s two ends, clipped to them, rather than uniform.
  `steps::TYPING` (80 to 250 ms) is the default.

`typing_time()` is the most that typing a text can take, which the
budget and the run's limit use, so a long text isn't cut off at the
step's timeout. The timeout covers getting to the field.

## Outcomes

`netweir_browser::steps::Outcome` has `action`, `status` (`ok`, `failed`,
`skipped`), `error`, `elapsed`, `value` (JSON) and `image` (a
screenshot's PNG, or a JPEG with `quality`). A `Report` holds
them, along with which step failed the list, if one did.

They ride on the response: `netweir_core::Response::steps` is an
`Arc<[Outcome]>`, empty for every response that isn't from Chrome. That
puts them wherever a response goes. Python reads them as `page.steps`, and
a rules page that goes to a worker process takes them along in
`Response.__reduce__`, as JSON with the image in base64. The Python
list is made once per response and kept, so reading `page.steps` twice
doesn't copy a screenshot twice. For a request the
steps failed, `FetchError::steps` carries them to the errback, so a
screenshot taken before the failure can still be looked at.

`FetchErrorKind::Step` is a new kind of error, and it isn't transient.
`Other`, which a browser failure would otherwise be, is retried. A step
may already have sent a form, and a retry would send it twice.

## Blocks mid-steps

Before the steps, a page still blocked after its challenge wait doesn't
run them. Its steps are reported as skipped, with "the page was blocked",
and the existing `Blocked` event goes out. A page answering an HTTP error
skips them the same way ("the page answered 404"). A 5xx is retried, and
its retry runs the steps.

After the steps, the page goes through `settle()` again. That's the
challenge wait factored out of `render()`, so a step that lands on a
challenge gets the same 20 s for it to pass. Then:

| After the steps | The request gets |
|---|---|
| blocked, whatever the steps did | `Blocked`, for `on_block` |
| not blocked, a step failed with `on_error="fail"` | `Failed`, kind `step` |
| not blocked, otherwise | `Fetched`, the HTML as the steps left it |

The block wins over a step failure. A step that couldn't find "Add to
basket" on a Cloudflare page failed because of the block, and treating it
as a step failure would hide the block from the circuit breaker, the
proxy rotation and `on_block`.

## Call steps

A call step is Python code, and the engine is Rust. When the runner
reaches one, `caller()` sends a `StepCall` on a channel: the request's id,
the step's index, the method name if there is one, the `LivePage`, the
timeout, and a oneshot for the answer. Python takes the receiver when it
builds the engine. `Crawler.serve_calls()` is a task running alongside
the main loop, awaiting `engine.next_call()` and running each call in a
task of its own. That avoids the obvious alternative, an event in the
engine's batches. Those are handled one at a time in the main loop, so a
call would wait behind whatever callback was running, and that wait would
come out of its own timeout.

The function is found by name (`getattr(spider, method)`). A step with no
name is found by its place in the waiting `Request`'s own `steps`. The
answer is JSON text, `reply()`, or a message, `fail()`, which becomes
the step's error. A StepCall dropped unanswered fails the step at once, and an
answer that doesn't come in time fails it at its timeout; Python also
cancels its side then. A crawl with
nobody taking the calls (the engine driven from Rust alone) fails a call
step at once rather than waiting out its timeout. So does one that comes
after Python's task has stopped (`close_calls()`, called as it ends),
where it used to sit in the channel until its timeout.

A rule's request exists only in Rust, so a `StepCall` also carries the
request's URL and depth, and Python adds the rule from the engine's tags.
The function gets a `Request` with that URL, depth, the rule's callback
and its steps.

A plain (not `async`) function blocks the event loop for as long as it
runs, every callback and other call with it. The crawl warns once per
function.

How it travels, compared with callbacks:

| Call step names | No checkpoint | Checkpoint or shared queue | `browser_steps`, `Follow(steps=)` |
|---|---|---|---|
| a spider method, or its name | yes | yes, by name | yes, by name |
| any other function | yes | `TypeError` at submit | `TypeError` where it's written |

It's the rule callbacks already follow, with the same message.
`Settings` goes to worker processes and rules live in Rust, so those two
take names only.

Call steps always run in the crawl's process, because that's where the
live page is. With workers, a callback for a page with `page.browser` runs
there too, as it already did. A rules page, which has no live page, goes
to a worker with its outcomes.

## Settings and precedence

`Settings.browser_steps` is the default for every page in Chrome,
including the one go `browser="on_block"` gives a blocked request.
`Request.steps` and `Follow(steps=)` replace it. They don't add to it. `None`
means the crawl's default and `[]` means none. That's how Scrapy treats a
setting and a request's own meta for the same thing (`DOWNLOAD_TIMEOUT`
and `meta["download_timeout"]`). A request that wants both can say
`[*settings.browser_steps, ...]`. Merging would leave no way to switch
the crawl's steps off for one request.

On the one go a blocked request gets, the crawl's steps run with every
`on_error="fail"` read as `"continue"` (`lenient()` in crawl.rs). The
request didn't ask for them, and a banner one site lacks would otherwise
turn getting past the block into a failed request. Their errors still
show in `page.steps`.

A request with steps is fetched in Chrome without `browser=True`, as a
Zyte request with `actions` gets a browser. A `Follow` with steps has its
pages fetched in Chrome too. That needed `Follow(browser=)`, which didn't
exist: a rules spider's followed pages could only be rendered with
`browser="always"`.

## Fingerprints

A request's own steps are part of its fingerprint, and through it of
its deltafetch key. (The development cache would key by it too, but it
keeps no page fetched in Chrome.) This follows scrapy-zyte-api, whose
fingerprinter keeps `actions` in, rather than Scrapy and
scrapy-playwright. Scrapy's default fingerprint hashes the method, the
canonical URL, the body and any `include_headers`, and leaves `meta`
out, and scrapy-playwright's page methods live in `meta`.

The first version of this milestone did what Scrapy does, and that
turned out to cost too much. A spider yielding the same URL plain and with
"click page 2" got only the first, and the second was dropped without a
word. The way round it, `dont_filter=True`, also skips the offsite
filter, so the fix for one silent loss was to switch off a safety check.
Different steps leave a different page, which is what the duplicate
filter and deltafetch are meant to tell apart.

The other order needed a fix of its own. Once Chrome has fetched a page,
the URL it ended at is marked seen, as a redirect's target is. Marked
with its bare fingerprint, that made a plain request for the same URL,
after one with steps, a duplicate, and the checkpoint kept it so. The
mark now carries the steps too, when they count toward the request's
fingerprint.

The key is `steps::key()`: each step's action and what it acts on
(selector, text, key, URL glob, frame...) as JSON in a fixed order.
Timeouts, `on_error` and `type`'s pace are left out, as they change how
long a step may take but not the page it leaves. The order is written
out field by field rather than taken from a struct's `Debug` text or a
map, so a fingerprint kept in a checkpoint or a deltafetch record means
the same in the next version, and a unit test pins it.
`canonical::with_steps` mixes the key into the fingerprint after the
cookie jar, as `in_jar` does the jar. No steps, `steps=[]` included,
leave the fingerprint as it was, so checkpoints and records from before
steps stay valid.

Only a request's own steps count. The crawl's `browser_steps` apply to
every page in Chrome alike, and whether a page goes to Chrome at all
under `browser="on_block"` isn't known when it's submitted.
`meta["steps_in_fingerprint"] = False` leaves a request's steps out,
named like the other flags of a request's meta.

Pages from Chrome are never cached or archived, so steps change no
cache key: the cache never sees a request with steps.

## Limits

Each `timeout`, `wait` and `idle` is at most a day, and `type`'s
`delay` at most 10 s. A `timeout` of 1e19 seconds used to overflow:
`Duration` arithmetic in `browser_fetch` panicked outside the task's
catch, so the crawl never ended, and `Instant` additions in the runner
panicked too. Every addition in the step code now saturates, and
deadlines go through `after()`, which falls back to a year away.

A day is still far too long for a page slot to be held, so
`Settings.browser_steps_max` caps all of one page's steps, 120 s by
default. Zyte API's own caps, 15 s for one wait and 60 s for all of a
request's actions, were the guide. The default is four of the browser's
30 s timeouts. It's enforced in two places:

- `steps::within()` refuses, where the steps are written, a list that
  asks outright for more: its waits, the timeouts it gives and its
  typing. Steps on the browser's default timeout don't count, since
  counting each as 30 s would refuse any list of five clicks.
- `steps::run()` holds every step to what's left of the cap, and a step
  that reaches it fails the list, whatever its `on_error`, with a
  `FetchError` of kind `step` naming `browser_steps_max`.
  `steps::budget()` never counts more than the cap, so the crawl's limit
  on a Chrome fetch has a ceiling too.

## Steps from data

A step list can come from a checkpoint or from the shared queue, which
anyone who can reach that Redis writes to. Two things there reach past
the page:

- `set_input_files` reads files on the machine running Chrome, and a
  form on the attacker's own page would send them off: a queue entry
  with `/etc/hosts` and a form that submits on change did just that in
  the repro for this. `from_data()` in `_crawl.py` refuses such a request, from
  the queue or a checkpoint, unless every path resolves inside
  `Settings.browser_upload_dir`, which is None (nothing allowed) by
  default. `from_data()` writes the folder into the step as `within`,
  overwriting whatever the data said, and the engine holds the upload to
  it (uploads.rs). Checking a path just before `DOM.setFileInputFiles`
  wasn't enough: Chrome reads an upload's file when the form is sent,
  which can be steps (or a callback) later, and a file swapped for a
  link in between was sent. So each resolved path is opened without
  following a link at its end (`O_NOFOLLOW`, or
  `FILE_FLAG_OPEN_REPARSE_POINT` and a reparse-point check on Windows),
  checked to be the same file (device and inode on Unix) and to resolve
  the same way still, and copied from the open file into a 0700 folder
  of netweir's own under its original name, which the form shows.
  Chrome is given the copy's path, as it is, and the folder is removed
  when the page closes or is dropped. Uploads not held to a folder
  (steps in a process's own code, without a queue) still give Chrome
  the file's own path.

  With a shared queue, every request comes back through `from_data()`,
  the spider's own included: the queue's members carry nothing that says
  which process made them, and a "made here" marker would be one more
  field for the data to forge. So with a queue, every upload needs
  `browser_upload_dir`, and the refusal says so. Without a queue, steps
  the process's own code makes aren't limited: the code could read the
  file anyway.
- `evaluate` is allowed. It runs in the page's own sandbox, with that
  site's cookies, which is what a page's own scripts can do. The risk
  (script on your crawl's pages and sessions) is set out in "Keeping
  Redis safe" in docs/distributed.md, along with the upload rule.

Names from data were checked by `getattr(spider, name)`, so a queue entry
could name `__init__` or any attribute. `_not_a_callback()` now wants a
function defined on the spider's own classes, or one of
`_NETWEIR_CALLBACKS`: `parse`, `on_block` and `_sitemap`, the methods of
netweir.Spider that netweir names as callbacks itself. Any other name
netweir's spider classes define (`closed`, `crawl`, `start`...) is
refused even where the spider overrides it, since an override is still
a hook netweir calls at its own time: a queue entry naming the
spider's own `closed` ran it mid-crawl. Dunders are never taken. That applies
to callbacks and errbacks as well as call steps, from the queue and
checkpoints alike, since they come down the same path.

A first version refused private `_names` from the queue and walked the
whole MRO. With a queue every request comes back through it, so that
refused a sitemap spider's own `_sitemap` and a spider's own `_detail`
callbacks, while letting an entry name netweir.Spider's `closed`, which
then ran mid-crawl, or `crawl`, which started a crawl inside the crawl
and hung. Each refusal now says which rule it broke: a dunder, netweir's
own machinery, not a method, or not defined.

## Stealth

No step does anything a page method doesn't. The input goes through
`Input.dispatchMouseEvent`, `Input.dispatchTouchEvent` and
`Input.dispatchKeyEvent`, nothing goes through `element.click()`, and the
helpers stay in the isolated world. The exceptions are listed in
docs/steps.md: `fill` focuses from script and inserts its text at once,
`type` on a phone selects the field's contents from script, a character
off the US layout comes with no key, and `select_option` and an empty
`set_input_files` set the element from script. `type`'s holds and gaps
are there for the same reason as the dialog pause: a page can time
keystrokes, and nobody types at 1 ms a key.

Selectors take `xpath=` as well as CSS. `__netweir.find()` in the
isolated world is the one place every helper finds its element, so XPath
cost one function. It lets Zyte's `{"type": "xpath"}` selectors carry
over, and it works for the page methods too.

## Tests

- `crates/netweir-browser/src/steps.rs`: parsing every action, every
  error message, the budget, outcomes to JSON and back, the failure's
  wording.
- `crates/netweir-browser/tests/steps.rs`, in Chrome: a banner, load more,
  `wait_for_response`, typing that counts keydowns, `select_option`, a
  screenshot, a form submitted and waited for; `continue`, `stop` and
  `fail`; dialogs answered and then back to the default; a click in a
  frame that loads late; a call, and one too slow for its timeout.
- `crates/netweir-core/tests/browser.rs`: a crawl's steps and a request's
  replacing them; the HTML after the steps; the stats; a failure that
  isn't retried; a step that leads to a challenge, and a page blocked
  before its steps; call steps with and without someone to take them.
- `tests/test_steps.py`: the builders, validation where a request, a
  `Settings` or a `Follow` is written, pickling of outcomes and of
  `FetchError`; crawls with a banner and load more, a form, an endless
  feed, crawl-wide steps, the three `on_error`s and the errback, call
  steps (a method, a name, a plain function, one that raises, one too
  slow, one whose result isn't JSON), a challenge after a click going to
  `on_block`, a rules spider, workers (a live page's callback in this
  process, a rules page's in a worker with its outcomes), a checkpoint
  resumed with a request and its steps pending, and a checkpoint refusing
  a call step it can't name.

## Not done

- No `goto` or `reload` step. Going to another page is a new `Request`,
  which the crawl schedules, deduplicates and checks against robots.txt.
  A `goto` step would get past all of that.
- No `wait_for_request`. `wait_for_response` covers what the Zyte
  examples use it for, and a request whose response never comes is
  better caught by its own timeout.
- No `hide` step. `evaluate` hides an element, as Zyte's `hide` does
  from script anyway.
- Zyte's `scrollTo` is `scroll(selector)`, which wheels the element into
  view, or `scroll(y=...)` for pixels. It's not a step of its own.
- `double_click` came with `Page::dblclick` (Python `page.dblclick`): a
  press and release with `clickCount` 1, a 70 to 160 ms pause, and again
  with `clickCount` 2; two taps on a phone.
