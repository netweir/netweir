# XML in the HTML tree

Netweir parsed everything as HTML, with lexbor, and that made feeds
unreadable: HTML's `<link>` is a void element, so an RSS item's URL
disappeared, names were lowercased, and prefixed XPath names were a parse
error. This note is how XML got in, and why it went where it did. The
user-facing description is in [xml.md](../xml.md).

## One tree, one XPath engine

The choice was between building XML documents as lexbor trees, so
everything already written works on them, and a separate XML tree with a
second XPath engine (or one made generic over both trees).

What a lexbor tree brings along, unchanged or nearly:

- The XPath engine (`xpath/`, about 2,200 lines) and its document index,
  which make `//x[pred]` a scan over arrays rather than a walk.
- CSS through lexbor's selector engine, with Scrapy's `::text` and
  `::attr()` on top (`query.rs`).
- The Python `Node` and `Selection`, every parsel and Beautiful Soup
  method on them, tree editing and its reader/edit locking, tracked
  selectors, `Item`s and `Follow` rules in Rust.

A second tree would have needed all of that again, or the XPath engine
rewritten against a trait, plus a CSS engine for the new tree. So XML is
read by a Rust tokenizer and built into a lexbor document node by node,
through a few new functions in `csrc/shim.c`. The evidence it holds up is
in `tests/xml.rs` and `tests/test_xml.py`, which compare answers with
parsel's on RSS, Atom and SOAP: names, attributes, namespaces, `name()`,
`local-name()`, `namespace-uri()`, and `get()` down to the order of the
namespace declarations. A 7.8 MB, 50,000-URL sitemap parses and answers
`//s:loc/text()` in about the time parsel takes (107 ms against 87 to
100 ms on one machine).

### What lexbor couldn't keep

lexbor is an HTML library, and three of its habits are wrong for XML.

**It lowercases tag ids.** Every element has a tag id, from the
lowercased name; the index and CSS compare those. An XML element gets the
same lowercased id, which keeps the index and CSS working, and its name
as written goes in lexbor's `qualified_name` (`prefix:local`), which
lexbor keeps in case. XPath uses the id to narrow its scan and then
compares the name exactly, so `//Price` and `//price` differ. CSS type
selectors keep lexbor's comparison, which ignores case; the docs say so.

**It lowercases namespace URIs.** `lxb_ns_append` interns them with
`lexbor_hash_insert_lower`, and XML compares URIs as written
(`http://www.w3.org/2001/XMLSchema-instance` has capitals). So the
namespaces live on the Rust side, in `XmlNames`: interned URIs, and a map
from node (element or attribute) to URI id. Nodes in no namespace,
almost every node of an RSS feed, have no entry. lexbor's own namespace
field stays empty, so its HTML-specific code never mistakes an XML
element for an HTML one.

**It keeps one `id` attribute per element.** `lxb_dom_element_attr_append`
drops an earlier attribute whose lowercased local name is `id` (or
`class`), and XML may have `id` and `ID`, or `id` and `xml:id`. Only an
attribute named exactly `id` or `class` gets lexbor's own id, which its
`#id` and `.class` matching needs; any other attribute's id is its name
as written. Attribute lookup by name in XML compares the written name in
Rust, since lexbor's would also match `x:href` for `href`.

Namespace declarations aren't attributes in XPath's data model, so they
aren't lexbor attributes either: `XmlNames` keeps each element's in
source order, for writing the element back out. HTML documents carry no
`XmlNames` at all, and each XML branch starts by checking for them.

### Writing XML back out

`get()` on an element writes it as lxml does, and the details were
settled by asking lxml rather than guessing (the cases are in the
tests): an element taken from a document declares its own namespaces,
then those its name and attributes are in, then everything else in
scope, nearest ancestor first, each prefix once. Empty elements are
`<x/>`; text escapes `&`, `<`, `>` and carriage returns, attribute values
those and `"`, tabs and newlines. Writing is iterative, so the deepest
document allowed can't overflow a stack.

## The tokenizer: quick-xml

quick-xml was already in the workspace, reading sitemaps in
netweir-core, so XML adds no dependency. It is actively maintained, and
it suits a trust boundary for two reasons: it never expands an entity
itself (references arrive as their own events), and it never reads
anything but the string it's given. Everything a hostile document could
abuse is therefore in netweir's code, where it's bounded:

