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
media queries and `devicePixelRatio`; the page against its worker and
against a frame from another site, which runs in a process of its own.
A setting that reaches some of those and not others is worse than no
setting.

So the rule is: each setting is made through Chrome's own machinery, in
every session a page has (its own, each frame's from another site, each
worker's), before that session runs anything, and nothing is patched in
by script. Where Chrome won't let a setting reach somewhere, that's said,
measured and left to the caller.

Everything below was measured on Chrome for Testing 154.0.8037.92 on
macOS, headless and headful, driving it over the pipe as Netweir does,
with a page that reads every surface itself, in a dedicated worker, in a
frame from `localhost` (another site than `127.0.0.1`) and in that
frame's worker, and a server that records every request's headers.

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
replaced by name. Pages have `set_viewport`, `emulate_media`,
`set_extra_headers`, `add_init_script`, `set_geolocation`,
`set_offline`, `grant_permissions` and `clear_permissions`; a context
has the ones that make sense for all its pages.

Python takes Playwright's names (`viewport`, `screen`,
`device_scale_factor`, `is_mobile`, `has_touch`, `locale`,
`timezone_id`, `geolocation`, `permissions`, `color_scheme`,
`reduced_motion`, `extra_http_headers`, `offline`, `user_agent`) plus
`device` and `init_scripts`, as keyword arguments to `new_page`,
`new_context` and `BrowserContext.new_page`, and as a dict to
`netweir.browser(environment=...)` and `Settings.browser_environment`.
An unknown key is a `TypeError`; settings that contradict each other,
or that Chrome refuses, are a `ValueError` before any page exists.

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
`getScreenInfos`) and given at launch
(`--screen-info={3024x1964 devicePixelRatio=2 workAreaTop=76}`, sizes in
device pixels). A window put on such a screen with
`Browser.setWindowBounds` shows its size, work area and ratio to the
page and to every frame alike: with a 1440×900 screen at 2 added, the
page and its frame from another site both read 1440×900 and 2. So:

- A browser's environment, given at launch, becomes its one screen
  (`--screen-info`), so `screen.isExtended` stays false.
- A context's or page's screen, if it differs, is added to the right of
  the others, made once and kept, and the page's window is moved onto it.
  With more than one screen in use, `screen.isExtended` is true, as on a
  desk with two monitors.
- Chrome rounds an added screen's ratio to a whole number (2.625 became
  2; `updateScreen` refuses one outright: "Invalid device pixel ratio,
  must be integral"). A ratio that isn't whole is only allowed at
  launch, where `--force-device-scale-factor` gives every screen it
  (measured: 2.625 everywhere).

**Real windows.** The viewport is the window's inside: the window is set
to the viewport plus its frame, measured once per page as the window's
bounds less `Page.getLayoutMetrics`' layout viewport (0 across and 143
down in headless Chrome, the same as headful). `outerWidth`,
`outerHeight`, `screenX` and `screenY` are then what a window that size
has, in the page and in its frames. Every page gets its window sized,
the default too: Chrome gives a new window the size of the last one, so
one page's viewport would otherwise become the next one's.

**A default that fits.** Headless Chrome's own screen is 800×600, while
the windows it opens for new contexts are 1280×600: `outerWidth` bigger
than `screen.width`, which no real browser shows. A headless Chrome
Netweir launches now gets a 1920×1080 screen (with the menu bar's 25
pixels on a Mac and the taskbar's 48 on Windows taken from its work
area), and each page a 1280×720 viewport where that fits, the window
filling the work area where it doesn't. A caller's own `--screen-info`
is left alone.

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
gives it.

The rest of the identity goes everywhere:

