# Roadmap

Where Netweir is going, roughly in order. There are no dates: each piece
ships when it's done and tested, and the order can change when users show
us something more urgent. Ideas and objections are welcome in
[Discussions](https://github.com/netweir/netweir/discussions/categories/ideas).

## Next

**The rest of the item schema.** `netweir.auto` reads products, listings,
jobs, articles, lists of articles and forum threads today. Next come a
business's page (address, phone, opening hours), a property for sale or to
rent, a single social media post, a search engine's results, and any page's
main content kept apart from its menus. This is in review now.

**Pages ready for retrieval.** Netweir already turns a page into clean
Markdown. Next, it splits that Markdown into chunks along the page's own
headings, each chunk knowing the headings above it and carrying an id that
stays the same from one crawl to the next, so a RAG pipeline can embed the
chunks and skip the ones that haven't changed. Netweir prepares the chunks;
it won't embed them or store vectors. That's for the tools you already use,
and there will be converters for LangChain and LlamaIndex.

**Extraction by a language model, if you want it.** For pages no type
covers: describe the fields you want, bring your own model and key, and
get them back in that shape. Nothing calls a model unless you ask it to.
Alongside it, an MCP server, so an AI agent can fetch and extract pages
through Netweir.

## After that

**More of the browser.** Modifier keys, dragging, and going back and
forward; finding elements by their text or role; saving a page as a PDF;
tracing; and replaying a HAR file through routes.

**More engines.** Firefox and WebKit, but only once they can be driven
without announcing it, as Chrome is today. And HTTP/3, when a request can go
out over it looking exactly like the browser it claims to be.

## Support

Python 3.10 reached the end of its life in October 2026. 0.5 is the last
release for it: the next needs Python 3.11 or later.

## How the order is set

What users run into comes first: a bug that loses data or a site Netweir
can't get past outranks a new feature. Then what removes a library from
people's stacks. [GOVERNANCE.md](GOVERNANCE.md) says who decides and how.
