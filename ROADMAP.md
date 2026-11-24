# Roadmap

Where Netweir is going, roughly in order. There are no dates: each piece
ships when it's done and tested, and the order can change when users show
us something more urgent. Ideas and objections are welcome in
[Discussions](https://github.com/netweir/netweir/discussions/categories/ideas).

## Next

**Firefox's gaps.** A few options still raise `BrowserError` on Firefox,
mostly where its WebDriver BiDi has no command for them.
[docs/browser.md](docs/browser.md#starting-firefox) lists them.

**WebKit,** once Safari can be driven without `navigator.webdriver` saying
so. No route does that today, and a WebKit that every challenge spots would
be no use to you.

## Shipped

On main for the next release: HTTP/3 for the Firefox, Chrome and Safari
profiles, each moving to it when a site offers it, with that browser's
first QUIC packets ([#385](https://github.com/netweir/netweir/issues/385)).
Safari's are the closest match to its own, since its QUIC is Apple's
code and isn't public.

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
