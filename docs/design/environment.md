# Netweir page environment design

Status: implemented, October 2026
Author: James Conway

## What it is for

Playwright users set a context's viewport, locale, time zone, device and
the rest with `new_context()`'s options, and expect the same of Netweir.
For a scraper whose promise is that its pages look like a person's
Chrome, setting them is the easy half. The hard half is that a page can
read each one from many places, and detection scripts compare them:
`navigator.language` against `Intl` against the `Accept-Language` the
server got; `Date` against `Intl`'s offset for its zone; `screen` against
media queries and `devicePixelRatio`; the page against its workers and
against a frame from another site, which runs in a process of its own.
A setting that reaches some of those and not others is worse than no
setting.

So the rule is: each setting is made through Chrome's own machinery, in
every session a page has (its own, each frame's from another site, each
worker's), before that session runs anything, and nothing is patched in
by script. Where Chrome won't let a setting reach somewhere, that's said,
measured and left to the caller.

Everything below was measured on Chrome for Testing 154.0.8037.92 on a
MacBook Pro 14 with macOS 26, headless and headful, driven over the pipe
as Netweir drives it, against a page that reads every surface itself, in
a dedicated worker, in a shared worker, in a service worker, in a frame
from `localhost` (another site than `127.0.0.1`) and in that frame's
worker, and a server that records every request's headers in order. The
ground truth is Google Chrome 155 started by hand on the same Mac with
the probe's URL and no DevTools at all.

## API

Rust, in `netweir-browser`:

```rust
let env = Environment {
    locale: Some("de-DE".into()),
    timezone: Some("Europe/Berlin".into()),
    viewport: Some(Size { width: 1366, height: 768 }),
    ..Default::default()
};
let context = browser.new_context_with(env).await?;
let page = context.new_page_with(Environment::default()).await?;
let phone = browser.new_page_with(Environment::device("Galaxy S23")?).await?;
```

`LaunchOptions::environment` is every page's; `Browser::new_page_with`,
`Browser::new_context_with` and `Context::new_page_with` take one over
it. `Environment::over` merges: a page's settings over its context's
over the browser's, init scripts and permissions added, extra headers
replaced by name, and a page that says `mobile: false` takes nothing of a
phone from its context. Pages have `set_viewport`, `emulate_media`,
`set_extra_headers`, `add_init_script`, `set_geolocation`, `set_offline`,
`grant_permissions` and `clear_permissions`; a context has the ones that
make sense for all its pages.

Python takes Playwright's names (`viewport`, `screen`,
`device_scale_factor`, `is_mobile`, `has_touch`, `locale`,
`timezone_id`, `geolocation`, `permissions`, `color_scheme`,
`reduced_motion`, `forced_colors`, `contrast`, `extra_http_headers`,
`offline`, `user_agent`) plus `device` and `init_scripts`, as keyword
arguments to `new_page`, `new_context` and `BrowserContext.new_page`, and
as a dict to `netweir.browser(environment=...)` and
`Settings.browser_environment`. `viewport=None` is Playwright's
`no_viewport`. `emulate_media` takes `media`, `color_scheme`,
`reduced_motion`, `forced_colors` and `contrast`, each left as it is when
left out and taken away with `"null"`, as Playwright's does. An unknown
key is a `TypeError`; settings that contradict each other, or that
Chrome refuses, are a `ValueError` before any page exists.

## Chrome as Google ships it

Chrome for Testing isn't the Chrome people run, in two ways a page sees:

- **An infobar** saying what it is takes 56 pixels from every viewport:
  the window was 143 pixels taller than its page, where Google Chrome's
  is 87.
- **Its field trial config**, which branded Chrome doesn't carry. With
  it, page script saw one language (`["en-US"]`, where Google Chrome
  shows `["en-US", "en"]`), and a document sent `Accept-Language` after
  `sec-ch-ua-platform`; Google Chrome sends it last, after
  `Accept-Encoding`, as Netweir's HTTP profiles do.

So Chrome is started with `--disable-infobars` and
`--disable-field-trial-config`; then the frame is 87, the languages two,
and a document's headers in Google Chrome's order. Google Chrome takes
both switches as nothing to turn off: the environment and stealth tests
pass with it as the executable, and with it connected.

