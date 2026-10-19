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
`class`), and XML may have `id` and `ID` side by side. An XML attribute
gets lexbor's id for its whole name lowercased (`href`, `x:href`), which is
what lexbor's CSS attribute selectors look up, so `a[href]` works on XML
as on HTML and ignores case as it does there. A name that lowercases to
`id` or `class` without being written so (`ID`, `Class`) gets lexbor's id
too, so `[id]` finds it, but is linked into its element without
`lxb_dom_element_attr_append`, whose bookkeeping would make it the
element's one `id` and drop an `id` already there; `#x` and `.x` match
only the attributes named exactly `id` and `class`. Beside an exact `id`
(or `class`) on the same element, `ID` takes the id of its written name
instead, as lexbor finds only the first attribute of an id, and `[id=x]`
should read the `id` whichever comes first; lexbor lowercases the names
in selectors, so `[ID]` then means the `id` too. The first version gave
every attribute but `id` and `class` the id of its written form, which
broke `a[href]` even on XHTML served as XML, since lexbor's ids for names
HTML knows are static ones, and a second left `ID` out of `[id]`.
Attribute lookup by name in XML compares the written name in Rust, since
lexbor's would also match `x:href` for `href`.

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
- **Time.** The builder counts work, events, attributes and 64-byte
  pieces of expanded entities, and reads the clock every 1024 units, so
  a single start tag with 200,000 attributes, which is one event, still
  runs out of time. The first version counted only events; 30,000
  attributes in one tag took 26 seconds against a 100 ms budget and
  returned a document.
- **Attributes.** Duplicates are found with a hash set of names (and of
  namespace and local name, for two prefixes of one namespace), not
  quick-xml's own check, which compares every attribute with every other.
  Namespace prefixes resolve through a map of prefix to bindings, so
  thousands of declarations on one element cost thousands of steps, not
  millions. The tests hold both to linear time.

Expanding declared entities differs from parsel, which leaves them as
references (`resolve_entities=False`): `text()` misses them there and
`string()` has them. Expanding within bounds gives what the document
says, safely.

## Strict by default, recovering when asked

parsel parses XML with lxml's `recover=True`, which carries on past most
breakage (a stray `&`, an undeclared `&nbsp;`, junk after the root) and
keeps the items around it, saying nothing; a document cut off part way
is cut short at the break. Scrapy's iternodes, by contrast, is strict.
Netweir's parser is strict by default: it stops at the first
well-formedness error with an `XmlError` giving the line, the column (in
characters) and what's wrong. In a crawl, the engine hands the page
to the callback, where reading its document raises the error and
`page.text` is there to repair.

Recover mode (`XmlOptions::recover`, `recover=True` in Python,
`Settings.xml_recover` for a crawl) repairs the faults feeds most often
have, each repair counted and the first ten kept with their line and
column, so a warning can say what was done: an undeclared entity is read
as HTML's named character (from lexbor's HTML5 entity table) or kept as
text, a bare `&` is text, an undeclared prefix leaves its element or
attribute in no namespace, anything after the root is ignored,
characters XML doesn't allow are dropped, the first of two attributes of
one name is kept, and elements open at the end are closed. Each repair
is a `repair(problem, fix)` call at the point the strict parser would
fail, so the strict errors and the repairs can't drift apart. What isn't
on the list (a start tag that never ends, say) is still an error, and
every limit on hostile XML holds in both modes. An empty document is
"Document is empty" when strict, as lxml says, and an empty document
when recovering.

Recovering costs no more than parsing: a `<` that starts no markup is
seen before quick-xml is asked to read it (which would read on to the
next `>`, so a run of them cost the square of its length), an `&` in an
attribute value is read only as far as a reference could go, and whether
an end tag closes anything open is a lookup, not a search of every open
element. The tests hold all three to linear time on a megabyte of each.
Sitemaps and iternodes are read on threads of their own, so a crawl's
event loop goes on while they are.

