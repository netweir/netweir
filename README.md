<p align="center"><img src="https://raw.githubusercontent.com/netweir/netweir/main/docs/assets/header.gif" alt="Netweir: Rust does the work. You write Python." width="100%"></p>

<p align="center">
  <a href="https://github.com/netweir/netweir/actions/workflows/ci.yml"><img src="https://github.com/netweir/netweir/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://pypi.org/project/netweir/"><img src="https://img.shields.io/pypi/v/netweir" alt="PyPI"></a>
  <a href="https://pypi.org/project/netweir/"><img src="https://img.shields.io/pypi/pyversions/netweir" alt="Python versions"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="License: AGPL-3.0"></a>
  <a href="https://scorecard.dev/viewer/?uri=github.com/netweir/netweir"><img src="https://api.scorecard.dev/projects/github.com/netweir/netweir/badge" alt="OpenSSF Scorecard"></a>
  <a href="https://www.bestpractices.dev/projects/15970"><img src="https://www.bestpractices.dev/projects/15970/badge" alt="OpenSSF Best Practices"></a>
</p>

# Netweir

An ultra-fast, self-healing, undetectable web scraper. Rust does the work;
you write Python.

That's the finished product, and it isn't finished yet. Here's what each
of those words will mean when it is:

- **Ultra-fast.** Fetching, parsing, crawl scheduling and extraction all
  run in Rust, off Python's GIL. A spider that only declares what it wants
  never touches Python per page. Every speed claim is a benchmark in this
  repository that you can run.
- **Self-healing.** Selectors that find their element again after a site
  redesign and tell you they had to. Blocks spotted and recovered from by
  rotating, backing off or slowing down. Crawls that pick up where they
  stopped after a crash.
- **Undetectable.** Requests that are byte-for-byte what a real browser
  sends, from the TLS handshake to the order of the headers, and a real
  Chrome for the pages that need one.

## Where it is now

Working today:

- Fetching that is indistinguishable from Chrome 155, Firefox 158 or
  Safari 27 on the wire, over HTTP/2 and HTTP/1.1, with cookies and
  redirects. Below: how that's proved, and the one known exception.
- Forms, logins and JSON APIs: POST and any other method, sent the way
  Chrome, Firefox or Safari sends a form or a page's own script, and forms read off
  a page the way a browser submits them. See
  [docs/forms.md](docs/forms.md).
- The options requests and httpx users reach for, under the same names:
  `params=`, `auth=` (HTTP Basic, kept to the site you gave it to),
  `allow_redirects=False` to read a redirect, `max_redirects`, a timeout
  per call, and `verify=False` or a CA file for hosts with certificates of
  their own. In a crawl, a request takes them too, Scrapy's meta keys
  (`download_timeout`, `dont_redirect`, `http_user`...) work, and
  `retry_http_codes`, `max_redirects` and `http_auth` are settings. See
  [docs/fetching.md](docs/fetching.md).
- Sessions you control: a client's cookies to read, set and save to a
  storage state file Playwright reads too; a login done in Chrome carried
  on over HTTP, or the reverse; and several logged-in users in one crawl
  with Scrapy's `cookies=` and `meta["cookiejar"]`, kept in the
  checkpoint. See [docs/sessions.md](docs/sessions.md).
- A parser that keeps pace with selectolax, the one to beat in Python, and
  pulls data out of the page faster.
- CSS queries with Scrapy's `::text` and `::attr()`, all of XPath 1.0, and
  Beautiful Soup's `find_all` family, each faster than the library you'd
  otherwise use for it.
- XML read as XML: RSS, Atom, sitemaps and SOAP, with namespaces,
  `register_namespace()` and `remove_namespaces()` as in Scrapy, and
  hostile XML (external entities, a "billion laughs") refused. Scrapy's
  XMLFeedSpider and CSVFeedSpider, with a node-by-node reader in Rust
  for feeds too big to hold. See [docs/xml.md](docs/xml.md).