- `Network.setUserAgentOverride` with Chrome's reduced Android user agent
  (`Linux; Android 10; K`, `Chrome/154.0.0.0 Mobile`), the running
  Chrome's own brands and full versions, `platform: "Android"`, the
  model, `platformVersion`, empty architecture and bitness,
  `formFactors: ["Mobile"]`, `mobile: true`, and `navigator.platform`
  `Linux armv81`, in the page's session and each frame's and worker's.
  The low-entropy hints (`Sec-CH-UA-Mobile: ?1`,
  `Sec-CH-UA-Platform: "Android"`) went with the document, its
  subresources and the frame's document; the model with what a page
  that asked for it (`Accept-CH`) loaded next. Chrome sends no client
  hints with a worker's requests, phone or not.
- `Emulation.setTouchEmulationEnabled` in the page's session and each
  frame's: set in the page's alone, the frame said `maxTouchPoints` 0
  and `(pointer: fine)`.
- `Emulation.setHardwareConcurrencyOverride` in every session: set in
  the page's alone, its worker and frame said 12 cores while it said 8.
- Input is a finger's. A click is a tap (`Input.dispatchTouchEvent`,
  down, 40 to 110 ms, up), with no mouse brought over the element first,
  so the page sees `pointerdown` with `pointerType` `touch`,
  `touchstart`, `touchend` and a `click` from a touch. Scrolling is
  `Input.synthesizeScrollGesture` from a touch, without a fling, so the
  page gets touches and no `wheel` events. `hover()` is refused.

Only presets with whole pixel ratios are offered (the Galaxy S21, S22
and S23 are 3), since an added screen can't have any other. Phones
aren't offered as iPhones: those run WebKit, which a Chrome page can't
pass for.

Known limits, measured:

- **A worker's `navigator.platform`** stays the system's (`MacIntel`):
  the override's `platform` doesn't reach a worker,
  `Emulation.setUserAgentOverride` doesn't either, and
  `Emulation.setNavigatorOverrides` is "only supported for pages, not
  workers".
- **A window narrower than 500 pixels** isn't made: a 360-pixel phone's
  window is 500 wide, which a frame from another site reads as its
  `outerWidth`.
- **The machine shows through** where CDP can't reach: WebGL names the
  machine's graphics card, the fonts are the machine's, and desktop-only
  APIs (`navigator.serial`, `getScreenDetails`) are there.

The reverse-check test asserts that these are the only findings for a
phone.

## Language

