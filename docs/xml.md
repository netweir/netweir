# XML, feeds and CSV

RSS and Atom feeds, sitemaps, SOAP answers, product exports: a lot of
what's worth scraping arrives as XML rather than as a web page, or as a
CSV file. Netweir reads XML as XML, with the same `css()`, `xpath()`,
`get()` and `getall()` you use on HTML, and has Scrapy's two feed spiders
for crawling feeds.

## Reading XML

A page whose Content-Type says it's XML is parsed as XML. That's
`text/xml`, `application/xml`, and any `+xml` type (RSS's, Atom's, SOAP's)
except XHTML's, which is a web page and is parsed as one. A page with no
Content-Type, or only `application/octet-stream` (some servers send feeds
that way), is judged by how it starts, as Scrapy judges it: `<html` means
HTML, and an XML declaration or a first element HTML doesn't have
(`<rss>`, `<urlset>`) means XML:

```python
page = netweir.get("https://example.com/feed.rss")
page.root.type                          # 'xml'
page.xpath("//item/title/text()").getall()
page.xpath("//item/link/text()").get()  # 'https://example.com/posts/1'
```

That last line is why it matters. HTML has a `<link>` element that never
holds anything, so an HTML parser throws away the URL inside an RSS
`<link>`. Read as XML, it's there.

Any text can be read as XML, whatever it was served as:

```python
doc = netweir.parse(page.text, type="xml")
doc = netweir.parse("<prices><price currency='EUR'>12.50</price></prices>", type="xml")
```

`type="html"` (the default) parses as HTML. `page.text` already reads the
encoding an XML declaration names (`<?xml version="1.0"
encoding="ISO-8859-1"?>`) when the Content-Type gives none, and finds
UTF-16 and UTF-32 by their first bytes when there's no byte order mark,
as the XML spec describes. `page.encoding` names the encoding it used.

What's different from HTML:

- **Names keep their case.** `//Price` and `//price` are different
  elements, as XML says they are.
- **`get()` writes XML.** An empty element is `<item/>`, and an element
  taken from a document declares the namespaces it uses, so the XML you
  get stands on its own: `<dc:creator
  xmlns:dc="http://purl.org/dc/elements/1.1/">Ann</dc:creator>`. It's
  the XML Scrapy gives, declaration for declaration.
- **Attribute names keep their prefix**, in `node.attrs` and in
  `Selection.attrib`: `{"xml:lang": "en"}`. (parsel gives lxml's
  `{http://www.w3.org/XML/1998/namespace}lang` instead.)
- **CDATA is text.** `<![CDATA[<p>Hi</p>]]>` reads as the text
  `<p>Hi</p>`, joined to any text beside it.
- Comments and processing instructions (`<?xml-stylesheet ...?>`) are
  kept, and XPath's `comment()` and `processing-instruction()` find them.

## Namespaces

Most feeds use namespaces, and they're where XPath on XML usually goes
wrong. An Atom feed starts like this:

```xml
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>One</title></entry>
```

`xmlns="..."` puts every element in the Atom namespace, and in XPath a
name without a prefix means *no* namespace. So `//entry` finds nothing.
Give the namespace a prefix, and use it:

```python
ATOM = "http://www.w3.org/2005/Atom"

page.xpath("//a:entry/a:title/text()", namespaces={"a": ATOM}).getall()

page.register_namespace("atom", ATOM)
entries = page.xpath("//atom:entry")
entries.xpath("atom:title/text()").getall()  # registered prefixes carry on
entries[0].xpath("atom:link/@href").get()
```

`namespaces=` is for that one query. `register_namespace()` binds the
prefix for every query from that node, and from the nodes and selections
those queries give you. Any prefix will do, as long as the URI is the
right one. A prefix that isn't bound is an `XPathError` saying so, rather
than a query that quietly finds nothing. `xml:` is always bound, and so
are `re:` and `set:`, as in Scrapy.

Or take the namespaces away:

```python
page.remove_namespaces()
page.xpath("//entry/title/text()").getall()
```

`remove_namespaces()` changes the whole document, for every node of it:
prefixes go (`dc:creator` becomes `creator`), and so do the declarations.
It's the quickest way to write a spider, and the right one unless two
namespaces in the document use the same name for different things.

`name()`, `local-name()` and `namespace-uri()` give an element's name as
written (`soap:Envelope`), without its prefix (`Envelope`), and its
namespace's URI.

## CSS on XML

CSS works, with allowances where it can't tell what XML tells apart:

- Type selectors match an element's local name whatever its namespace and
  whatever its case: `page.css("entry title::text")` reads an Atom feed
  without `remove_namespaces()`, and `css("item")` finds `<Item>` too.
- Attribute names ignore case, as in HTML: `[href]` matches `HREF="..."`.
  A prefixed attribute is matched by its whole name, so `[href]` doesn't
  match `xlink:href`.

When the namespace or the case matters, use XPath, which respects both.

## Broken and hostile XML

XML that isn't well-formed raises `netweir.XMLError`, a `ValueError`,
saying what's wrong and where:

```python
try:
    doc = netweir.parse(text, type="xml")
except netweir.XMLError as e:
    print(e.line, e.column, e.reason)  # 2 10 </feed> doesn't close <entry>
```

That's the default, and it's stricter than Scrapy. parsel reads XML in
lxml's recover mode, which carries on past a stray `&`, an undeclared
`&nbsp;` or junk after the root and keeps the items around them, without
a word; only a document cut off part way is cut short there. (Scrapy's
iternodes is strict.) Netweir tells you what's wrong, and in a crawl the
callback still gets the page: reading its document raises the error, and
`page.text` and `page.body` are still yours to fix up and parse again.
That's because a page read as XML is parsed when something first reads
it (`page.root`, `page.xpath()`, `page.css()`), not before the callback
runs, unless the crawl's rules need it first; then a parse error is
logged by the crawl, and raised again where the callback reads the page.
An iternodes spider never reads the page's document, so the whole tree
is never built.
An empty or blank body raises `XMLError("Document is empty")`, as lxml
does.

For feeds you know to be sloppy, ask for recover mode:

```python
doc = netweir.parse(page.text, type="xml", recover=True)
root = page._response.parse(recover=True)   # or for a whole crawl:
settings = netweir.Settings(xml_recover=True)
```

It repairs what feeds most often get wrong, and logs one warning per
document saying how many problems it repaired, with the line, column and
fix of the first ten:

| Problem | Repair |
|---|---|
| An undeclared entity (`&nbsp;`, `&eacute;`) | read as HTML's character of that name if there is one, kept as text if not |
| A `&` that starts no reference (`Tom & Jerry`) | kept as text |
| An undeclared prefix (`<media:content>`) | the element or attribute is in no namespace, named as written |
| Anything after the root element (a PHP warning) | ignored |
| A character XML doesn't allow (`\x0b`) | dropped |
| An attribute given twice | the first is kept |
| The document ends with elements open | they're closed |
| An end tag for an element further out (`</channel>` with `<title>` open) | the elements inside it are closed with it |
| An end tag that closes nothing open | ignored |
| A `<` that starts no markup (`1 < 2`) | kept as text |
| An empty document | an empty document |

Anything else, a start tag that never ends say, is still an error, and
the limits below still hold. Where lxml's recover mode repairs these
differently, Netweir keeps more: lxml ends the document at an end tag
that closes nothing, losing the rest, drops a lone `<`, and lets `</a>`
close only the innermost element when `<a>` is further out.

XML can also be written to hurt the program reading it. These are
refused, each with an `XMLError`:

| What | The limit |
|---|---|
| External entities (`<!ENTITY x SYSTEM "file:///etc/passwd">`), external DTDs | never fetched or read. Using an external entity is an error; a DTD's own declarations are ignored |
| Entities the document declares | expanded, but no more than 16 deep, and to no more than 1 MiB plus 8 bytes per byte of the document in all. The "billion laughs" (10 levels of 10 references each, a gigabyte from a kilobyte) stops at the limit |
| Entities that refer to themselves | refused |
| Nesting | 2048 elements deep, lxml's limit with Scrapy's settings (lxml cuts a deeper document off there without a word) |
| Time | `netweir.parse(text, type="xml", timeout=5)` raises `ParseTimeout` after 5 seconds; the clock is read every thousand or so events, attributes or kilobytes of expanded entity, so one enormous start tag can't outrun it. In a crawl, every page has `Settings.parse_timeout` (10 seconds), wherever it's parsed: in the engine for its rules, or when your callback first reads it, in the main process or a worker. A sitemap has 10 seconds, and iternodes 10 seconds for each node. Only a page you make yourself (`netweir.parse`, or a `Page` built by hand) has no limit unless you give one |

Parameter entities (`%name;`) are never expanded, and declarations after
one aren't read.

## XML feeds: XMLFeedSpider

`netweir.XMLFeedSpider` crawls feeds as Scrapy's does. Each element named
`itertag` goes to `parse_node`, which you write:

```python
class Blog(netweir.XMLFeedSpider):
    start_urls = ["https://example.com/feed.rss"]
    itertag = "item"  # the default, for RSS

    def parse_node(self, page, node):
        yield {
            "title": node.xpath("title/text()").get(),
            "link": node.xpath("link/text()").get(),
        }
        yield page.follow(node.xpath("link/text()").get(), callback="post")

    def post(self, page):
        ...
```

`parse_node` gets the page and the node, and returns or yields items and
requests, like any callback. It can be async. The node is a Selection of
one, as Scrapy hands over a Selector: `node.xpath()`, `node.css()`,
`node.get()`, `node.getall()`, `node.attrib`, `node.re()`,
`node.re_first()` and `node.register_namespace()` all work on it.

`iterator` says how the feed is read:

| `iterator` | Reads |
|---|---|
| `"iternodes"` (the default) | one node at a time, as the feed is read, so a feed of a million items is never a tree in memory. The feed's text is still read whole (decoded, and copied into Rust), but each node is a document of its own, gone once you're done with it. |
| `"xml"` | the whole feed, as XML, before the first node |
| `"html"` | the whole feed, as HTML, for markup too broken to be XML. `<link>` comes out empty. |

`iternodes` reads the feed in Rust; 50,000 nodes take about a quarter of
the time Scrapy's iternodes does.

For a feed with namespaces, list them in `namespaces`. They're registered
on every node, and `itertag` can use them, with every iterator (in Scrapy
only `iterator = "xml"` uses them for `itertag`):

```python
class Sitemap(netweir.XMLFeedSpider):
    start_urls = ["https://example.com/sitemap.xml"]
    namespaces = [("s", "http://www.sitemaps.org/schemas/sitemap/0.9")]
    itertag = "s:url"

    def parse_node(self, page, node):
        yield page.follow(node.xpath("s:loc/text()").get(), callback="product")
```

A prefix in `itertag` that isn't in `namespaces` means what the feed
declares it to mean where the element is, as Scrapy's iternodes reads
it: `itertag = "g:item"` finds a Google Merchant feed's items, and
`node.xpath("g:id/text()")` works on them. An Atom feed's entries are in
its default namespace, which has no prefix, so `itertag = "entry"` finds
none of them; use `itertag = "atom:entry"` with the Atom namespace listed.
(For sitemaps themselves, `sitemap_urls` on any spider does the work; see
[Sitemaps](crawling.md#sitemaps).)

`recover = True` reads the feed in recover mode (see
[Broken and hostile XML](#broken-and-hostile-xml)); the default, `None`,
follows `Settings.xml_recover`.

Two more methods, both optional, both sync or async:

- `adapt_response(page)` gets the page before it's read, and returns the
  page to read. `page.replace(body=...)` makes one with a new body; text
  is encoded in the page's own encoding, so its charset and any XML
  declaration stay true (or in UTF-8, with the charset changed, if that
  encoding can't hold it).
- `process_results(page, results)` gets what `parse_node` gave for one
  node, as a list, and returns the list to keep.

```python
class Shop(netweir.XMLFeedSpider):
    start_urls = ["https://example.com/products.xml"]
    itertag = "product"

    def adapt_response(self, page):
        # The feed uses HTML's &nbsp;, which XML doesn't have.
        return page.replace(body=page.text.replace("&nbsp;", "&#160;"))

    def parse_node(self, page, node):
        yield {"sku": node.xpath("@sku").get(), "price": node.xpath("price/text()").get()}

    def process_results(self, page, results):
        return [item for item in results if item["price"]]
```

Without `parse_node`, the crawl's first page raises `NotConfigured`
saying so.

## CSV feeds: CSVFeedSpider

`netweir.CSVFeedSpider` sends each row to `parse_row` as a dict keyed by
the headers:

```python
class Products(netweir.CSVFeedSpider):
    start_urls = ["https://example.com/products.csv"]
    delimiter = ";"     # "," by default
    quotechar = "'"     # '"' by default
    headers = ["id", "name", "price"]  # the first row's, when not given

    def parse_row(self, page, row):
        yield {"id": row["id"], "price": float(row["price"])}
```

A row with more or fewer fields than there are headers is skipped, with a
warning naming its line. A quoted field can hold the delimiter or a line
break. `adapt_response` and `process_results` work as on XMLFeedSpider.

`netweir genspider -t xmlfeed` and `-t csvfeed` write a spider of each
kind to start from.

## Coming from Scrapy

Selectors, `register_namespace()`, `remove_namespaces()`, `namespaces=`
and the feed spiders' attributes carry over. The differences:

- `parse_node(self, page, node)` and `parse_row(self, page, row)` get a
  Netweir `Page`, as every callback does.
- Broken XML raises `XMLError` unless you ask for recover mode, and
  undeclared entities are errors then too (see above). Scrapy's
  selectors always recover.
- Entities a document declares in its own DTD are expanded, within the
  limits, so `text()` has them. parsel keeps them as references, which
  `text()` leaves out and `string()` doesn't.
- Attribute names keep their prefix (`xml:lang`) where lxml writes the
  namespace in braces.
- CSS type selectors on XML ignore namespaces and case, and attribute
  names ignore case; cssselect respects both.
- `itertag` uses `namespaces` with every iterator, and an unlisted prefix
  means the feed's own.
- `get()` on a document's root gives the whole document, a processing
  instruction before the root element included; parsel gives the root
  element.
- XPath's `namespace::` axis finds nothing.
