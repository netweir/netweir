# A page's environment

A Chrome page says a lot about where it's running: the size of the
screen and the window, the language, the time zone, whether it's a phone,
where in the world it is. You can set all of that, for one page, for a
context's pages or for every page of a browser, with the names Playwright
gives `new_context()`:

```python
import netweir

async with netweir.browser() as browser:
    context = await browser.new_context(
        locale="de-DE",
        timezone_id="Europe/Berlin",
        viewport={"width": 1366, "height": 768},
        screen={"width": 1920, "height": 1080},
        color_scheme="dark",
    )
    page = await context.new_page()
    await page.goto("https://example.com/")
```

Or as a phone:

```python
page = await browser.new_page(device="Galaxy S23")
```

What matters is that every part of the page agrees. A site doesn't only
read `navigator.language`: it compares it with `Intl`, with the
`Accept-Language` header of every request, with what a worker says and
with what a frame from another site says. Netweir sets each setting
through Chrome itself, in the page, in each of its frames and in each of
its workers, so they all say the same thing. The
[design note](design/environment.md) has what was measured, and the few
things Chrome won't let anyone change.

## Settings

| Setting | Does |
|---|---|
| `device` | a preset, below: a desktop's screen, or a phone's screen, viewport, touch and identity. The other settings change it |
| `viewport` | `{"width", "height"}`: `innerWidth` and `innerHeight`. The window around it is a real one, so `outerWidth` and `outerHeight` are what a window that size has |
| `screen` | `{"width", "height"}`: `screen.width` and `screen.height`, and what media queries say about the device |
| `device_scale_factor` | `devicePixelRatio`. A whole number, except for a whole browser (see [Where to set it](#where-to-set-it)) |
| `is_mobile` | a phone's layout, with the viewport meta tag honoured. Needs a phone's `device` or `user_agent` |
| `has_touch` | a touch screen: touch events, `navigator.maxTouchPoints` and `(pointer: coarse)` |
| `locale` | such as `"de-DE"`: `Intl`, dates and numbers, `navigator.language` and `Accept-Language` |
| `timezone_id` | such as `"Europe/Berlin"`: `Intl`, `Date` and `getTimezoneOffset()` |
| `geolocation` | `{"latitude", "longitude", "accuracy"}`, for `navigator.geolocation` once the page may use it |
| `permissions` | permissions granted to every origin, such as `["geolocation"]` |
| `color_scheme` | `"light"`, `"dark"` or `"no-preference"`, for `prefers-color-scheme` |
| `reduced_motion` | `"reduce"` or `"no-preference"`, for `prefers-reduced-motion` |
| `extra_http_headers` | a dict of headers sent with every request, after Chrome's own |
| `offline` | `True` takes the page off the network: requests fail and `navigator.onLine` is false |
| `user_agent` | the running Chrome's user agent, perhaps for another system; the client hints change with it |
| `init_scripts` | scripts run in every document, before its own scripts |

Unset settings are Chrome's own. A page with no `viewport` shows 1280 by
720, or fills the screen's work area if that's smaller; a headless Chrome
Netweir starts has a 1920 by 1080 screen, which every window fits on.

## Devices

`netweir.devices` lists the presets, each as the keyword arguments it
stands for, so `browser.new_context(**netweir.devices["Galaxy S23"])` and
`browser.new_context(device="Galaxy S23")` are the same.

| Preset | Screen | Ratio | Viewport |
|---|---|---|---|
| `Desktop 1920x1080` | 1920 × 1080 | 1 | as unset |
| `Desktop 2560x1440` | 2560 × 1440 | 1 | as unset |
| `Desktop 1366x768` | 1366 × 768 | 1 | as unset: it fills the screen |
| `Desktop 1440x900 HiDPI` | 1440 × 900 | 2 | as unset |
| `Desktop 1512x982 HiDPI` | 1512 × 982 | 2 | as unset |
| `Galaxy S21` | 360 × 800 | 3 | 360 × 724 |
| `Galaxy S22` | 360 × 780 | 3 | 360 × 704 |
| `Galaxy S23` | 360 × 780 | 3 | 360 × 704 |

A desktop preset changes the screen and nothing else: the page is still
the system Chrome runs on, since its fonts and its graphics card would
say so whatever its user agent claimed.

A phone is an Android phone running Chrome, in every client hint and
every header: `Sec-CH-UA-Mobile: ?1`, the platform `"Android"`, its model
(`SM-S911B` for the S23), a phone's user agent and `navigator.platform`,
eight cores, five touch points. Clicks become taps and scrolling becomes
a finger dragging the page, since a phone has no mouse; `hover()` on a
phone raises `ValueError`, as there's nothing to hover with.

There's no Pixel and no iPhone. A Pixel's pixel ratio is 2.625 or 2.75,
and headless Chrome can only make screens with whole ratios, so a frame
from another site would see a different one. An iPhone runs Safari's
engine, which nothing about a Chrome page can pass for.

## Where to set it

- **A page**: `browser.new_page(locale=..., ...)`, a page with a context
  of its own.
- **A context's pages**: `browser.new_context(...)`, and
  `context.new_page(...)` changes it for one page. A page can't have a
  `timezone_id` or `locale` of its own in a shared context: Chrome keeps
  one of each per renderer process, and pages of one site share one, so
  `context.new_page(timezone_id=...)` raises `ValueError` unless it's the
  context's. Open the page with `browser.new_page()` instead.
- **Every page of a browser**: `netweir.browser(environment={...})`,
  with the same keys. Set here, the screen and pixel ratio are Chrome's
  own from the start, the language is Chrome's own (so it sends it
  exactly as a person's Chrome would), and the user agent reaches service
  workers, which nothing set later does. A pixel ratio that isn't a whole
  number, such as Windows' 1.25, can only be set here.

A screen, a pixel ratio or a phone needs a headless Chrome Netweir
started. A real screen can't be changed, so with `headless=False`, or a
browser you connected to, they raise `ValueError`; a `viewport` resizes
the real window.

## Changing it later

```python
await page.set_viewport_size({"width": 1024, "height": 768})
await page.emulate_media(color_scheme="dark", reduced_motion="reduce")
await page.set_extra_http_headers({"X-Team": "data"})
await page.add_init_script("window.started = Date.now()")
await page.add_init_script(path="setup.js")
await page.set_geolocation({"latitude": 51.5, "longitude": -0.12})
await page.set_offline(True)
```

`page.viewport_size` is the viewport Netweir last gave the window. A
context has `set_geolocation`, `set_offline`, `set_extra_http_headers`
and `add_init_script` too, for the pages it has and the ones it opens
later.

## Permissions and where you are

A page asking for its position goes through Chrome's own permission
check, as it would for a person: it needs the `geolocation` permission
for its origin, and a position.

```python
page = await browser.new_page(geolocation={"latitude": 48.86, "longitude": 2.35})
await page.grant_permissions(["geolocation"], origin="https://example.com")
```

`grant_permissions` sets only the permissions you name; the rest stay
what a new person's Chrome has (`"prompt"`). `clear_permissions()` takes
back what was granted. A page that asks without the permission is turned
down, as if the person had clicked Block, and the profile remembers that
for the origin.

## Init scripts

An init script runs in the page and in each of its frames, those from
other sites included, before any of the document's own scripts, in the
page's own JavaScript world. It doesn't run in workers: Chrome has no way
to run anything in a worker before its own script.

## In a crawl

`Settings.browser_environment` gives every page the crawl opens in Chrome
an environment:

```python
settings = netweir.Settings(
    browser="on_block",
    browser_environment={"device": "Galaxy S23", "locale": "pt-BR", "timezone_id": "America/Sao_Paulo"},
)
```

Chrome is started with it, as `netweir.browser(environment=...)` starts
it. When Chrome gets through a block and hands the site's cookies to the
HTTP client, the client sends the same `Accept-Language`, user agent and
client hints Chrome sent, so the site sees one visitor rather than two.

## What it can't hide

Chrome lets DevTools change a great deal, but not everything:

- **A worker's `navigator.platform`** is always the system Chrome runs
  on. A phone's page and frames say `Linux armv81`; its workers say
  `MacIntel` on a Mac.
- **A phone's window, seen from a frame from another site.** Chrome won't
  make a window narrower than 500 pixels, and such a frame reads the real
  window's size (`outerWidth`), where the page reads the phone's.
- **What the hardware says.** A phone preset still renders with the
  machine's graphics card, which WebGL names, and with its fonts. Desktop
  APIs a phone doesn't have (`navigator.serial`, `getScreenDetails`) are
  still there.
- **Service workers' user agent** follows the browser's. Set the device
  or `user_agent` for the whole browser where service workers matter.
  Their language, clock, headers and network do follow the page.
- **More than one screen.** Pages with different screens in one browser
  put Chrome's windows on screens side by side, as on a desk with two
  monitors, so `screen.isExtended` is true. One screen for the whole
  browser, set at launch, leaves it false.
- **A permission prompt** is turned down at once, where a person would
  take a moment.
