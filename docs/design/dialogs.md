# Netweir dialogs, popups and downloads design

Status: implemented, October 2026
Author: James Conway

## What it is for

Three things a page can do that leave a driven browser stuck or blind:

- **A dialog.** `alert()`, `confirm()`, `prompt()` and the "leave this
  site?" prompt of `beforeunload` stop the page's script until someone
  answers. With the `Page` domain on, Chrome waits for the DevTools
  client to answer, and before this milestone nobody did: a page that
  called `confirm()` never ran another line, and every command sent to
  it (`evaluate`, a click's checks) waited until its timeout.
- **A popup.** `window.open()` and a link with `target=_blank` open a new
  page. Netweir never attached to it, so it couldn't be read, and
  nothing the opener had set up (routes, blocks, the user agent override)
  reached it.
- **A download.** A link to a file, or a response with
  `Content-Disposition: attachment`, is saved by Chrome rather than shown.
  Chrome's default for a context Netweir makes cancelled it, and nothing
  said it had happened.

## Python API

```python
page.on_dialog(handler)                 # handler(dialog): plain or async
page.remove_dialog_listener(handler)
dialog.type             # "alert", "confirm", "prompt" or "beforeunload"
dialog.message
dialog.default_value    # a prompt's default text
dialog.url              # the frame that opened it
await dialog.accept(prompt_text=None)
await dialog.dismiss()

page.on_popup(callback) / page.remove_popup_listener(callback)
popup = await page.wait_for_popup(timeout=None)
async with page.expect_popup() as info:
    await page.click("a[target=_blank]")
popup = await info.value
await popup.wait_for_load(wait="load", timeout=None)

page.on_download(callback) / page.remove_download_listener(callback)
download = await page.wait_for_download(pattern=None, timeout=None)
async with page.expect_download("**/*.csv") as info:
    await page.click("a.export")
download = await info.value
download.url, download.suggested_filename
await download.path()           # waits for it to finish; the file Chrome wrote
await download.save_as(path)
await download.body()           # bytes
await download.failure()        # None, or why it didn't finish
await download.cancel()
```

They work as the network listeners do (see [network.md](network.md)):
a listener is a plain function or a coroutine function, and an exception
in it is logged to the `netweir` logger. Chrome's events reach Python
only while something listens or waits.

In Rust, `Page::listen()` carries popups and downloads with the requests
and responses: its events, renamed `PageEvent`, are a request, a
response, a popup (a `Page`) or a `Download`. Dialogs have a receiver of
their own, `Page::dialogs()`, as routes do, because holding one is a
promise to answer: a program that listens to responses and reads them
later would otherwise hold every dialog meanwhile, and the page with it.
Python takes dialogs only while a dialog listener is added; its
listeners run one after another, each awaited, and then the dialog is
let go, which answers it the default way if none did.

## Dialogs

Chrome sends `Page.javascriptDialogOpening` on the page's session, with
the type, the message, a prompt's default text and the URL of the frame
that opened it, and the dialog stays open until
`Page.handleJavaScriptDialog` answers it (`accept`, and `promptText` for a
prompt). Netweir already has `Page.enable` on every page's own session;
nothing new is enabled.

**It never waits for nobody.** Each dialog becomes a `Dialog`, handed to
whoever takes the page's dialogs. One that nobody answers is answered
for them: at once when nobody takes dialogs, when the last copy of it is
dropped (in Python, once every dialog listener has returned), or when
the browser's timeout runs out with a listener still holding it. The default dismisses an alert, a confirm or a prompt, which
is what pressing Escape does, and accepts `beforeunload`: Netweir's own
`goto()` is what asked to leave, and staying would fail it.

**It never answers sooner than a person could.** A page can time how long
`confirm()` blocked it, and a dialog answered within a millisecond is
something only a program does. So every answer, the default and the
listener's alike, is held until a moment after the dialog opened: between
half a second and a second and a half, picked at random for each dialog.
A listener that takes longer is answered when it says. A dialog counts
as answered from the moment `accept` or `dismiss` is called, so a plain
Python listener can call one without awaiting it; a second answer
raises.

**Frames.** Chrome reports a dialog opened by a frame, its own process's
or another site's, on the page's session, where `Page` is enabled, with
the frame's URL; the answer goes there too. A test opens one from a frame
on another site.

**`beforeunload` on close.** `close()` uses `Target.closeTarget`, which
closes the page without running its `beforeunload` handlers, so closing
never waits on one. Chrome only shows `beforeunload` once a person has
interacted with the page (a click counts), so a page that was only
loaded and read never asks.

**Headless and headed.** The same either way. With a window, Chrome
also draws the dialog, and Netweir's answer closes it; a person answering
it in the window first leaves Netweir's answer with nothing to do, which
it takes quietly.

## Popups

A popup is a new `page` target with an `openerId`, the target of the page
(or of the frame from another site) that opened it. The page's own
`Target.setAutoAttach` doesn't reach it: Chrome attaches a page session
only to its frames and workers.

So the browser's own session sends `Target.setAutoAttach` with
`waitForDebuggerOnStart` and a filter of `page` targets. Chrome then
attaches every new page and holds it before it runs anything, as it
holds frames since milestone 34. The browser's handler hears Netweir's
own `Target.attachToTarget` too, and the pages there already, neither of
them held; it looks only at those Chrome holds:

- One whose opener is a page of Netweir's, or a frame from another site
  in one, is a popup. It's set up exactly as `new_page()` sets a page up
  (`Page.enable`, lifecycle events, `Network.enable`, the Fetch domain
  for its rules, the user agent override, its own `Target.setAutoAttach`
  for its frames and workers), then let go with
  `Runtime.runIfWaitingForDebugger`, then told to the opener's
  listeners. Its first request goes out with all of that in place.
- Anything else is let go and left at once: a page Netweir made with
  `Target.createTarget` (which has its own session, attached as before),
  or a tab of someone else's in a browser Netweir connected to. Each is
  held for one round trip.

A held page doesn't answer `Page.enable` until it runs, so setup can't
wait for each answer before sending the next command, as a frame's
does. Every setup command is sent at once, in order, then the popup is
let go, and the answers are waited for after: Chrome takes a session's
commands in the order they come, so the popup runs only after all of
them. Pages from `new_page()` are set up the same way now, a few round
trips sooner. A popup whose setup fails, or outlasts the browser's
timeout, is closed with a warning, rather than left fetching without
its rules (or, in a crawl, without the guard that keeps it off what
robots.txt rules out).

A page's main frame has its target's id, which the page's state now
knows from the start: a popup's first response can arrive before the
frame tree does, and is still taken as its document's.

**What applies to a popup.** It's in its opener's context, so it shares
its cookies and storage, and the context's routes. It also has its
opener's own routes and blocks behind its own, so that a page which
blocks images, or routes an API, gets the same in the window a click
opens from it: its own routes first, then its opener's, then the
context's. A page's guard, which a crawl uses to keep its page off what
robots.txt rules out, guards its popups' documents too. Listeners don't
carry over; a popup has its own. Closing a page whose context is its own
(`browser.new_page()`) closes its popups with the context.

**Tabs in front and behind.** A popup opens as a tab in front of its
opener, and Chrome counts the opener as hidden (`visibilityState` is
`hidden`): it runs no animation frames there, and the mouse goes to the
tab in front. A click on the opener then took five seconds, its checks
waiting on frames that never came. A person switches back to a tab
before using it, so Netweir does too: the pages of one opener and its
popups share which of them is in front, and an action that sends input
(`click`, `fill`, `press`, `hover`, `scroll`, and `check` through
`click`) on one that isn't sends `Page.bringToFront` first. The page sees
`visibilitychange` to `visible`, as it would. Nothing is sent for a page
already in front, so pages without popups see no change.

**Reading it.** The popup comes to its listeners once it's running, on
whatever document Chrome shows first. `wait_for_load()` waits until the
document showing when it was called, or one that replaced it, reaches
the wait state. Chrome's first document in a popup is its initial empty
one, which never fires `load`, so a popup heading for a URL is waited
for until that URL's document loads; one opened blank (`window.open('')`)
commits an `about:blank` document that loads at once.

**Popups from frames.** A frame from another site has its own target,
and its popup names that target as its opener. Netweir knows each page's
frames from other sites by their targets, so their popups are their
page's. A test opens one with a click in such a frame.

## Downloads

Each context Netweir makes is given its own folder for downloads with
`Browser.setDownloadBehavior` (`allowAndName`, `browserContextId`,
`eventsEnabled`): Chrome saves each file there under the download's GUID,
so no name a server picks reaches the disk. A browser keeps every
context's folder under one temporary folder, made the first time it's
needed, with a lock as a temporary profile has, so a program killed
mid-download leaves nothing behind for long: the next launch's sweep
takes it. A context's folder is removed when the context closes (a page
from `browser.new_page()` closes its context with it); everything goes
when the browser closes. Pages of a kept profile download into a folder
of the default context's, the same way. Nothing goes to the user's
Downloads folder, headless or headed.