## The screen and the window

**Not metrics emulation.** `Emulation.setDeviceMetricsOverride` is what
Playwright and Puppeteer use for `viewport`, `screen` and
`deviceScaleFactor`. Measured with a 1280×720 viewport on a 1440×900
screen at 2:

| | page | frame from another site |
|---|---|---|
| `screen` | 1440×900 | 800×600 (the real one) |
| `devicePixelRatio` | 2 | 1 |
| `innerWidth × innerHeight` | 1280×720 | (its own size) |
| `outerWidth × outerHeight` | 756×556 (the real window) | 756×556 |

Two giveaways in one: a frame that disagrees with its page about the
screen and the ratio, and a page whose inside is bigger than its window.
The call can't be sent to a frame's session either: Chrome answers
"Command can only be executed on top-level targets". A detection
vendor's script typically runs in a frame of its own, so this matters.

**Real screens.** Headless Chrome has virtual screens, which can be
made and changed over CDP (`Emulation.addScreen`, `updateScreen`,
`getScreenInfos`) and given at launch (`--screen-info={3024x1964
label='Built-in Retina Display' devicePixelRatio=2 colorDepth=30
isInternal=true workAreaTop=66 workAreaBottom=120}`, sizes in device
pixels, the label after the size). A window put on such a screen with
`Browser.setWindowBounds` shows its size, work area, depth and ratio to
the page and to every frame alike. So:

- A browser's environment, given at launch, becomes its one screen, so
  `screen.isExtended` stays false.
- A context's or page's screen, if it differs, is added to the right of
  the others, made once and kept, and the page's window is moved onto it.
  With more than one screen in use, `screen.isExtended` is true, as on a
  desk with two monitors.
- Chrome rounds an added screen's ratio to a whole number (2.625 became
  2; `updateScreen` refuses one outright: "Invalid device pixel ratio,
  must be integral"). A ratio that isn't whole is only allowed at
  launch, where `--force-device-scale-factor` gives every screen it
  (measured: 2.625 everywhere).
- A viewport too big for its screen is refused, not moved to a bigger
  screen: that would add a second one, and turn `screen.isExtended` on
  for every page, live.

**What a screen looks like** beyond its size, measured with
`getScreenDetails()` in Google Chrome 155 on the MacBook (with the
`window-management` permission):

| | built-in | external, 2560×1440 |
|---|---|---|
| label | Built-in Retina Display | V277U |
| internal | true | false |
| colour depth | 30 | 24 |
| menu bar | 33 (beside the notch) | 30 |
| Dock | 60, on the first screen | none |

