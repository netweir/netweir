# Roadmap

Where Netweir is going, roughly in order. There are no dates: each piece
ships when it's done and tested, and the order can change when users show
us something more urgent. Ideas and objections are welcome in
[Discussions](https://github.com/netweir/netweir/discussions/categories/ideas).

## Next

**HTTP/3,** when a request can go out over it looking exactly like the
browser it claims to be. Netweir's fetcher speaks HTTP/1.1 and HTTP/2,
which is what a browser falls back to when a site offers no HTTP/3.

**Firefox's gaps.** Firefox can't yet take a `proxy`, and a few
other options raise `BrowserError` on it.
[docs/browser.md](docs/browser.md#starting-firefox) lists them.

**WebKit,** once Safari can be driven without `navigator.webdriver` saying
so. No route does that today, and a WebKit that every challenge spots would
be no use to you.

## Shipped

0.8 added Firefox: `netweir.browser(engine="firefox")` drives the Firefox
`netweir install firefox` installs, and pages can't tell it's driven. 0.7
finished the browser list before it: finding elements by their text or
role, saving a page as a PDF, tracing, and replaying a HAR file through
routes. The [release notes](https://github.com/netweir/netweir/releases)
have the details.

## Support

Python 3.10 reached the end of its life in October 2026. 0.5 is the last
release for it: 0.6 and later need Python 3.11 or later.

## How the order is set

What users run into comes first: a bug that loses data or a site Netweir
can't get past outranks a new feature. Then what removes a library from
people's stacks. [GOVERNANCE.md](GOVERNANCE.md) says who decides and how.