Recover mode also repairs end tags: one for an element further out
closes what's inside it, and one that closes nothing open is ignored. A
`<` that starts no markup (quick-xml reads `1 < 2` as a start tag with no
name) is kept as text, and reading starts again after it. lxml's recover
mode does all three differently, and loses more: it ends the document at
an end tag that closes nothing, drops the `<`, and lets `</a>` close only
the innermost element. The tests compare the two case by case.

Whitespace before the XML declaration is accepted in both modes: feeds
generated by templates often start with a blank line, and lxml reads
them.

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

## Parsed when read

The crawl engine used to parse every page it took for a web page or XML
before the callback ran, off Python's threads, and hand the tree over.
For an iternodes spider that meant the whole feed's tree was built
anyway, and thrown away: a 33 MB feed peaked at 592 MB. A page read as
XML is now parsed in the engine only when its rules need the tree (to
follow links or extract an Item); otherwise the callback gets the page
unparsed, and `page.root` builds the tree the first time something reads
it. The same feed now peaks at 184 MB. That parse keeps the crawl's
budget (`Settings.parse_timeout`) and recover setting, which every page
a crawl makes carries with it, into worker processes too: being parsed
later is no way round the limit. HTML pages are still parsed in
the engine: that's where their parse is cheapest, and where a
pathological page's 10-second budget is enforced, and a callback reads
almost every HTML page anyway.

## One node at a time

`XmlNodes` is Scrapy's `iternodes`: it reads the document once, and each
element with the wanted name becomes a document of its own, handed out
as soon as it ends, so the tree in memory is one node's worth however
big the feed. The feed's text is read whole first (decoded in Python and
copied into the iterator); reading from the bytes as they arrive would
save that copy, and is left for a crawl that streams responses.
The whole document is still checked as it's read; a break ends the
iteration with the error, after every node before it. The top element of
each node keeps the declarations it had in scope, so it's written out as
it would be from the whole document. Reading resumes from a byte offset
with a fresh quick-xml reader, which keeps the iterator free of
self-borrowing; end tags are checked against the builder's own stack for
that reason. Several nodes can end inside one entity expansion; finished
nodes wait in a queue, and each `next()` gives the oldest. Each `next()`
has a budget of its own (10 seconds in XMLFeedSpider), for the same
reason a whole parse has one.

A prefix in the tag that the caller doesn't bind means what the document
binds it to where each element is, as Scrapy's `xmliter_lxml` reads it;
the binding goes with the node, so the node's own queries can use the
prefix too.

Each node costs a lexbor document, about 3.5 µs to make, which is most
of the 270 ms 50,000 nodes take. Scrapy's `xmliter_lxml` took 1.2 s on
the same feed. lexbor can make a document that borrows another's
memory, which would be cheaper, but then no node's memory is returned
until the last is dropped, which defeats reading one at a time.

## Sitemaps

Sitemaps were read by a quick-xml loop of their own, which left
declared entities unexpanded and knew nothing of the rest. The first
version of this work read them through `Document::parse_xml`: whole, and
strictly. That broke on sitemaps the old loop and Scrapy both read (an
undeclared `image:` or `xhtml:` prefix, `&nbsp;`, a `\x0b`, a PHP warning
after the root, an attribute given twice), and a 200,000-URL sitemap went
from 196 MB to 864 MB.

So sitemaps are now walked, not parsed: `walk_xml` runs the same builder
with an `XmlVisitor` instead of a tree, so it has the same checks, entity
limits and budget (10 seconds a sitemap), in recover mode, with what was
repaired returned for one warning per sitemap. The visitor matches
`urlset`, `sitemapindex`, `url`, `sitemap`, `loc`, `lastmod`,
`changefreq`, `priority` and alternate `link`s by local name, in any case
and namespace. The 200,000-URL sitemap is back to 195 MB, and reads in
0.7 s.

## Known limits

- CSS type selectors on XML ignore namespaces and case; CSS attribute
  names ignore case.
- The `namespace::` axis gives nothing.
- DTD attribute defaults aren't applied (nor are they by lxml as Scrapy
  sets it up).