Netweir's screens follow that: a Mac's own sizes at a ratio of 2 are the
built-in display, labelled, internal and 30 bits deep; others are
external monitors named for their size (a common model's EDID name).
The menu bar comes from the macOS version: 33 beside a notch and 30
otherwise on macOS 26, measured; 37 and 25 before it, which couldn't be
measured here. The Dock is on the first screen.

On Windows, measured on a Dell XPS laptop with Windows 11 and Google
Chrome 155.0.8059.85, its display at 125%: the screen is 1536×960 at a
ratio of 1.25, the work area 1536×912 with the taskbar's 48 at the
bottom (`availTop` 0), 24 bits deep, and `getScreenDetails()` names it
`"160WUX\n"`, the panel's model, newline and all, internal. A maximised
window is the work area exactly, 1536×912 at 0, 0: no invisible border
when maximised. Netweir's Windows screens follow that: the taskbar's 48
at the bottom of every screen (40 before build 22000, Windows 10's
default, not measured), and a laptop's own panel sizes at 125% or 150%
internal and named for a panel model (`160WUX\n` for 1920×1200, as
measured, and `B156HAN02.1\n`, a common 15.6-inch 1920×1080 panel's).
The `Desktop 1536x960 125%` preset is that laptop; its ratio can only
be a whole browser's, since an added screen is rounded to a whole one.
A work area at a ratio like 1.25 is given in whole pixels rounded down,
since Chrome shows the whole CSS pixels inside it (30 at 1.25 given as
38 device pixels came out as 31). On Linux a screen is its whole: its desktops put a bar, a dock or
a panel anywhere, or nothing at all on a server, and no one shape is
more like a person's.

**Real windows.** The viewport is the window's inside: the window is
set to the viewport plus its frame, measured once per page as the
window's bounds less `Page.getLayoutMetrics`' layout viewport (0 across
and 87 down, as Google Chrome's). `outerWidth`, `outerHeight`, `screenX`
and `screenY` are then what a window that size has, in the page and in
its frames. Every page gets its window sized, the default too: Chrome
gives a new window the size of the last one, so one page's viewport
would otherwise become the next one's. A `--window-size` switch is every
page's window unless the environment names a viewport, made to fit the
work area as Chrome makes its own; `viewport=None` leaves the window as
Chrome opens it. Known limit: a restored (not maximised) window on
Windows is said to have an invisible border, which would make
`outerWidth` 16 more than `innerWidth`; a maximised one has none, as
measured, and a headless window has none either way. The restored
window's border and the frame's height on Windows are still to be
measured.

Headless Chrome on Linux takes `Browser.setWindowBounds` in the device
pixels of the screen the window is on and gives them back in those of
the screen it lands on. Measured with Google Chrome 155.0.8059.84 on
Ubuntu 24.04: a window on the 1920×1080 screen at ratio 1, asked to be
1200×787 at x 1920 on an added 1440×900 screen at ratio 2, came out
600×394 at x 960; moved back, it came out twice the size. Asked again
from the screen it's now on, it's where it was asked to be, so Netweir
reads the bounds back after setting them and sets them once more when
they differ. macOS takes them in CSS pixels throughout.

**A default that fits.** Headless Chrome's own screen is 800×600, while
the windows it opens for new contexts are 1280×600: `outerWidth` bigger
than `screen.width`, which no real browser shows. A headless Chrome
Netweir launches gets a 1920×1080 external screen, and each page a
1280×720 viewport where that fits, the window filling the work area
where it doesn't. A caller's own `--screen-info` is left alone.

**Headful and connected browsers** have real screens nobody can change,
so a screen, ratio or phone is refused there; a viewport resizes the
real window.

## Phones

A phone needs `Emulation.setDeviceMetricsOverride` with `mobile: true`:
nothing else makes Chrome lay a page out as a phone (the viewport meta
tag, a 980-pixel layout without one, overlay scrollbars). It's sent for
phones only, and the screen under it is a real one of the same size and
ratio, so frames from other sites read what the page reads. The window is
set to the viewport itself, since a phone's browser has no frame around
the page: the page reads `outerWidth` 360, as Chrome's own device mode
gives it. `positionX` and `positionY` in the metrics can't put the
page's screen where an added screen is: Chrome answers "View position
should be on the screen" for anything past the emulated screen's width.

The rest of the identity goes everywhere:

- `Network.setUserAgentOverride` with Chrome's reduced Android user agent
  (`Linux; Android 10; K`, `Chrome/154.0.0.0 Mobile`), the running
  Chrome's own brands and full versions, `platform: "Android"`, the
  model, `platformVersion`, empty architecture and bitness,
  `formFactors: ["Mobile"]`, `mobile: true`, and `navigator.platform`
  `Linux armv81`, in the page's session and each frame's and dedicated
  worker's. The low-entropy hints (`Sec-CH-UA-Mobile: ?1`,
  `Sec-CH-UA-Platform: "Android"`) went with the document, its
  subresources and the frame's document; the model with what a page
  that asked for it (`Accept-CH`) loaded next. Chrome sends no client
  hints with a worker's requests, phone or not.
- `Emulation.setTouchEmulationEnabled` in the page's session and each
  frame's: set in the page's alone, the frame said `maxTouchPoints` 0
  and `(pointer: fine)`. Touch emulation makes the device touch-only
  (`pointer: coarse`, `hover: none`, no fine pointer), and
  `setEmulatedMedia` silently leaves `pointer` and `hover` alone, so a
  desktop with `has_touch` (a touch laptop has a fine pointer too) is
  refused, as is a phone without it.
- `Emulation.setHardwareConcurrencyOverride` in every session: set in
  the page's alone, its worker and frame said 12 cores while it said 8.
- Input is a finger's. A click is a tap (`Input.dispatchTouchEvent`,
  down, 40 to 110 ms, up, with a fingertip 8 to 14 pixels across, a
  little different each time and the same from landing to lifting),
  with no mouse brought over the element first, so the page sees
  `pointerdown` with `pointerType` `touch`, `touchstart`, `touchend`
  and a `click` from a touch. (Chrome gives the lifted touch's
  `pointerup` a width of 1 however the tap is sent, `touchEnd` with its
  point or without, or `synthesizeTapGesture`.) Scrolling is a finger
  dragging the page, as touch events. It lands near the far edge of
  what it drags (the box that scrolls under the point, or the screen),
  a few pixels off from last time, and moves every 16 ms by the clock:
  past Chrome's 15-pixel slop (a tap's wobble) in its first move, then
  speeding up and slowing down over 60 ms plus a quarter of a
  millisecond a pixel, drifting a pixel or two sideways, its fingertip
  growing as it presses. It holds still 80 to 120 ms and lifts, so the
  page doesn't fling on (lifted without the hold, a 300-pixel drag went
  380). While it holds, it reads where the page got to and moves the
  last few pixels: Chrome carries a fast drag about 6 pixels past the
  finger at scale 1. The moves are sent without waiting for each answer,
  since waiting for each made them 33 ms apart. A drag too long for the
  box goes as several, each kept inside it, so a finger on a 100-pixel
  box at the top of the page never leaves the screen; a box with no
  room for the slop isn't dragged. Touch points are in the page's CSS
  pixels, so on a page zoomed out to fit (no viewport meta tag, a scale
  of about 0.37 on a Galaxy) the slop and the margins are divided by
  the visual viewport's scale. Measured on a Galaxy S23, to the bottom
  of a 15000-pixel page: 13.9 s, where `synthesizeScrollGesture` took
  15.5 and a desktop's wheel 6.1; 600 pixels up or down lands within 5
  pixels (the test's bound), zoomed or not, and stays there. The page gets touches
  and no `wheel` events, and its pointer is a fingertip's size all the
  way; `Input.synthesizeScrollGesture`, used before, gave it a width
  of 1. A key pressed is what an
  on-screen keyboard sends: `keydown` and `keyup` "Unidentified" with
  `keyCode` 229, no `keypress`, and the text as `input`
  (`Input.insertText`). `hover()` is refused.

A browser launched as a phone also has no PDF viewer, as a phone's
Chrome has none (`plugins.always_open_pdf_externally`): its
`navigator.plugins` and `mimeTypes` are empty and `pdfViewerEnabled` is
false. Its `--user-agent` switch is the phone's, so its service and
shared workers say the phone too.

Only presets with whole pixel ratios are offered (the Galaxy S21, S22
and S23 are 3), since an added screen can't have any other. Phones
aren't offered as iPhones: those run WebKit, which a Chrome page can't
pass for.

Known limits, measured, that the reverse check asserts are the only
findings for a phone:

- **Desktop APIs** a phone's Chrome hasn't are there: `navigator.hid`,
  `navigator.serial`, `navigator.keyboard`, `showOpenFilePicker`,
  `getScreenDetails` and `screen.isExtended`; and `navigator.connection`
  has no `type`, which only a phone's has.
- **A worker's `navigator.platform`** stays the system's (`MacIntel`):
  the override's `platform` doesn't reach a worker,
  `Emulation.setUserAgentOverride` doesn't either, and
  `Emulation.setNavigatorOverrides` is "only supported for pages, not
  workers".
- **A window narrower than 500 pixels** isn't made: a 360-pixel phone's
  window is 500 wide, which a frame from another site reads as its
  `outerWidth`.
- **The machine shows through** where CDP can't reach: WebGL names the
  machine's graphics card, and the fonts are the machine's.

And for a phone given to one page or context, besides: a screen of its
own beside the browser's (so `screen.isExtended` is true), the
browser's PDF viewer, and service and shared workers that keep the
browser's user agent. Netweir warns of these once, and the docs point to
the browser-level phone.

## Language

`Emulation.setLocaleOverride` sets `Intl`, dates and numbers;
`Network.setUserAgentOverride`'s `acceptLanguage` sets
`navigator.language`, `navigator.languages` and the header. Set in the
page's session alone, the worker had the locale (its process's) but said
`navigator.language` `en-US` and sent `Accept-Language: en-US,en;q=0.9`;
the frame from another site had neither. Set in every session, every
surface and every request agreed.