- Changing the page as Beautiful Soup does (`decompose()`, `extract()`,
  `replace_with()`, `unwrap()`, `new_tag()`, `append()`, `prettify()`),
  `find_similar()` to get every product card once you've found one, and
  JMESPath queries over JSON APIs and the JSON in a page's scripts. See
  [docs/selecting.md](docs/selecting.md#changing-the-page).
- Structured data: `page.metadata()` reads a page's JSON-LD, microdata,
  Open Graph, Twitter cards and Dublin Core in one call, in Rust.
- Prices, dates and script data: `parse_price` gives a `Decimal` and the
  currency from `£1,234.50` or `1.234,50 €`; `parse_date` reads ISO,
  HTTP and numeric dates, month names in seven languages and `3 days
  ago`; `page.js_data("__INITIAL_STATE__")` reads the JavaScript objects
  pages keep their data in. All three work as `into=` in an item. See
  [docs/selecting.md](docs/selecting.md#prices-dates-and-data-in-scripts).
- Item loaders, as Scrapy's: `add_css()`, `add_xpath()`, `add_jmes()`,
  `MapCompose`, `TakeFirst` and the rest, into dicts, dataclasses, attrs
  classes or Scrapy Items. Change the imports and your loaders carry
  over; they load a page's items in about three quarters of the time
  itemloaders and parsel take. See [docs/loaders.md](docs/loaders.md).
- Articles and Markdown: `page.main_content()` finds the story, post or
  description among the menus, sidebars, comments and ads (or tells you a
  listing has none), `page.article()` gives its title, byline, date,
  image and text, and `page.markdown()` writes the page or just its
  content as Markdown, tables and code blocks included. All in Rust, in a
  few milliseconds on a 1 MB page. See
  [docs/selecting.md](docs/selecting.md#reading-an-article-and-markdown).
- Chunks for RAG: `page.chunks()` cuts the content's Markdown at its
  headings into pieces sized for an embedding model, keeps code blocks
  and tables whole where they fit, and gives each chunk its heading path
  and an id that stays the same from crawl to crawl. `to_langchain()` and
  `to_llamaindex()` hand them on, and `netweir crawl docs --url URL -o
  docs.jsonl` chunks a whole docs site. See
  [docs/selecting.md](docs/selecting.md#chunks-for-retrieval).
- For AI agents: `netweir mcp` is an MCP server. Add it to Claude Code,
  Claude Desktop or Cursor and the agent can fetch pages as Chrome would
  and read them as Markdown, chunks, or products, jobs and articles as
  data. See [docs/mcp.md](docs/mcp.md).
- Auto-extraction: `page.extract(netweir.auto.Product)` reads a product,
  a category page's listing, a job posting, an article, a forum thread,
  a business's page, a property, a social post or a search engine's
  results from the
  structured data the page carries (JSON-LD, microdata, Open Graph), the
  state apps keep in their scripts (Next.js, Nuxt, Apollo), and its
  layout where those run out, in the common item schema
  Zyte's API uses, with where each field came from. When one field is
  wrong for a site, pin it with a selector and the rest stay automatic;
  it works in declarative spiders, and `netweir extract URL` tries it from
  the command line. `netweir crawl ecommerce --url URL` (or `article`,
  `job`) crawls a whole site with it, finding the categories and pages
  itself. See [docs/extraction.md](docs/extraction.md).
- Language-model extraction, if you want it: `netweir.llm.extract(page,
  {"name": "the loaf's name", "weight": "its weight"})` has Claude read the
  fields you describe from a page no type covers, in exactly the shape you
  asked for. Your key, and nothing calls a model unless you ask. See
  [docs/llm.md](docs/llm.md).
- Crawling: spiders, sitemaps, robots.txt, per-site throttling, and items
  written to JSON Lines, JSON, XML, CSV or Parquet, on disk or in cloud
  storage, in one file or in batches. Downloader and spider
  middlewares, most of Scrapy's signals, `from_crawler`, and pipelines
  with `open_spider`/`close_spider` work as Scrapy's do, so most Scrapy
  pipelines and middlewares carry over unchanged.
  `allowed_domains` keeps a crawl on the sites you meant; `max_items`,
  `max_pages`, `max_errors`, `max_time` and `max_memory` stop it when
  you've seen enough.
- Files and images an item links to, downloaded through the crawl
  itself (its robots.txt, delays, proxies and cache) and stored once
  each, on disk or in a bucket, in Scrapy's layout and with Scrapy's
  `file_urls` and `image_urls` fields. Images become JPEGs with
  thumbnails. See
  [docs/crawling.md](docs/crawling.md#downloading-files-and-images).
- `netweir serve`: spiders run as jobs you schedule, watch and cancel
  over HTTP, with their logs, items and stats kept, like scrapyd and
  speaking much of its API. See [docs/serve.md](docs/serve.md).
- `netweir shell URL`, `fetch` and `view`: the page Netweir gets, at a
  Python prompt, on stdout or in your browser, for trying selectors before
  they go in a spider. See [docs/shell.md](docs/shell.md).
- Scrapy's project commands, so your hands already know them:
  `startproject`, `genspider` (plain, rules, sitemap or feed spiders), `list`,
  `crawl NAME`, `parse URL` to see what a callback makes of one page, and
  `check`, which runs Scrapy-style contracts from your docstrings,
  custom ones included, and exits 1 when a redesign broke a spider. See
  [docs/project.md](docs/project.md).
- Validating items against a dataclass or a JSON Schema, and checks at
  the end of a crawl (item counts, error rate, field coverage) that make
  `netweir crawl` exit 1 when a crawl finished but didn't work.
- Watching a crawl while it runs: a progress line in the log every
  minute, live `spider.stats` with Scrapy's `inc_value` and friends, and
  an optional endpoint Prometheus can scrape. See
  [docs/crawling.md](docs/crawling.md#watching-a-crawl-as-it-runs).
- A download handler for Scrapy, so a Scrapy project can fetch as a
  browser does without moving off Scrapy. See
  [docs/crawling.md](docs/crawling.md#or-keep-scrapy-and-fetch-with-netweir).
- A response cache for the days you spend writing a spider: run it thirty
  times and the site serves its pages once. See
  [docs/crawling.md](docs/crawling.md#caching-while-you-develop).
- A crawl you run every day can skip the pages whose items it already
  has, as scrapy-deltafetch does, and still read every listing for what's
  new. The output keeps every run's items, a crash included. See [docs/crawling.md](docs/crawling.md#skipping-what-you-already-have).
- WARC and HAR files of everything a crawl fetched: the requests as they
  went out, header for header, and the responses, redirect hops and
  robots.txt included; logins are left out. Read them back with warcio, or open them in a
  browser's network panel. See
  [docs/crawling.md](docs/crawling.md#keeping-what-you-fetched).
- Declarative spiders, where you describe the item and the links and
  Rust does the rest without running any Python per page.
- Callbacks in worker processes, for spiders whose own Python is the slow
  part: `workers=4` ran a heavy spider 3.1 times faster, with the same
  output.
- One crawl on many machines: processes share a request queue and a
  duplicate filter in Redis, as with scrapy-redis, and a process that
  dies has its requests picked up by the others. Feed a running crawl
  with `netweir queue push`. See [docs/distributed.md](docs/distributed.md).
- Self-healing: block pages recognised and recovered from, crawls that
  resume after a crash, and selectors that find their element again after
  a redesign.
- Proxy pools: a new proxy for every request, one per site, or one only
  after a block, with proxies that keep failing benched for a while. See
  [docs/self-healing.md](docs/self-healing.md#proxies).
- A Chrome driver for pages that need JavaScript: click, type, hover,
  scroll an endless feed to its end, pick from dropdowns, tick boxes,
  upload files and reach into iframes like Playwright, finding elements by
  CSS, XPath, their text or their role, save a page as a PDF, replay a HAR file, record a trace, then read the
  page with Netweir's selectors, or read the JSON its own requests got back. A
  profile can be kept between runs. Requests can be blocked, changed or
  answered before they leave Chrome. An `alert()` never stalls a page,
  popups and new tabs come back as pages you can read, and downloads
  land in a folder you can save from. In a
  crawl, it can take over a request a site keeps blocking, and hand the
  cookies it earns back to the fast HTTP client. `netweir install chrome`
  fetches the Chrome it's tuned for.
- Steps: a request, a rule or a whole crawl can carry a list of things
  for Chrome to do before the callback reads the page, such as
  dismissing a banner, clicking "Load more", typing a search, scrolling a
  feed to its end, taking a screenshot or running your own function. The
  list is JSON, so it survives checkpoints and shared queues. Each step's
  outcome comes back to the callback, and the input is the same human
  mouse, touch and keys as the driver's. See [docs/steps.md](docs/steps.md).
- A page's environment, with Playwright's names: viewport and screen,
  phones (`device="Galaxy S23"`), language, time zone, location and
  permissions, dark mode, extra headers, offline and init scripts, per
  page, per context, for a whole browser or for a crawl. Each is set
  where Chrome sets it for itself, so the page, its workers and frames
  from other sites all say the same thing, and the request headers
  agree. See [docs/environment.md](docs/environment.md).

Coming next: Firefox in the driver, once there's a Firefox that doesn't
announce it's automated. The designs are in [docs/design/](docs/design/).

## Install

```
pip install netweir
```

There are wheels for Linux, macOS and Windows, on Python 3.11 and up,
free-threaded 3.14 included, so there's nothing to compile. Pages that
need a real browser want Chrome too: `netweir install chrome` fetches the
version Netweir is tuned for.

## Quick look

```python
import netweir

page = netweir.get("https://books.toscrape.com/")

page.css("h1::text").get()  # "All products"
page.css(".price_color::text").getall()  # ["£51.77", "£53.74", ...]
page.css("article h3 a::attr(href)").getall()  # ["/catalogue/...", ...]

for book in page.css("article.product_pod"):
    print(book.css("h3 a::attr(title)").get(), book.css(".price_color::text").get())
```

If you've used Scrapy, `::text`, `::attr()`, `get()` and `getall()` mean what
you think they mean. Already have the HTML? `netweir.parse(html)` skips the
request and gives you something you query the same way.

## Bring your selectors with you

Whatever you scrape with now, your selectors should work here unchanged.
XPath is all of XPath 1.0, plus the extras parsel users lean on:

```python
page.xpath("//article//h3/a/@title").getall()
page.xpath("//p[has-class('price_color')]/text()").get()
page.xpath("//article//a[re:test(@href, '_\\d+/index\\.html$')]/@href").getall()
page.xpath("//li[@class=$cls]", cls="next").get()  # variables, as in parsel
page.css(".price_color::text").re(r"[\d.]+")  # ["51.77", "53.74", ...]
```

And if you think in Beautiful Soup, the same page answers that way too:

```python
import re

page.find("ul", class_="pager").find("a")["href"]
page.find_all("a", title=True, limit=10)
page.find_all(string=re.compile("£"))
page.find("h3").find_parent("li").find_next_sibling("li")
```

Two things work slightly differently, on purpose. `get()` on an element
gives its HTML the way Chrome's `outerHTML` writes it, so an attribute
holding a quote comes out as `&quot;` where lxml would switch to single
quotes; the elements and their order are the same. And attribute values are
plain strings: `node["class"]` is `"star-rating Three"`, not Beautiful
Soup's list. Filters still match one class out of several, as you'd expect.

## Data without selectors

Most shops, job boards and news sites describe their pages to search
engines in structured data. Netweir reads it, falls back on the page's
layout for whatever it leaves out, and gives you the item:

```python
product = page.extract(netweir.auto.Product)
product["name"], product["price"], product["currency"]
product["metadata"]["sources"]["price"]  # "json-ld"
```

Field names and shapes are the common item schema's, so items from many
sites, or from Zyte's API, share one table. When a site's data is wrong
for one field, pin that field and keep the rest:

```python
class Lumo(netweir.auto.Product):
    price = netweir.css("span.sale", track="price")
```

A pinned price element gives `"9.50"`, not `"$9.50 inc. VAT"`, and
`track=` keeps the selector working through a redesign. Products,
listings, job postings and articles are covered so far; see
[Auto-extraction](docs/extraction.md).

## Crawling a site

A spider is a class with a `parse` method. It yields what it found and
the links worth following:

```python
import netweir


class Books(netweir.Spider):
    start_urls = ["https://books.toscrape.com/"]

    async def parse(self, page):
        for book in page.css("article.product_pod"):
            yield {
                "title": book.css("h3 a::attr(title)").get(),
                "price": book.css(".price_color::text").get(),
                "url": page.urljoin(book.css("h3 a::attr(href)").get()),
            }
        if next_page := page.css("li.next a::attr(href)").get():
            yield page.follow(next_page)
```

Run it from the shell, and the file extension picks the format:

```
netweir crawl books.py -o books.jsonl
```

That run fetched all 50 pages and wrote 1,000 books in 18 seconds, most of
it spent waiting on purpose. Out of the box Netweir reads each site's
robots.txt and stays out of what it disallows, skips pages whose owners
reserve text and data mining rights (TDMRep), and spaces its requests to
each site by how quickly the site answers, backing off when it gets a 429.
A page linked three different ways, or with `utm_` tags on the end, is
fetched once.

All of that is a setting when you need it to be:

```python
class Books(netweir.Spider):
    settings = netweir.Settings(concurrency=128, per_domain=4, throttle=False)
    pipelines = [drop_out_of_stock, netweir.export.parquet("books.parquet")]
```

A pipeline is any function, sync or async, that gets each item and returns
it, changes it, or returns `None` to drop it. If you know Scrapy, the rest
will look familiar: `Request` with `callback`, `errback`, `meta` and
`priority`; `page.follow_all()`; `-s concurrency=16` on the command line.
A callback that raises is logged with the URL and counted, and the crawl
carries on unless you set `fail_fast=True`.

## Spiders with no Python in the loop

Most spiders are the same three moves: follow the pagination, open each
product, pull the same fields out of every one. You can say that instead of
writing it:

```python
class Book(netweir.Item):
    title = netweir.css("h1::text")
    price = netweir.css(".price_color::text", re=r"[\d.]+", into=float)
    upc = netweir.xpath("//th[.='UPC']/following-sibling::td/text()")
    stock = netweir.css(".availability::text", re=r"\d+", into=int)


class Books(netweir.Spider):
    start_urls = ["https://books.toscrape.com/"]
    rules = [
        netweir.Follow("li.next a"),
        netweir.Follow("article.product_pod h3 a", extract=Book),
    ]
```

Run against the real site, that crawled all 1,050 pages and wrote 1,000
books like this one:

```json
{"title": "A Light in the Attic", "price": 51.77, "upc": "a897fe39b1053632", "stock": 22}
```

Every page in that crawl was parsed, searched and turned into an item in
Rust, several pages at a time on separate threads. Python saw finished
items and nothing else, which leaves your event loop and your GIL free for
whatever your pipelines do. If a price won't turn into a float, you get
`None` and one warning naming the field and the text it found, rather than
a crash on page 600.

You don't have to choose. A rule can hand its pages to a callback, a
callback can call `Book.extract(page)`, and a spider can have both rules
and its own `parse`. `bench/rules.py` crawls a local 1,050-page shop both
ways; the declarative spider takes about two thirds of the time.

## When a site pushes back, or changes

Three things go wrong on a long crawl, and Netweir handles each without
being asked.

**The site blocks you.** Every response is checked against the block pages
of Cloudflare, Akamai, DataDome, HUMAN, Kasada, Imperva and AWS WAF, so a
challenge that comes back as a 403 isn't mistaken for a page. A blocked
request is tried again with a new session (an empty cookie jar, and the next
of your `proxies` if you gave several), and the site is slowed down. A
server error is retried with backoff, a 429's Retry-After is honoured, and a
site that keeps blocking is paused for a while instead of hammered. With
`browser="on_block"`, a request still blocked after all that gets one go in
Chrome, which waits for the challenge to pass and then hands its cookies to
the HTTP client, so the rest of the site doesn't need Chrome. If even that
fails, your spider's `on_block(request, page)` hears about it. Outside a
crawl, `netweir.get` raises `netweir.Blocked`, naming the vendor.

**The crawl dies.** Give it somewhere to keep its state and run it again
after a crash, a reboot or a `kill -9`:

```
netweir crawl books.py -o books.jsonl -s checkpoint=crawls/books
```

It picks up where it stopped. Pages already done aren't fetched again, and
no item ends up in `books.jsonl` twice: the tests kill a crawl at random
moments until it finishes and check exactly that.

**The site is redesigned.** Name a selector and Netweir remembers what its
element looked like:

```python
price = page.css(".price_color::text", track="price")
```

When a new build renames `price_color`, wraps it in another `div` or swaps
`h1` for `h2`, the selector stops matching, and Netweir finds the element
most like the one it remembers. `price.relocated` says it had to, and
`price.score` says how sure it is. You get a warning, not an empty column.
`track=` works on Item fields too. Add `repair = netweir.repair.llm()` to a
spider, and a language model proposes a replacement selector for each one
that broke. Netweir checks every proposal on the page and reports it to you,
and never applies one itself.

## Looking like a browser

Most sites don't block scrapers by reading their code. They block them by
the first few hundred bytes of the connection: which TLS ciphers and
extensions arrive, what the HTTP/2 settings frame says, which headers come
first. A Python HTTP library gives itself away before it has
asked for anything.

Netweir sends what Chrome 155 sends. Not something close to it: the same
ClientHello, the same HTTP/2 settings and priorities, the same headers in
the same order and the same capitalisation over HTTP/1.1. Cookies go where
Chrome puts them, and redirects are followed hop by hop the way Chrome
follows them, header order after a redirect included.

You don't have to take that on trust. Chrome was recorded doing four
navigations against a local server: a page that sets a cookie, a revisit,
a redirect to another site and one within the site. `cargo test -p
netweir-core` has Netweir do the same four and fails if any request differs
from Chrome's. The public checker at tls.peet.ws reports the same JA4
fingerprint for both: `t13d1517h2_8daaf6152771_cb7bf5808d99`.

Firefox 158 is there too: `netweir.get(url, profile="firefox")`, or
`profile="firefox"` in a spider's settings. Firefox sends its TLS extensions
in a fixed order, with no GREASE, and the same test checks that order
against a recording of the real browser, as well as everything above.
Haven't updated yet? The versions before stay, as `chrome-154-macos` and
`firefox-156-macos`, and are tested the same way.

So is Safari 27, as `profile="safari"`. It has two habits of its own, and
Netweir copies both. Two cookies with the same path go out newest first.
And once a site has answered over HTTP/1.1, Safari stops offering it
HTTP/2 on new connections, so Netweir does the same.

One known difference: Chrome sends a `priority` header only over HTTP/2,
and Netweir can't tell a server lacks HTTP/2 until it has answered once. So
the first request to an https server that only speaks HTTP/1.1 carries that
header; every request after it doesn't.

Images a crawl downloads with `netweir.images` go out as an `<img>` on
the page asks for them, not as a page, and the same test holds Chrome's
and Firefox's to recordings of each loading an image. Safari's are
written from what Safari is known to send, and not yet checked against a
recording.

A login goes where the browser puts it. Chrome 155 and Firefox 158 were
recorded holding an HTTP login and sending it with a navigation, a form, a
script's request and an image; Netweir's `Authorization` header lands in
the same place in each, and the same test checks it. Safari's place isn't
recorded yet, so with `profile="safari"` it goes after Safari's own
headers. And `verify=False` or a CA file of your own only changes what
Netweir accepts from the server, never what it sends: a test holds both
handshakes to the browser's recording.

For many pages at once, use a client. It keeps connections and cookies
between requests, and the requests run concurrently in Rust:

```python
async with netweir.Client(proxy="http://user:pass@proxy:8080") as client:
    pages = await client.get_many(urls)
```

A request that fails before any response arrives raises `netweir.FetchError`,
and its `kind` says why: `"timeout"`, `"connect"`, `"tls"` and so on. A 404 is
still a page; check `page.status`.

## Why it's fast

Netweir parses with its own HTML parser, written in Rust from the HTML
Standard. It passes the html5lib tokenizer tests and the web-platform-tests
tree-construction tests that don't need JavaScript, and on 50 real pages it
builds the same trees as lexbor, the C parser selectolax uses, about twice
as fast. Then the whole query runs in Rust too, and Python gets a finished
list of strings rather than an object for every element along the way. The
GIL is released while it works, so threads parse in parallel.

`bench/parse.py` pulls every price, title and link out of a generated shop
page:

| | 1 MB page | 10 MB page |
|---|---|---|
| Netweir | 3.0 ms | 31 ms |
| selectolax | 5.7 ms | 64 ms |
| BeautifulSoup (lxml) | 177 ms | 2.0 s |
| lxml + cssselect | 300 ms | 54 s |
| parsel | 297 ms | 54 s |

Measured on an Apple M-series laptop with Python 3.14. Run it yourself:
`uv run --group bench python bench/parse.py`.

XPath and `find_all` are a different story, because the libraries people
use for them are much slower. `bench/query.py` parses the same shop page and
asks six XPath questions, or six Beautiful Soup ones:

| | 1 MB page | 3 MB page |
|---|---|---|
| Netweir, XPath | 7.5 ms | 22 ms |
| lxml, XPath | 824 ms | 8.4 s |
| parsel, XPath | 823 ms | 8.5 s |
| Netweir, `find_all` | 7.7 ms | 23 ms |
| Beautiful Soup, `find_all` | 149 ms | 462 ms |

Look at lxml across the row: three times the page took ten times as long.
Netweir builds a flat index of the document on the first query, so `//x` is
a scan over a few arrays, and its time grows with the page and no faster.

Most spiders don't query the whole page, though. They loop over cards and
ask each one for a few fields, and on an element that small, a query's
cost is mostly overhead. `bench/items.py` does that for the shop page's
998 cards, four fields each:

| Per query | CSS | XPath |
|---|---|---|
| Netweir | 0.8 µs | 1.3 µs |
| selectolax | 1.2 µs | |
| parsel | 7.0 µs | 5.6 µs |
| lxml | 10.3 µs | 6.0 µs |

You can write the query inline; each thread keeps the ones it has
compiled. All three benchmarks run in CI, and the build fails if any
library beats Netweir.

Pulling the readable part out of a page is the other job people time,
usually right before it goes to a search index or a model.
`bench/content.py` hands each tool a 1 MB article buried in navigation, a
sidebar and a thousand comments, and times it from raw HTML to answer,
parsing included:

| | Netweir | The usual tool |
|---|---|---|
| Main text | 6.1 ms | trafilatura 2.3: 121 ms |
| Main content as Markdown | 7.2 ms | trafilatura 2.3: 163 ms |
| Whole page as Markdown | 9.5 ms | markdownify 1.2: 172 ms |

That's twenty times faster on one M2 Max with Python 3.14, and CI fails
the build if either one catches up. It measures speed, not judgement:
trafilatura has years of tuning on news sites, so if what you need is
the best possible text from a newspaper, test both on your own pages.

Parsing is one part of a crawl. `bench/crawl.py` times the whole thing: one
local server plays 100 sites of 100 pages, each answer 50 ms late, and every
crawler fetches all 10,000 pages and pulls a title and price from each,
with the same limits (100 requests in flight, 8 per site):

| | Pages a second | CPU per page | Peak memory |
|---|---|---|---|
| Netweir | 1,770 | 0.22 ms | 68 MB |
| Scrapy 2.19 | 890 | 1.1 ms | 125 MB |
| httpx + selectolax | 220 | 3.2 ms | 190 MB |
| Scrapling 0.4 | 180 | 0.8 ms | 76 MB |

With 100 requests in flight and 50 ms per answer, 2,000 pages a second is
the most any crawler could do here, so Netweir is waiting on the server,
not on itself. Scrapling is held back by its HTTP session's default of 10
connections, which it doesn't let you change; that is how it ships. Same
laptop, Python 3.13; run it yourself with
`uv run python bench/crawl.py` once the others are installed.

Pages you don't control can be built to be slow to parse. Pass a timeout and
Netweir gives up instead of hanging:

```python
netweir.parse(html, timeout=2.0)  # raises netweir.ParseTimeout
```

The timeout bounds time, not memory. A crawl reads at most 64 MiB of a
response, counted after decompression, so a huge page or a small compressed
one that unpacks to gigabytes fails instead of filling memory
(`max_response_size` sets the limit; `max_size=` does the same for
`netweir.get`). It doesn't stop a small page built to abuse the HTML5
spec's rules for misnested formatting tags, which can make any
spec-compliant parser allocate gigabytes; nothing in Netweir caps the
parser's memory yet.

## When a page needs a real browser

Some pages are empty until their JavaScript runs. For those, Netweir
drives Chrome, and you get the rendered page back as the same `Node` you'd
get from a fetch:

```python
async with netweir.browser() as browser:
    page = await browser.new_page()
    await page.goto("https://quotes.toscrape.com/js/")
    await page.click("li.next a")
    root = await page.parse()
    quotes = root.css("span.text::text").getall()
```

There's no `sleep` in that, and there doesn't need to be. A click waits
until its button exists, is visible, has stopped moving, is enabled and
isn't covered by a cookie banner, and if it never gets there, the error
tells you which of those it was.

A driven Chrome normally gives itself away: `navigator.webdriver` is true,
the user agent says `HeadlessChrome`, and the usual drivers switch on
DevTools features that page scripts can notice. Netweir doesn't do any of
that. A test serves a page that looks for the first two from script, and
also fails if any of those DevTools features was switched on.
[docs/browser.md](docs/browser.md) has the rest, including what it doesn't
hide yet.

## Documentation

[docs/](docs/README.md) has a getting-started guide, a page for each part of
Netweir and every setting with its default.

## Building

You need Rust, a C compiler, CMake and [uv](https://docs.astral.sh/uv/).

```
git clone --recurse-submodules https://github.com/netweir/netweir
cd netweir
uv sync --group dev
uv run maturin develop --uv
uv run pytest
```

uv installs the dependencies and maturin builds Netweir, so after changing
any Rust, rerun `uv run maturin develop --uv` before testing.

If you set `RUSTFLAGS` yourself, it replaces the flags in
`.cargo/config.toml`, so add `--cfg pyo3_disable_reference_pool` to it.
Without that, threads on free-threaded Python barely scale, and
`tests/test_parse.py` fails to tell you
([docs/design/threads.md](docs/design/threads.md)).

## Contributing

Issues, fixes and new browser profiles are all welcome.
[CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and the one
hard rule (write it yourself), and before your first pull request is merged
you'll be asked to agree to the [CLA](CLA.md), once. Questions go in
[Discussions](https://github.com/netweir/netweir/discussions), and
[ROADMAP.md](ROADMAP.md) says what's coming next.

## Licence

AGPL-3.0. If that doesn't work for your company, a commercial licence is
available: see [netweir.com/licence](https://netweir.com/licence/) or email
legal@netweir.com. See NOTICE for the licences of what Netweir bundles.

The Netweir name and logo are trademarks of James Conway; the
[trademark policy](https://netweir.com/trademark/) says how you can use them.
To sponsor Netweir, see [GitHub Sponsors](https://github.com/sponsors/jamesconwaydev)
or email sponsors@netweir.com. Anything else: hello@netweir.com.

Please use it the way you'd want your own site scraped. Crawls obey
robots.txt and TDMRep unless you switch that off, and Netweir tells you when
you have. A one-off `netweir.get()` checks neither; that's on you.