`Emulation.setLocaleOverride` sets `Intl`, dates and numbers;
`Network.setUserAgentOverride`'s `acceptLanguage` sets
`navigator.language`, `navigator.languages` and the header. Set in the
page's session alone, the worker had the locale (its process's) but said
`navigator.language` `en-US` and sent `Accept-Language: en-US,en;q=0.9`;
the frame from another site had neither. Set in every session, all four
surfaces and all seven requests (document, stylesheet, worker script,
the worker's fetch, the frame's document, its worker's script and fetch)
agreed.

What the list is matters. Chrome's own header for a German speaker is
`de-DE,de;q=0.9`; an override of `de-DE` alone sent `de-DE`, which no
person's Chrome sends, so the locale goes over as `de-DE,de`, which
Chrome sends as `de-DE,de;q=0.9`. Chrome 154 by itself shows page script
only the first language (`navigator.languages` is `["de-DE"]`) while an
override shows both; and the override moves `Accept-Language` earlier
among a worker's request headers. Neither can be had another way per
context: an extra `Accept-Language` header doesn't touch navigations.
For the whole browser it can: Chrome launched with
`--accept-lang=de-DE,de` does exactly what a person's does (one language
to script, the full list in workers' headers), so a browser's locale is
given that way and no `acceptLanguage` override is sent for it.

Chrome keeps one locale per renderer process: a second session in a
process answers "Another locale override is already in effect". A
worker shares its frame's process, so that answer is expected there and
ignored. Pages of a context can share a process too, so a page in a
shared context can't have a locale or time zone of its own; it's
refused, and `Browser.new_page`, with a context per page, is the way.

## Time zone

`Emulation.setTimezoneOverride`, in every session: the page's alone gave
its worker the zone (process-wide) but not the frame from another site,
which said `America/Denver` and `GMT-0700` while the page said
`Asia/Tokyo` and `GMT+0900`. In every session, `Intl`,
`getTimezoneOffset()` in winter and summer, and `Date`'s string agreed
everywhere. A zone Chrome doesn't know is refused as a `ValueError`.

## Service workers

A service worker takes its user agent from the `--user-agent` switch
alone; no override reaches it, which is why a browser's device or user
agent is put in the switch at launch. Its other overrides do work, with
a catch: commands sent to a held service worker's session aren't
answered until it runs, and are dropped when the session goes. Detached
after its overrides, a service worker went back to `navigator.language`
`en-US` and Chrome's locale while keeping the zone. So when a page's
environment has a locale, zone, headers, network or phone, its service
workers' sessions get the overrides (sent, not awaited) before they're
let go, and are kept, which keeps them running while the page's session
lives. Otherwise they're let go and left, as before.

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
  what headless Chrome records, as for a person who clicked Block.
  That's documented; a person takes a moment to click, which can't be
  had without a prompt to answer.

## The rest

- **Colour scheme and reduced motion**: `Emulation.setEmulatedMedia`'s
  features, in the page's session and each frame's (workers have no
  media queries; Chrome says "only supported for pages").
- **Extra headers**: `Network.setExtraHTTPHeaders` in every session; set
  in the page's alone, the worker's requests went without them.
- **Offline**: `Network.emulateNetworkConditionsByRule` with `offline`
  fails requests but leaves `navigator.onLine` true;
  `Network.overrideNetworkState` with `offline` sets `onLine` false but
  lets requests through. Both, in every session, make a page whose fetch
  fails while it says it's offline, which is what a person's offline
  Chrome shows (the deprecated `Network.emulateNetworkConditions` did
  both too). Turned off, `overrideNetworkState` with no limits left
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
  the environment's `Accept-Language`, user agent, `Sec-CH-UA-Mobile`
  and `Sec-CH-UA-Platform`, in the places the profile sends them, so the
  cookies come back with the same visitor's headers.

## Found on the way

- **`outerWidth` is 0 during the first parse** of a document Chrome has
  just put in a new process by a browser-started navigation
  (`Page.navigate`), headless and headful alike; once loaded, it's the
  window's. A navigation the page starts itself reads it straight away.
  It's Chrome's, not something to fix.
- **The user agent override moves `User-Agent`** earlier among a
  worker's script request's headers (`Accept`, `User-Agent`,
  `Sec-Fetch-*` where Chrome on its own sends `Accept`, `Sec-Fetch-*`,
  `Referer`, `User-Agent`). The override has been on every page since
  the driver's first milestone, to put back the high-entropy hints the
  `--user-agent` switch empties; that trade stands, and is noted here.

## Testing

`crates/netweir-browser/tests/environment.rs` reads every surface from
the page, its dedicated worker, a frame from another site and that
frame's worker, and every request's headers on the server, for a desktop
environment, for a phone, for a context's pages and for a browser
launched with one; then a page runs the tricks detection scripts use
(Intl's offset against `Date`'s, the zone in `Date`'s string, the screen
against `device-width` and `resolution`, the window against the screen,
touch against the pointer and the client hints, the client hints
against the user agent, the page against its worker and frames, and
whether any getter is a script's rather than native). It finds nothing
on a desktop, and only the limits above on a phone. Others cover
geolocation through the permission, offline in a page and its worker,
settings changed later, service workers, taps and touch scrolling, a
phone's popup from its first script, a frame's dialog told once, and
what's refused. `tests/test_environment.py` does the same from Python,
and a crawl's; `crates/netweir-core/tests/browser.rs` checks a hand-back
sends what Chrome sent.

## Milestone

**M50, a page's environment.** Viewport, screen, ratio, phone and touch,
locale and language, time zone, geolocation and permissions, colour
scheme, reduced motion, extra headers, offline, user agent and init
scripts, per browser, context and page, from Rust, Python and a crawl,
with device presets.