What the list is matters. Chrome's own header for a German speaker is
`de-DE,de;q=0.9`; an override of `de-DE` alone sent `de-DE`, which no
person's Chrome sends, so a locale goes over as `de-DE,de`, which Chrome
sends as `de-DE,de;q=0.9`.

**Chrome's own languages differ by context.** With no locale given,
Chrome is left its own: the machine's, as its `Intl` is (giving it
`en-US,en` always, as an earlier version did, showed `navigator.language`
`en-US` beside an `Intl` in `de-DE` on a German machine); with field
trials off, an English machine's are `["en-US", "en"]`, as Google
Chrome's. Launched with `--accept-lang`, Chrome's own context shows that
list, but a context
made over CDP (as every `new_page` and `new_context` is) adds its own UI
language for some lists, the same in page and worker: `de-DE,de` became
`de-DE,de,en-US,en`, `en-GB,en` became `en-GB,en-US,en`, while `fr-FR`
and `ja` stayed as they were. So the languages Chrome shows with no
override are read once for each kind of context, from a new page on
`about:blank` before its override, and a page whose locale is the
browser's (or none) keeps those exactly: no `acceptLanguage` goes in its
override, and its workers are left to Chrome's own. An override's
`Accept-Language` goes out after `User-Agent`, where Chrome's own is last,
so leaving Chrome's to it is what keeps every header in its place.

