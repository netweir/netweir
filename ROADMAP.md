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

0.6 brought what used to head this list: five more page types (places,
property, social posts, search results and any page's main content), chunks
for retrieval with converters for LangChain and LlamaIndex, extraction by
a language model on your own key, and an MCP server. The
[release notes](https://github.com/netweir/netweir/releases/tag/v0.6.0)
have the details.

## Support

Python 3.10 reached the end of its life in October 2026. 0.5 is the last
release for it: 0.6 and later need Python 3.11 or later.

## How the order is set

What users run into comes first: a bug that loses data or a site Netweir
can't get past outranks a new feature. Then what removes a library from
people's stacks. [GOVERNANCE.md](GOVERNANCE.md) says who decides and how.
