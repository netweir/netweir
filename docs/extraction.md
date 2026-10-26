# Auto-extraction: data without selectors

Most shops, job boards and news sites already describe their pages in
structured data, for search engines: the product's name, price and stock,
the job's salary and location, the article's author and date. Netweir
reads it, in Rust, and where a page has none, or leaves a field out, it
reads the page the way you would: the heading, the price beside it, the
grid of cards. You get the item in a fixed shape, out of a page you've
never written a selector for:

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

A price is kept as the page wrote it. In structured data that's
schema.org's form, a point for decimals and nothing else, so `"59.990"`
stays `"59.990"`; a price written as a JSON number has lost its trailing
zeros before Netweir sees it, so `12.50` comes out `"12.5"`. A price
read from the page's text (a pinned field) is read as people write it,
so a German shop's `1.299 €` is `"1299"`.

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
first, then microdata, then meta tags, then the layout (`dom:`); for
microdata and the layout the source includes the CSS path of the element,
which is the selector you'd use to pin that field. `validation_messages` lists problems by field: JSON-LD and
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

## Pages without structured data

A product page that only shows its product still has a heading, a price
near it and a buy button. Netweir takes the heading as the name and the
price closest to it as the price. A struck-through price beside it
becomes `regular_price`. The images around them, an "In stock" or
"Sold out", labelled codes (`SKU: ARC-01`), the description and the
breadcrumb trail fill in the rest. Prices inside a grid of cards never
count, so the "You may also like" strip under a product can't pass for
it, and a category page asked for a product stays close to zero.

Category and search pages are read from their cards: the biggest group
of elements that look alike, most of them priced, each giving a name, a
link, a price and an image. The next page comes from `rel=next` or from
the pagination widget's "Next" link (a disabled one is skipped), and
`page_number` from the widget or the URL. A job advert without a
JobPosting gives its title, salary, location, job type and closing date
when the page shows an apply button, a salary or those labelled facts.

These guesses carry lower probabilities than structured data (0.55 to
0.75), and every field says `dom:` and the element it came from, so you
can see at a glance which ones to check and pin.

## What it doesn't do yet

Apps built with Next.js, Nuxt or Apollo keep their product data in a
script rather than in the HTML; reading that is the next step. Until
then, a page that renders its product only in JavaScript needs
`--browser` (or a browser crawl) for the layout to have anything to
read.