**A page's or context's own locale** can only be had through the
override, so its `Accept-Language` is out of place in every request
(after `User-Agent`, or after `Accept` for a worker's script), and its
service worker's own script goes with Chrome's language, since Chrome
fetches it from no session. Both are documented, and the browser-level
locale is the one to use where they matter.

Chrome keeps one locale per renderer process: a second session in a
process answers "Another locale override is already in effect". A worker
shares its frame's process, and so does a popup `window.open` makes
(which was being closed over it); that answer means the locale is in
place. Pages of a context can share a process too, so a page in a shared
context, or of a kept profile, can't have a locale or time zone of its
own; it's refused, and `Browser.new_page`, with a context per page, is
the way.

## Worker scripts' headers

The user agent override every page has (it puts back the high-entropy
client hints the `--user-agent` switch empties) has the renderer set
`User-Agent` on a dedicated, module or shared worker's script request
itself, which puts it early among the headers: `Accept`, `User-Agent`,
`Sec-Fetch-Site`, where Google Chrome sends `Accept`, `Sec-Fetch-*`,
`Referer`, `User-Agent`, `Accept-Encoding`, `Accept-Language`. Every
headless browser Netweir has launched since its first release sent it
so. Order honoured in `Fetch.continueRequest`'s headers it isn't: Chrome
kept `User-Agent` where it was. Left out, though, Chrome adds it back in
its own place.

So, when `worker_header_order` is on, each session with an override has
the Fetch domain pause requests of type Other, as part of the patterns
routes and the guard already need, so nothing is paused twice. Only a
worker's script is changed: Other, no client hints, no `Origin` (a
module a worker imports has one, and comes with `User-Agent` first as
Chrome sends it), `Accept: */*`, and `Sec-Fetch-Dest` worker or
sharedworker where the request has one. On a page that isn't a secure
context (plain http, not loopback) a favicon's request is Other with no
client hints and no Sec-Fetch headers too; its `Accept` tells it apart,
and its `User-Agent` stays first, as Chrome sends it. A worker's script
goes on without its `User-Agent` and `Accept-Language` where their
values are the ones Chrome adds itself (the switch's, and its own
languages), and Chrome puts them back. What a route decides goes on with
those headers left out too, unless it gives others. The stealth tests
check every kind of worker script, what each imports, and a page that
isn't a secure context, against Google Chrome's order. Where a value
differs from the switches (one page's user agent or locale), it can't be
left out, and stays where the override put it.

**It's off by default, because of what it costs.** Any `Fetch.enable`
puts every request of the session through the interceptor, whatever the
patterns say, and a frame's session waits for its Fetch setup before
it's let go. Loading one page at a time, alternating, 15 times each:

| page | off | on |
|---|---|---|
| 300 subresources | 93 ms | 118 ms |
| 300 subresources, 30 dedicated and 4 shared workers, 12 frames (half from another site) with 2 workers each | 200 ms | 292 ms |

A lighter way was tried first: turning the pattern on only once a page
starts a worker. It can't work. A dedicated worker's script is asked
for before its target is attached and held (the server had the request
10 ms before the session heard of the worker), and so are a module
worker's and a shared worker's; by the time a session could turn Fetch
on, the request has gone. So it's `worker_header_order` (on
`LaunchOptions`, `netweir.browser()` and, for a crawl,
`Settings.browser_worker_header_order`), for those who'd rather pay than
leave the order changed, and the leak is said plainly in the guide.

