<p align="center"><img src="https://raw.githubusercontent.com/netweir/netweir/main/docs/assets/header.gif" alt="netweir: Rust does the work. You write Python." width="100%"></p>

# netweir

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

- Fetching that is indistinguishable from Chrome 154, Firefox 156 or
  Safari 27 on the wire, over HTTP/2 and HTTP/1.1, with cookies and
  redirects. Below: how that's proved, and the one known exception.
- Forms, logins and JSON APIs: POST and any other method, sent the way
  Chrome, Firefox or Safari sends a form or a page's own script, and forms read off
  a page the way a browser submits them. See
  [docs/forms.md](docs/forms.md).
- A parser that keeps pace with selectolax, the one to beat in Python, and
  pulls data out of the page faster.
- CSS queries with Scrapy's `::text` and `::attr()`, all of XPath 1.0, and
  Beautiful Soup's `find_all` family, each faster than the library you'd
  otherwise use for it.
- Structured data: `page.metadata()` reads a page's JSON-LD, microdata,
  Open Graph, Twitter cards and Dublin Core in one call, in Rust.
- Crawling: spiders, sitemaps, robots.txt, per-site throttling, and items
  written to JSON Lines, JSON, XML, CSV or Parquet, on disk or in cloud
  storage, in one file or in batches. Middlewares, signals and
  pipelines with `open_spider`/`close_spider` work as Scrapy's do, so
  simple Scrapy pipelines and middlewares carry over as they are.
  `allowed_domains` keeps a crawl on the sites you meant; `max_items`,
  `max_pages`, `max_errors`, `max_time` and `max_memory` stop it when
  you've seen enough.
- `netweir shell URL`, `fetch` and `view`: the page netweir gets, at a
  Python prompt, on stdout or in your browser, for trying selectors before
  they go in a spider. See [docs/shell.md](docs/shell.md).
- Validating items against a dataclass or a JSON Schema, and checks at
  the end of a crawl (item counts, error rate, field coverage) that make
  `netweir crawl` exit 1 when a crawl finished but didn't work.
