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
| `netweir.auto.ArticleList` | a blog's index or a news section: each article's headline, link, date, authors, image and teaser |
| `netweir.auto.ArticleNavigation` | the same page as where it leads: article links and the next page |
| `netweir.auto.JobPostingNavigation` | a jobs board or a job search: advert links and the next page |
| `netweir.auto.ForumThread` | a forum thread: its topic and each post's text, date, likes and replies |

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

## Which type a page is

A crawl that wanders reaches product pages, category pages, the blog
and the careers page in one run. `page.page_type()` says which is
which: `"product"`, `"product-list"`, `"job-posting"`,
`"job-posting-navigation"`, `"forum-thread"`, `"article-list"`,
`"article"`, or `"other"`. It runs each type's reader and keeps the one with the
strongest evidence, so a product page with a long description is still
a product, and a category page whose cards are marked up as products is
a listing. A page that's only prose (an About page, terms and
conditions) is `"other"` unless something says it's an article: a
byline, a publication date, or `og:type` article. A home page showing a
grid of priced products reads as a listing, which it is.

```python
kinds = {
    "product": netweir.auto.Product,
    "product-list": netweir.auto.ProductList,
    "job-posting": netweir.auto.JobPosting,
    "forum-thread": netweir.auto.ForumThread,
    "article-list": netweir.auto.ArticleList,
    "article": netweir.auto.Article,
}

def parse(self, page):
    kind = page.page_type()
    if kind in kinds:
        yield page.extract(kinds[kind])
```

`netweir extract URL` does the same when you leave out `--type`.

## Pages that fit no type

A register of licences, a library catalogue, a directory of branches: no
type covers them, but they all repeat. `page.records()` finds the
page's main repeated region (a table's rows, a run of cards, a list) and
gives you the rows with column names worked out for you.

```python
found = page.records()
found.rows[0]
# {"name": "Ada Byron", "name_url": "https://example.org/l/1",
#  "licence_no": "L-001", "issued": "2024-01-03"}
found.columns[0]
# {"name": "name", "selector": "td:nth-child(1)"}
found.selector
# "table.results > * > tr:has(> td), table.results > tr:has(> td)"
```

Columns are named by the table's header, by a label beside the value
(`<dt>Phone</dt>`, `Area: 120 m²`), by the element's `itemprop` or
class, and otherwise by what the value is: `heading`, `date`, `link`,
`image`, `text`. A link's URL sits in its own column, after its text
(`title` and `title_url`), and links and images are absolute. A row
without a column has None there.

Menus, footers and prose don't count: a region needs two columns of
text or images. The biggest region wins, and it stops at 5000 rows. When
it picks the wrong one, or names a column badly, the selectors are there
to copy into your own `netweir.Item`. Each column's selector is written
from one row, so check it on a row shaped differently from the rest
(one cell spanning two, an element only some rows have). `records()` returns None when
nothing repeats.

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
that isn't one. A page whose structured data lists several products (or
jobs) and links the one read to another page gets 0.3: it's a listing,
and the item is its first entry. Filter on it when a crawl reaches pages
of every kind.

`sources` names the source of every field that has a value. JSON-LD comes
first, then microdata, then the app's state in its scripts (`js:`), then
meta tags, then the layout (`dom:`). For microdata and the layout the
source includes the CSS path of the element, which is the selector you'd
use to pin that field. `validation_messages` lists problems by field: JSON-LD and
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

## A whole site from one URL

You have a shop's address and want every product on it. You don't
want to work out where its categories live or how it pages. The
templates do that part:

```
netweir crawl ecommerce --url https://shop.example/ -o products.jsonl
netweir crawl article --url https://blog.example/journal -o posts.jsonl
netweir crawl job --url https://jobs.example/search?q=baker -o jobs.jsonl
```

Each reads the navigation on every listing it reaches (the items, the
next page, the sub-categories) with the navigation types above, and
extracts the items it lists. No selectors, and no Python per page.
`--strategy` says how far it goes:

| Strategy | Goes |
|---|---|
| `navigation` | through sub-categories and next pages, taking every item listed |
| `pagination_only` | through the start listing's pages only |
| `full` | everywhere on the site; any page that is an item gives one, even if no listing shows it |
| `direct_item` | nowhere: the start URLs are the items. For checking a few pages every day |
| `automatic` | `full` from a home page, `navigation` from anywhere else. The default |

Every strategy stays on the start URLs' sites. The usual settings stop
it sooner: `-s max_pages=500`, `-s max_items=1000`, `-s max_depth=4`.

They're Spiders, so you can subclass one, for instance to pin a field
the site gets wrong:

```python
class Lumo(netweir.auto.Product):
    price = netweir.css("span.sale")


class Shop(netweir.templates.Ecommerce):
    start_urls = ["https://shop.example/c/lamps"]
    strategy = "navigation"
    item = Lumo
```