## Shared and service workers

**Shared workers** belong to no page, so no page's session is told of
one. The browser's session holds them (`Target.setAutoAttach` with a
`shared_worker` filter) and gives each the environment of a page of its
context before it runs, as a dedicated worker gets its page's.

**Service workers and shared workers take their user agent from the
`--user-agent` switch alone**; no override reaches it. With the page's
whole override, one page's phone gave such a worker a desktop
`navigator.userAgent` and Android client hints. So they get the
switch's user agent with the page's languages and the rest of its
environment, and say one thing; a browser's phone is in the switch, and
they say it too.

**Overrides go with the session.** Commands sent to a held service
worker's session aren't answered until it runs. Detached after its
overrides, a service worker went back to `navigator.language` `en-US`
and Chrome's locale while keeping the zone. So when a page's environment
has a locale, zone, headers, network or phone, its service workers'
sessions get the overrides (sent, not awaited) before they're let go,
and are kept. Known limit: kept, a service worker never idles out
(measured: the same worker answered after 45 seconds idle, where one let
go was started afresh), which a worker timing its own life could see.
What a kept service worker fetches goes with the page's language and
headers; its own script doesn't (see Language).

A service worker that was let go is out of reach of the page's session:
set offline, its `navigator.onLine` said false but its fetch went
through. Going offline attaches each such worker of the page's context,
takes it offline too, and lets it go when the page comes back online.

## Time zone

`Emulation.setTimezoneOverride`, in every session: the page's alone gave
its worker the zone (process-wide) but not the frame from another site,
which said `America/Denver` and `GMT-0700` while the page said
`Asia/Tokyo` and `GMT+0900`. In every session, `Intl`,
`getTimezoneOffset()` in winter and summer, and `Date`'s string agreed
everywhere. A zone Chrome doesn't know is refused as a `ValueError`.

## Geolocation and permissions

`Emulation.setGeolocationOverride` in the page's session reaches its
frames: a frame from another site allowed `geolocation` got the same
position. The position is only given through the permission a person
would grant:

- `Browser.grantPermissions` sets every permission it isn't given to
  denied: after granting geolocation, `notifications` read `denied` and
  `Notification.permission` `denied`, where a new person's Chrome says
  `prompt` and `default`. Netweir sets each with `Browser.setPermission`
  instead, which leaves the rest alone.
- `Browser.resetPermissions` takes back what was set. A page that asked
  without the permission was turned down at once (`PERMISSION_DENIED`),
  and the origin then read `denied`, which a reset doesn't clear: it's
  what headless Chrome records, as for a person who clicked Block. A
  person takes a moment to click, which can't be had without a prompt
  to answer.

## The rest

- **Media**: `Emulation.setEmulatedMedia`, with the media type and the
  colour scheme, reduced motion, forced colours and contrast features,
  in the page's session and each frame's (workers have no media queries;
  Chrome says "only supported for pages"). Each call carries every
  emulation the page has, so taking one away is sending the rest.
