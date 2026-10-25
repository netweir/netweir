# Auto-extraction: data without selectors

Most shops, job boards and news sites already describe their pages in
structured data, for search engines: the product's name, price and stock,
the job's salary and location, the article's author and date. Netweir
reads it, in Rust, and hands you the item in a fixed shape, so you can get
a product out of a page you've never written a selector for:

```python
import netweir

page = netweir.get("https://shop.example/p/arc-lamp")
product = page.extract(netweir.auto.Product)
product["name"], product["price"], product["currency"]
# ("Arc desk lamp", "49.99", "GBP")
```

From the command line, `netweir extract URL` prints the same item as JSON
(see [Trying a page](shell.md#extract)).

## The types

| Class | What it reads |
|---|---|
| `netweir.auto.Product` | a product page: name, price and regular price, currency, stock, SKU, MPN, GTINs, brand, images, description, rating, breadcrumbs, colour, size, a product group's variants |
| `netweir.auto.ProductList` | a category or search page: its products (name, price, URL, image), breadcrumbs, category name and next page |
| `netweir.auto.ProductNavigation` | the same page as where it leads: product URLs and the next page |
| `netweir.auto.JobPosting` | a job advert: title, organisation, location, salary range and rate, employment type, requirements, remote status, dates |
| `netweir.auto.Article` | an article: headline, authors, dates, language, images, videos, description and the body as text and HTML |

Field names are the common item schema's, the one Zyte's API and
zyte-common-items use, in snake_case: `regular_price`, `main_image`,
`aggregate_rating`. Values have the same shapes too: prices are decimal
strings (`"1299.00"`, never a float), currencies are ISO 4217 codes,
dates are ISO 8601, and a brand is `{"name": ...}`. Items from fifty
different sites land in one table without a cleanup script, and items
from Zyte's API land in the same one: export with `field_names="zyte"`
to get its camelCase names.

```python
class Shop(netweir.Spider):
    pipelines = [netweir.export.jsonl("products.jsonl", field_names="zyte")]
```

A field the page doesn't give is None. Netweir doesn't guess: a price
written `$12` has `currency_raw` `"$"` and `currency` None, because `$`
alone could be any of a dozen dollars.

## Where each value came from

Every item ends with `metadata`:

```python
product["metadata"]
# {"probability": 0.95,
#  "date_downloaded": "2026-10-25T18:04:05Z",
#  "sources": {"name": "json-ld", "price": "json-ld",
#              "breadcrumbs": "microdata:nav.crumbs > ol",
#              "canonical_url": "meta:canonical"},
#  "validation_messages": {}}
```

`probability` is how sure Netweir is that the page holds one of these at
all: around 0.95 for a page with a complete schema.org Product, lower when
only its Open Graph tags say it's a product, and close to zero for a page
that isn't one. Filter on it when a crawl reaches pages of every kind.

`sources` names the source of every field that has a value. JSON-LD comes
first, then microdata, then meta tags; for microdata the source includes
the CSS path of the element, which is the selector you'd use to pin that
field. `validation_messages` lists problems by field: JSON-LD and
microdata giving two different prices, a page carrying several products,
or a pinned selector that found nothing.

## When a field is wrong for a site

Structured data is written by people, and sometimes it's stale or wrong:
the JSON-LD still has last week's price, say, while the page shows the
sale price. Open the page source, find the element that holds the right
value, and pin that one field:

```python
class Lumo(netweir.auto.Product):
    price = netweir.css("span.sale")
```

Everything else stays automatic. A pinned field is read as that field, so
`span.sale` holding `$9.50 inc. VAT` gives `"9.50"`, a `<time>` gives an
ISO date, a breadcrumb trail's `<ol>` gives the list of names and links,
and an image gives its URL, made absolute. Give it `::text` or
`::attr()` and the string is read the same way. With `into=`, your
function reads it instead.

A pinned field that finds nothing is None, with a message in
`validation_messages`. It never falls back to the automatic value
silently: if you pinned it, you meant it, and a quiet fallback would hide
a broken selector. Add `track=` and Netweir follows the element through
redesigns, as any tracked selector does (see
[Self-healing](self-healing.md#tracked-selectors)):

```python
class Lumo(netweir.auto.Product):
    price = netweir.css("span.sale", track="lumo-price")
```

Fields the type doesn't have are kept after its own, so a subclass can
add what the schema lacks:

```python
class Lumo(netweir.auto.Product):
    warranty = netweir.css("dd.warranty::text", strip=True)
```

## In a crawl

The classes are `Item`s, so they go wherever an `Item` goes. In a
declarative spider the whole extraction runs in Rust, with no Python per
page:

```python
class Lamps(netweir.Spider):
    start_urls = ["https://shop.example/c/lamps"]
    rules = [
        netweir.Follow("li.next a"),
        netweir.Follow("article.product a", extract=Lumo),
    ]
    pipelines = [netweir.export.jsonl("lamps.jsonl")]
```

In a callback, `page.extract(netweir.auto.Product)` or
`Lumo.extract(page)` does the same. On a `Node` without a page, pass the
URL its links are relative to: `netweir.auto.Product.extract(root,
url="https://shop.example/p/arc-lamp")`.

## Field readers on their own

The readers behind the fields work on any text:
`netweir.parse_gtin`, `netweir.parse_rating`, `netweir.parse_availability`
and `netweir.parse_salary`, beside `parse_price` and `parse_date`. See
[Selecting](selecting.md#codes-ratings-stock-and-salaries).

## What it doesn't do yet

Today auto-extraction reads what pages say about themselves: JSON-LD,
microdata and meta tags, plus Netweir's main-content reading for
articles. Most shops, job boards and news sites carry that data, because
search engines reward it. A page that has none (a product shown only in
its HTML, a listing without an `ItemList`) gives an item with a low
probability and few fields. Reading those from the page's layout, and
from the data apps keep in their scripts, is the next step. Until then,
pin the fields such a site needs.