Chrome tells the page's session `Page.downloadWillBegin` (the frame, the
GUID, the URL and the name the server suggested), and the browser's
session `Browser.downloadProgress` as bytes arrive, then once more with
`completed` or `canceled`. The page's event says which page it was; the
browser's carries on after the page has closed, which a download can
outlive. A download started in a popup that never showed a document (a
`target=_blank` link to a file opens one and Chrome closes it again) is
told to the opener too, where whoever clicked is listening.

`path()` waits for the download to finish and returns the file Chrome
wrote, or raises if it was cancelled or failed. `save_as` copies it,
`body` reads it, and `failure()` waits and returns `None` or
`"canceled"`, which is what Chrome reports for a cancelled download and
for one whose connection broke. `cancel()` sends `Browser.cancelDownload`.

**A download that `goto()` started** fails the navigation with
`net::ERR_ABORTED`, as it always has, since no document came; the
download carries on, and a listener hears it.

## Stealth

Nothing here is visible to page script:

- `Page.handleJavaScriptDialog`, `Browser.setDownloadBehavior`,
  `Browser.cancelDownload` and the browser's `Target.setAutoAttach`
  change nothing a page can read. No domain is enabled that wasn't.
- `Page.bringToFront` changes what a person switching tabs changes, and
  only when a popup has put another tab in front.