- **Extra headers**: `Network.setExtraHTTPHeaders` in every session; set
  in the page's alone, the worker's requests went without them.
- **Offline**: `Network.emulateNetworkConditionsByRule` with `offline`
  fails requests but leaves `navigator.onLine` true;
  `Network.overrideNetworkState` with `offline` sets `onLine` false but
  lets requests through. Both, in every session, make a page whose fetch
  fails while it says it's offline, which is what a person's offline
  Chrome shows. Turned off, `overrideNetworkState` with no limits left
  `navigator.connection` as Chrome reports it.
- **Init scripts**: `Page.addScriptToEvaluateOnNewDocument`, in the main
  world, in the page's session and each frame's. A frame's session needs
  the Page domain for it (without, the script didn't run there), so a
  frame's session gets `Page.enable` when there are init scripts, and
  its `javascriptDialogOpening` events, which the page's session hears
  too, are ignored. Workers can't have one: `Runtime.evaluate` sent to a
  held worker isn't answered until it runs, and sent before
  `runIfWaitingForDebugger` it held the worker, and the page's load,
  for good.
- **User agent**: the running Chrome's own, perhaps for another system,
  with client hints made to match (Windows' platform, architecture
  `x86` and `navigator.platform` `Win32`, say). One naming another
  Chrome version, or `HeadlessChrome`, is refused: the brands, full
  versions and features would say otherwise.
- **Hand-back**: a crawl's Chrome hands a site's cookies to an HTTP
  session using the profile of the same Chrome. That profile now sends
  the `Accept-Language`, user agent, `Sec-CH-UA-Mobile` and
  `Sec-CH-UA-Platform` the page sent, in the places the profile sends
  them, so the cookies come back with the same visitor's headers.

## Found on the way

- **`uaFullVersion` was empty** in every headless page, and so was
  `Sec-CH-UA-Full-Version` when a site asked for it, since the driver's
  first launch: the client hints Netweir reads from Chrome left it out.
  Deprecated, it's still a hint Chrome gives; it's read now, and set to
  the full version where the brands are Netweir's.
- **Going offline with a dedicated worker running** warned every time:
  a worker's session answers `Network.overrideNetworkState` "Not
  supported", and the worker's `navigator.onLine` follows its page. It
  goes to pages and frames alone now.

- **`outerWidth` is 0 during the first parse** of a document Chrome has
  just put in a new process by a browser-started navigation
  (`Page.navigate`), headless and headful alike; once loaded, it's the
  window's. A navigation the page starts itself reads it straight away.
  It's Chrome's, not something to fix.

## Testing

`crates/netweir-browser/tests/environment.rs` reads every surface from
the page, its dedicated and shared workers, a frame from another site
and that frame's worker, and every request's headers on the server, for
a desktop environment, for a phone, for a context's pages and for
browsers launched with one or as a phone; then a page runs the tricks
detection scripts use (Intl's offset against `Date`'s, the zone in
`Date`'s string, the screen against `device-width` and `resolution`,
the window against the screen, touch against the pointer and the client
hints, the client hints against the user agent, the page against its
workers and frames, a phone's desktop APIs, and whether any getter is a
script's rather than native). It finds nothing on a desktop, and only
the limits above on a phone. Others cover the Mac's screens, a
`--window-size`, a viewport too big for its screen, geolocation through
the permission, offline in a page, its worker and a service worker it
let go, settings changed later, service workers, taps, touch scrolling
and typing, a phone's popup and a locale's same-process popup from their
first script, a frame's dialog told once, media, and what's refused.
`crates/netweir-browser/tests/stealth.rs` holds the frame, languages and
header order of documents, stylesheets and every kind of worker script
against Google Chrome's. `tests/test_environment.py` does the same from
Python, and a crawl's; `crates/netweir-core/tests/browser.rs` checks a
hand-back sends what Chrome sent.

## Milestone

**M50, a page's environment.** Viewport, screen, ratio, phone and touch,
locale and language, time zone, geolocation and permissions, media,
extra headers, offline, user agent and init scripts, per browser,
context and page, from Rust, Python and a crawl, with device presets;
and Chrome started as Google ships it, with worker scripts' headers in
its order.