The rules they're made of work in any spider.
`netweir.Follow(navigation=netweir.auto.ProductNavigation, part="items",
extract=Lumo)` follows the items a page lists. `part="next_page"` and
`part="sub_categories"` follow the rest of its navigation. And
`netweir.Extract(Lumo, when="product")` takes the item from any page
the rules reach that reads as a product.

## Field readers on their own

The readers behind the fields work on any text:
`netweir.parse_gtin`, `netweir.parse_rating`, `netweir.parse_availability`
and `netweir.parse_salary`, beside `parse_price` and `parse_date`. See
[Selecting](selecting.md#codes-ratings-stock-and-salaries).

## Pages without structured data

A product page that only shows its product still has a heading, a price
near it and a buy button. Netweir takes the heading as the name and the
price closest to it as the price, as long as that price is marked as one
or has a buy button beside it. A struck-through price beside it, or one
written "Was £120" or "RRP £120", becomes `regular_price`. The images
around them, an "In stock" or "Sold out", labelled codes
(`SKU: ARC-01`), the description and the breadcrumb trail fill in the
rest. Prices inside a grid of cards never count, and neither do "Save
£20" badges or prices in a sentence ("Tickets £15", "Products from
£19"), so the "You may also like" strip under a product can't pass for
it, and a category page asked for a product stays close to zero.

Category and search pages are read from their cards: the biggest group
of elements that look alike, most of them priced, each giving a name, a
link, a price and an image. The next page comes from `rel=next` or from
the pagination widget's "Next" link (a disabled one is skipped), and
`page_number` from the widget or the URL, on pages that have a listing.
A job advert without a JobPosting gives its title, salary, location, job
type and closing date when the page shows an "Apply now" button, a
salary written as adverts write it ("£12.50 an hour"), or two of those
labelled facts.

These guesses carry lower probabilities than structured data: 0.5 for a
job, 0.55 to 0.7 for a product, 0.75 for a listing, and 0.3 for the
cards on a page that shows a product of its own. Every field says
`dom:` and the element it came from, so you can see at a glance which
ones to check and pin.

## Lists of articles, jobs boards and threads

A blog's index, a news section or a jobs board lists its entries in
structured data often enough (a Blog's posts, an ItemList, several
JobPostings), and Netweir reads them from there first. Without it, the
entries are the page's biggest group of look-alike cards each led by a
heading that links somewhere: the card's heading is the headline, its
`<time>` the date, its byline the author, its image and first paragraph
the rest. Cards with no dates or bylines (a shop grid without prices, a
jobs board, a menu of links) give only 0.35, so they don't type a page
as a list of articles. A story with "related stories" under it is still
a story, and a grid of priced cards is a shop's, so neither reads as a
list with any confidence either. Cards alone can't tell a jobs board
from a blog, so `JobPostingNavigation` read only from the layout says
0.35 too.

A forum thread is a DiscussionForumPosting (or a Question with its
answers) and its comments, replies to replies included, with likes and
replies from their interaction counts. Without structured data, the
posts are the biggest group of look-alike blocks each with a date or a
poster (a username, a link to a member's profile), most of them naming
who wrote them, so a changelog, a blog's dated teasers or a member
directory aren't a thread. A post's text is everything in it but its
date, poster and buttons, every paragraph and any quotes of other
posts; likes are read as people write them (`1.2k`). The markup of
XenForo, phpBB, vBulletin, Invision Community, bbPress, Discourse (its
crawler view), GitHub's issue comments, Reddit (replies to replies
included) and question-and-answer pages is read this way,
along with hand-written forums that mark their posters. Reviews under a
product and comments under an article stay that page's, not a thread.
Disqus, whose comments arrive by script, isn't.

## Pages built in the browser

Shops built with Next.js, Nuxt or Apollo often send a page whose HTML is
an empty `<div>`, with the product sitting in a script for the browser
to draw. Netweir reads that state without running anything:
`__NEXT_DATA__`, Nuxt's `__NUXT__` and `__NUXT_DATA__`, Apollo's cache
(references followed), and the `__INITIAL_STATE__`-style objects
Redux apps assign. It looks for objects shaped like a product (a name
and a price, plus codes, images, a brand) or a job (a title plus a
salary or job type, and more besides), whatever key the framework
filed them under. A product that stands alone beats one in a list, and
anything under a basket, a delivery option or recommendations is
passed over, so related products and the basket don't win. A list of
three or more priced products fills a listing, except on a product's
own page, where such lists are its variants and related products.

These fields say `js:` and the path to the value, such as
`js:__NEXT_DATA__.props.pageProps.product.price`, so you can find the
value in `page.js_data("__NEXT_DATA__")` yourself. They come after
JSON-LD and microdata but before meta tags and the layout, and an item
read from state carries a probability of 0.7 to 0.85.
