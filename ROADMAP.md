# Roadmap

Where Netweir is going, roughly in order. There are no dates: each piece
ships when it's done and tested, and the order can change when users show
us something more urgent. Ideas and objections are welcome in
[Discussions](https://github.com/netweir/netweir/discussions/categories/ideas).

## Next

**More engines.** Firefox and WebKit, but only once they can be driven
without announcing it, as Chrome is today. And HTTP/3, when a request can go
out over it looking exactly like the browser it claims to be.

## Shipped

0.7 finished the browser list: finding elements by their text or role, saving
a page as a PDF, tracing, and replaying a HAR file through routes. The
[release notes](https://github.com/netweir/netweir/releases/tag/v0.7.0)
have the details.

## Support

Python 3.10 reached the end of its life in October 2026. 0.5 is the last
release for it: 0.6 and later need Python 3.11 or later.

## How the order is set

What users run into comes first: a bug that loses data or a site Netweir
can't get past outranks a new feature. Then what removes a library from
people's stacks. [GOVERNANCE.md](GOVERNANCE.md) says who decides and how.
