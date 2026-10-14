# Getting started

This page takes you from installing Netweir to a crawl that writes a file of
items. It takes about ten minutes. Each step builds on the one before.

## Install

Netweir needs Python 3.10 or later, on Linux, macOS or Windows.

```
pip install netweir
```

The wheels include everything Netweir needs, BoringSSL and the HTML parser
among it, so there's nothing else to install.

## Fetch a page

```python
import netweir

page = netweir.get("https://books.toscrape.com/")
print(page.status)                       # 200
print(page.css("h1::text").get())        # All products
```

`netweir.get` sends the request the way Chrome would and waits for the
answer. The page it returns holds the response (`status`, `url`,
`headers`) and the parsed document.

## Pull data out of it

Query the page with CSS, XPath or Beautiful Soup's methods. They all work on
the same page, so use whichever you know:

```python
prices = page.css(".price_color::text").getall()
titles = page.xpath("//article//h3/a/@title").getall()
next_link = page.find("li", class_="next").find("a")["href"]
```

`::text` gives an element's own text and `::attr(name)` an attribute's
value. Without them, `get()` gives the element's HTML. See
[Selecting](selecting.md) for everything a query can do.

## Write a spider

A spider fetches pages for you and follows links. Put this in `books.py`:

```python
import netweir


class Books(netweir.Spider):
    start_urls = ["https://books.toscrape.com/"]

    async def parse(self, page):
        for book in page.css("article.product_pod"):
            yield {
                "title": book.css("h3 a::attr(title)").get(),
                "price": book.css(".price_color::text").get(),
            }
        if next_page := page.css("li.next a::attr(href)").get():
            yield page.follow(next_page)
```

`parse` runs on every page the spider fetches. Each dict it yields is an
item; each `page.follow(...)` is another page to fetch.

## Run it

```
netweir crawl books.py -o books.jsonl
```

Netweir fetches all 50 pages of the site and writes 1,000 items to
`books.jsonl`, one JSON object per line. It reads the site's robots.txt
first and keeps a polite pace, so the crawl takes about 20 seconds. Name
the file `books.csv` or `books.parquet` to get that format instead.

## Where next

- [Trying a page](shell.md): `netweir shell URL` gives you a Python prompt
  with the page Netweir got, for trying selectors before they go in a
  spider.
- [Crawling](crawling.md): requests, callbacks, pipelines, exporters and
  the command line.
- [Projects](project.md): several spiders sharing settings, made with
  `netweir genspider`, tried with `netweir parse` and checked with
  `netweir check`.
- [Declarative spiders](declarative.md): describe items and links, and let
  Rust do the crawl without running Python on each page.
- [Self-healing](self-healing.md): block recovery, crash-safe resume and
  selectors that survive a redesign.
- [Settings](settings.md): every setting and its default.