- A download handler for Scrapy, so a Scrapy project can fetch as a
  browser does without moving off Scrapy. See
  [docs/crawling.md](docs/crawling.md#or-keep-scrapy-and-fetch-with-netweir).
- A response cache for the days you spend writing a spider: run it thirty
  times and the site serves its pages once. See
  [docs/crawling.md](docs/crawling.md#caching-while-you-develop).
- Declarative spiders, where you describe the item and the links and
  Rust does the rest without running any Python per page.
- Callbacks in worker processes, for spiders whose own Python is the slow
  part: `workers=4` ran a heavy spider 3.1 times faster, with the same
  output.
- Self-healing: block pages recognised and recovered from, crawls that
  resume after a crash, and selectors that find their element again after
  a redesign.
- Proxy pools: a new proxy for every request, one per site, or one only
  after a block, with proxies that keep failing benched for a while. See
  [docs/self-healing.md](docs/self-healing.md#proxies).
- A Chrome driver for pages that need JavaScript: click, type and wait
  like Playwright, then read the page with netweir's selectors. In a
  crawl, it can take over a request a site keeps blocking, and hand the
  cookies it earns back to the fast HTTP client. `netweir install chrome`
  fetches the Chrome it's tuned for.

Coming next: Firefox in the driver, once there's a Firefox that doesn't
announce it's automated. The designs are in [docs/design/](docs/design/).

## Install

```
pip install netweir
```

There are wheels for Linux, macOS and Windows, on Python 3.10 and up,
free-threaded 3.14 included, so there's nothing to compile. Pages that
need a real browser want Chrome too: `netweir install chrome` fetches the
version netweir is tuned for.

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
it spent waiting on purpose. Out of the box netweir reads each site's
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

Three things go wrong on a long crawl, and netweir handles each without
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

**The site is redesigned.** Name a selector and netweir remembers what its
element looked like:

```python
price = page.css(".price_color::text", track="price")
```

When a new build renames `price_color`, wraps it in another `div` or swaps
`h1` for `h2`, the selector stops matching, and netweir finds the element
most like the one it remembers. `price.relocated` says it had to, and
`price.score` says how sure it is. You get a warning, not an empty column.
`track=` works on Item fields too. Add `repair = netweir.repair.llm()` to a
spider, and a language model proposes a replacement selector for each one
that broke. netweir checks every proposal on the page and reports it to you,
and never applies one itself.

## Looking like a browser

Most sites don't block scrapers by reading their code. They block them by
the first few hundred bytes of the connection: which TLS ciphers and
extensions arrive, what the HTTP/2 settings frame says, which headers come
first. A Python HTTP library gives itself away before it has
asked for anything.

netweir sends what Chrome 154 sends. Not something close to it: the same
ClientHello, the same HTTP/2 settings and priorities, the same headers in
the same order and the same capitalisation over HTTP/1.1. Cookies go where
Chrome puts them, and redirects are followed hop by hop the way Chrome
follows them, header order after a redirect included.

You don't have to take that on trust. Chrome was recorded doing four
navigations against a local server: a page that sets a cookie, a revisit,
a redirect to another site and one within the site. `cargo test -p
netweir-core` has netweir do the same four and fails if any request differs
from Chrome's. The public checker at tls.peet.ws reports the same JA4
fingerprint for both: `t13d1517h2_8daaf6152771_cb7bf5808d99`.

Firefox 156 is there too: `netweir.get(url, profile="firefox")`, or
`profile="firefox"` in a spider's settings. Firefox sends its TLS extensions
in a fixed order, with no GREASE, and the same test checks that order
against a recording of the real browser, as well as everything above.

So is Safari 27, as `profile="safari"`. It has two habits of its own, and
netweir copies both. Two cookies with the same path go out newest first.
And once a site has answered over HTTP/1.1, Safari stops offering it
HTTP/2 on new connections, so netweir does the same.

One known difference: Chrome sends a `priority` header only over HTTP/2,
and netweir can't tell a server lacks HTTP/2 until it has answered once. So
the first request to an https server that only speaks HTTP/1.1 carries that
header; every request after it doesn't.

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

Pages are parsed by [lexbor](https://github.com/lexbor/lexbor), the same
spec-compliant C parser selectolax uses. The difference is what happens
after: netweir runs the whole query in Rust and hands Python a finished list
of strings, instead of a Python object for every element along the way. The
GIL is released while it works, so threads parse in parallel.

`bench/parse.py` pulls every price, title and link out of a generated shop
page:

| | 1 MB page | 10 MB page |
|---|---|---|
| netweir | 5.0 ms | 61 ms |
| selectolax | 5.3 ms | 64 ms |
| BeautifulSoup (lxml) | 196 ms | 2.3 s |
| lxml + cssselect | 347 ms | 82 s |
| parsel | 353 ms | 82 s |

Measured on an Apple M-series laptop with Python 3.14. The lead over selectolax is small, because both parse with lexbor, and it comes from doing the extraction in Rust. On other machines the two can swap places by a few percent; on CI's Linux runners they do. Run it yourself:
`uv run --group bench python bench/parse.py`.

XPath and `find_all` are a different story, because the libraries people
use for them are much slower. `bench/query.py` parses the same shop page and
asks six XPath questions, or six Beautiful Soup ones:

| | 1 MB page | 3 MB page |
|---|---|---|
| netweir, XPath | 9.9 ms | 33 ms |
| lxml, XPath | 809 ms | 8.8 s |
| parsel, XPath | 814 ms | 8.8 s |
| netweir, `find_all` | 9.4 ms | 29 ms |
| Beautiful Soup, `find_all` | 170 ms | 515 ms |

Look at lxml across the row: three times the page took eleven times as long.
netweir builds a flat index of the document on the first query, so `//x` is
a scan over a few arrays, and its time grows with the page and no faster.
Both benchmarks run in CI, and the build fails if any library beats netweir.

Parsing is one part of a crawl. `bench/crawl.py` times the whole thing: one
local server plays 100 sites of 100 pages, each answer 50 ms late, and every
crawler fetches all 10,000 pages and pulls a title and price from each,
with the same limits (100 requests in flight, 8 per site):

| | Pages a second | CPU per page | Peak memory |
|---|---|---|---|
| netweir | 1,770 | 0.22 ms | 68 MB |
| Scrapy 2.19 | 890 | 1.1 ms | 125 MB |
| httpx + selectolax | 220 | 3.2 ms | 190 MB |
| Scrapling 0.4 | 180 | 0.8 ms | 76 MB |

With 100 requests in flight and 50 ms per answer, 2,000 pages a second is
the most any crawler could do here, so netweir is waiting on the server,
not on itself. Scrapling is held back by its HTTP session's default of 10
connections, which it doesn't let you change; that is how it ships. Same
laptop, Python 3.13; run it yourself with
`uv run python bench/crawl.py` once the others are installed.

Pages you don't control can be built to be slow to parse. Pass a timeout and
netweir gives up instead of hanging:

```python
netweir.parse(html, timeout=2.0)  # raises netweir.ParseTimeout
```

The timeout bounds time, not memory. A crawl reads at most 64 MiB of a
response, counted after decompression, so a huge page or a small compressed
one that unpacks to gigabytes fails instead of filling memory
(`max_response_size` sets the limit; `max_size=` does the same for
`netweir.get`). It doesn't stop a small page built to abuse the HTML5
spec's rules for misnested formatting tags, which can make any
spec-compliant parser allocate gigabytes; nothing in netweir caps the
parser's memory yet.

## When a page needs a real browser

Some pages are empty until their JavaScript runs. For those, netweir
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
DevTools features that page scripts can notice. netweir doesn't do any of
that. A test serves a page that looks for the first two from script, and
also fails if any of those DevTools features was switched on.
[docs/browser.md](docs/browser.md) has the rest, including what it doesn't
hide yet.

## Documentation

[docs/](docs/README.md) has a getting-started guide, a page for each part of
netweir and every setting with its default.

## Building

You need Rust, a C compiler, CMake and [uv](https://docs.astral.sh/uv/).

```
git clone --recurse-submodules https://github.com/netweir/netweir
cd netweir
uv sync --group dev
uv run maturin develop --uv
uv run pytest
```

uv installs the dependencies and maturin builds netweir, so after changing
any Rust, rerun `uv run maturin develop --uv` before testing.

## Contributing

Issues, fixes and new browser profiles are all welcome.
[CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and the one
hard rule (write it yourself), and before your first pull request is merged
you'll be asked to agree to the [CLA](CLA.md), once. Questions go in
[Discussions](https://github.com/netweir/netweir/discussions).

## Licence

AGPL-3.0. If that doesn't work for your company, a commercial licence is
available. See NOTICE for the licences of what netweir bundles.

Please use it the way you'd want your own site scraped. Crawls obey
robots.txt and TDMRep unless you switch that off, and netweir tells you when
you have. A one-off `netweir.get()` checks neither; that's on you.