- A dialog is never answered faster than a person could, as above.
- A popup is held while it's set up, as DevTools holds it when it's
  open: the page that opened it sees `window.open()` return at once, and
  the popup's first request starts a few round trips later.
- Chrome's popup blocker stays on, as a person has it: a popup opened
  without a click is blocked.

## Known limits

Known limit: `page.evaluate("window.open(...)")` opens nothing. Chrome's
popup blocker only lets a page open a window from a user gesture, and
`evaluate` isn't one; giving it one (`userGesture`) would hand the page a
user activation no person made. Click whatever opens it. This is why a
same-site `window.open('/x')` from `evaluate` never became readable.

Known limit: a listener added to a popup hears from the moment it's
added; the popup's first requests may already have gone. Routes and
blocks reach them, since they're the opener's or the context's.

Known limit: which page a download belongs to comes from
`Page.downloadWillBegin`, which the protocol marks deprecated in favour
of the browser's event, whose frame id alone can't name the page. Chrome
155 still sends it.

Known limit: for a browser Netweir connected to, each context's download
folder is a path on this machine. A Chrome running elsewhere (in Docker,
say) saves to that path on its own machine, or fails to.

Known limit: a dialog's default answer, and the human pause before any
answer, are not configurable: a listener that answers is the way to
choose. A page that opens dialogs in a loop costs about a second each.

Known limit: which tab is in front is known only for a page and the
popups it opened, from what Netweir did. A tab a person brings to the
front of a headed Chrome, or a popup the page closes itself, leaves
Netweir's idea of it behind; the cost is one `Page.bringToFront` too
many, or a slow action on a hidden tab, as before this milestone.

## Testing

Tests in `crates/netweir-browser/tests/events.rs` and
`tests/test_browser.py`, each written to fail first:

- `confirm()` from `evaluate` returned nothing in 30 seconds; now it
  returns `false` after the human pause, and every kind of dialog is
  answered by default, and by a listener with prompt text, in Rust and in
  Python with a plain, an async and a silent listener. A dialog from a
  frame on another site is heard and answered.
- `goto()` away from a page whose `beforeunload` asks, after a click,
  leaves; `close()` doesn't wait on it.
- A `target=_blank` link and a `window.open` from a click give a popup
  that loads and reads back with its document's response, carrying the
  opener's route, block and the user agent override from its first
  request; a context's route reaches it too. A click in a frame from
  another site opens one as well, and so does a blank `window.open('')`
  a script writes into. The second click on an opener whose popup is in
  front brings it back first (`visibilitychange` says so).
- A download is heard with its name and URL, saved and read; its file is
  in its context's folder, which no other context sees and which goes
  when the context closes. One cancelled, and one whose server hangs
  up, report `canceled`.
- No forbidden method was sent in any of it.