- **Entities.** The internal subset's general entities are read from the
  DOCTYPE (`declared_entities`); external ones are recorded as external,
  and using one is an error, so nothing is ever fetched or opened.
  Parameter entities aren't expanded, and as XML asks of a processor
  that doesn't read them, declarations after one aren't read. An
  expansion runs the replacement text through the same builder, so
  markup in an entity becomes nodes, at most 16 entities deep, refusing
  one that refers to itself, and within a budget of 1 MiB plus 8 bytes
  per byte of the document for everything expanded, in text and in
  attribute values. The budget is what stops both the "billion laughs"
  and the flat "quadratic blowup" (one large entity used thousands of
  times), which a depth limit alone wouldn't.
- **Depth.** Elements nest at most 2048 deep, lxml's limit with
  `huge_tree`, which Scrapy's selectors turn on.
- **Time.** The builder checks the parse budget every 1024 events, as the
  HTML parser checks between chunks.

Expanding declared entities differs from parsel, which leaves them as
references (`resolve_entities=False`): `text()` misses them there and
`string()` has them. Expanding within bounds gives what the document
says, safely.

## Strict, not recovering

parsel parses XML with lxml's `recover=True`. A document that breaks
part way is cut off at the break, and the selector shows the half it
read, with no error; a stray `&nbsp;` is dropped. For a scraper that
means silently missing items. Netweir's parser stops at the first
well-formedness error with an `XmlError` giving the line, the column (in
characters) and what's wrong, and nothing in the tree is invented. In a
crawl, the engine still hands the page to the callback, where reading
its document raises the error and `page.text` is there to repair. That
costs some leniency on broken feeds; the docs show the usual repair.

## Namespaces on queries

parsel keeps registered namespaces on each selector, copying them into
the selectors its queries return. Netweir does the same: each Python
`Node` holds an `Arc` of `(prefix, uri)` pairs, copied into every node
and selection it gives, and `register_namespace()` replaces its own
copy. Nodes found before a registration don't see it, as in parsel.
`namespaces=` on one query is for that query alone. A document-wide
registry would have been less code, and wrong in the cases where parsel
users rely on a selector's prefixes staying its own.

The XPath parser now accepts `prefix:name` and `prefix:*`; prefixes are
resolved when the query runs, against the namespaces it's run with
(`XPath::run_in`). An unbound prefix is an error naming the fix rather
than an empty result. In HTML, where nothing has a namespace, a bound
prefix matches nothing.

`remove_namespaces()` is an edit: it waits for readers like any other,
renames elements and attributes in place (lexbor keeps the old name
strings, so a `&str` borrowed earlier still reads what it read), and
clears `XmlNames`' node maps but not its URIs, for the same reason.

## One node at a time

`XmlNodes` is Scrapy's `iternodes`: it reads the document once, and each
element with the wanted name becomes a document of its own, handed out
as soon as it ends, so memory is one node's worth however big the feed.
The whole document is still checked as it's read; a break ends the
iteration with the error, after every node before it. The top element of
each node keeps the declarations it had in scope, so it's written out as
it would be from the whole document. Reading resumes from a byte offset
with a fresh quick-xml reader, which keeps the iterator free of
self-borrowing; end tags are checked against the builder's own stack for
that reason.

Each node costs a lexbor document, about 3.5 µs to make, which is most
of the 270 ms 50,000 nodes take. Scrapy's `xmliter_lxml` took 1.2 s on
the same feed. lexbor can make a document that borrows another's
memory, which would be cheaper, but then no node's memory is returned
until the last is dropped, which defeats reading one at a time.

## Sitemaps

Sitemaps were read by a quick-xml loop of their own, which left
declared entities unexpanded and knew nothing of the rest. They now go
through `Document::parse_xml`, with its limits, and the entries are read
off the tree by local name, in any case and namespace, as before. That
holds a sitemap's whole tree at once, as Scrapy's `Sitemap` does with
lxml; it was faster than reading entry by entry (150 ms for the 50,000
URLs, where the per-node documents above would add 180 ms), and a
sitemap's size is capped by `max_response_size` anyway.

## Known limits

- CSS type selectors on XML ignore namespaces and case.
- The `namespace::` axis gives nothing.
- DTD attribute defaults aren't applied (nor are they by lxml as Scrapy
  sets it up).
