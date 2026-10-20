# Declarative spiders

A declarative spider describes its items and its links. Netweir then runs the
whole crawl in Rust: pages are parsed on worker threads, links queued and
items extracted without Python running for each page. Python sees only the
finished items.

```python
import netweir


class Book(netweir.Item):
    title = netweir.css("h1::text")
    price = netweir.css(".price_color::text", re=r"[\d.]+", into=float)
    upc = netweir.xpath("//th[.='UPC']/following-sibling::td/text()")
    stock = netweir.css(".availability::text", re=r"\d+", into=int, default=0)


class Books(netweir.Spider):
    start_urls = ["https://books.toscrape.com/"]
    rules = [
        netweir.Follow("li.next a"),
        netweir.Follow("article.product_pod h3 a", extract=Book),
    ]
```

## Items

An `Item` subclass lists fields. Each is `netweir.css(query, ...)` or
`netweir.xpath(query, ...)`, with these options:

| Option | Does |
|---|---|
| `re=` | keeps what a regular expression matches: its first group if it has one, else the whole match. Rust's regex syntax. |
| `into=` | `int`, `float`, `bool` or `str` convert in Rust. A value that won't convert becomes `None`, with one warning naming the field and the text. Any other function runs in Python on the finished value, `netweir.parse_price` and `netweir.parse_date` among them. |
| `all=True` | every result, as a list, instead of the first. Nothing found is `[]`. |
| `strip=True` | trims whitespace. |
| `default=` | the value when nothing is found (not used with `all=True`). |
| `track=` | follows the element through redesigns under that name; see [Self-healing](self-healing.md#tracked-selectors). |

Items come out as dicts, with keys in the order the fields are written. A
query or pattern that can't be parsed raises where the field is written,
not halfway through a crawl. `Book.extract(page)` runs the same extraction
inside an ordinary callback.

## Rules

`netweir.Follow(css, ...)` or `netweir.Follow(xpath=..., ...)` takes links
from every page the rules apply to:

| Option | Does |
|---|---|
| `extract=` | an Item class, extracted from each page the rule leads to |
| `callback=` | a spider method (or its name) that also gets those pages |
| `follow=` | whether the rules apply again on those pages. True by default for a rule with neither `extract` nor `callback`, False otherwise. |
| `priority=` | the priority of the requests the rule makes |
| `allow=`, `deny=` | regexes matched against each absolute link: it's taken if it matches one of `allow` (or there are none) and none of `deny` |
| `allow_domains=`, `deny_domains=` | the same by site; a domain covers its subdomains |
| `browser=` | True fetches the pages the rule leads to in Chrome, so the item and links come from the page its scripts built |
| `steps=` | what to do on those pages in Chrome before they're read, such as `[steps.click("button.load-more")]`; implies `browser=True`. See [Steps](steps.md#in-rules) |

A matched element gives its `href`; a query ending in `::attr(...)` or
`@attr` gives that value instead. Links are resolved the way a browser
resolves them, deduplicated per page, and anything but http(s) is skipped.

The rules apply to the start pages, and to requests a callback yields
without a callback of their own. A spider that defines `parse` has it run
on those pages too. Pages with an error status, and bodies that aren't HTML,
are left alone by the rules (logged at info, counted as `pages_ignored`).

## How much it saves

`bench/rules.py` crawls a local 1,050-page shop both ways. The declarative
spider takes about two thirds of the time. It uses about the same CPU, but
on other threads, so your event loop and your pipelines aren't kept
waiting.
