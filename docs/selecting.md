# Selecting

A page, and every element in it, answers CSS, XPath and Beautiful Soup
style queries. They return the same kinds of things, so you can mix them.

```python
import netweir

doc = netweir.parse("""
<ul class="books">
  <li class="book"><a href="/a" title="Book A">A</a><span class="price">£1.00</span></li>
  <li class="book sold"><a href="/b" title="Book B">B</a><span class="price">£2.50</span></li>
</ul>
""")
```

`netweir.parse(html)` gives you a document without fetching anything;
`netweir.get(url)` gives you a page you query the same way.

## CSS

```python
doc.css("li.book a::attr(href)").getall()    # ['/a', '/b']
doc.css(".price::text").get()                # '£1.00'
doc.css("li.sold").get()                     # '<li class="book sold">...</li>'
doc.css("a::attr(title), .price::text").getall()
# ['Book A', '£1.00', 'Book B', '£2.50']: each selector keeps its ending,
# and results come back in document order
```

- `::text` gives an element's own text nodes; ` ::text` (with a space)
  gives every text node below it.
- `::attr(name)` gives an attribute's value.
- With neither, `get()` gives the element's HTML, the way Chrome's
  `outerHTML` writes it.

## XPath

All of XPath 1.0, plus the functions Scrapy users rely on:

```python
doc.xpath("//li[span[contains(., '2.50')]]/a/@title").get()   # 'Book B'
doc.xpath("count(//li)").get()                                # '2.0'
doc.xpath("//li[has-class('sold')]/a/text()").get()           # 'B'
doc.xpath("//a[re:test(@href, '^/[ab]$')]/@href").getall()     # ['/a', '/b']
doc.xpath("//li[a/@title = $t]/span/text()", t="Book A").get() # '£1.00'
```

Keyword arguments to `xpath()` bind `$variables`. A query that can't be
parsed raises `netweir.XPathError`; a CSS selector that can't be parsed
raises `netweir.SelectorError`.

## Selections

`css()` and `xpath()` return a `Selection`, a list of results:

| | Gives |
|---|---|
| `get(default=None)` | the first result as a string, or `default` |
| `getall()` | every result as a string |
| `re(pattern)` | every match of a regular expression in the results (the groups if it has any) |
| `re_first(pattern, default=None)` | the first of those |
| `attrib` | the first element's attributes, as a dict |
| `sel[0]`, `sel[1:3]`, `len(sel)` | indexing and slicing |
| `sel.css(...)`, `sel.xpath(...)` | a query run below every element in it |

Indexing gives a `Node` for an element and a `str` for text or an
attribute's value. `extract()` and `extract_first()` are the older names
for `getall()` and `get()`.

## Beautiful Soup style

The same document answers Beautiful Soup's methods:

```python
import re

doc.find("a")["href"]                           # '/a'
[a["title"] for a in doc.find_all("a")]         # ['Book A', 'Book B']
doc.find("li", class_="sold").find("a").string  # 'B'
doc.find_all(string=re.compile("£"))            # the two prices
doc.find("span").find_parent("li")["class"]     # 'book'
```

`find`, `find_all` and the directional families (`find_parent(s)`,
`find_next_sibling(s)`, `find_previous_sibling(s)`, `find_next`,
`find_all_next`, `find_previous`, `find_all_previous`) take the filters you
know: a string, a list, `True`/`False`, a compiled regex or a function, with
`class_=`, `string=` (or `text=`), `limit=` and `recursive=`. Navigation
(`parent`, `children`, `contents`, `descendants`, siblings,
`next_element`), `get_text()`, `.string`, `select()` and `select_one()`
work as in Beautiful Soup.

Two differences, both on purpose:

- Attribute values are strings. `node["class"]` is `"book sold"`, not a
  list; filters still match one class out of several.
- `.text` and `get_text()` leave out the contents of `<script>`, `<style>`
  and `<template>`, as Beautiful Soup 4.10 and later do.

## Following an element through a redesign

Give a query a name with `track=`, and netweir remembers what its element
looked like. When a redesign breaks the query, netweir finds the most
similar element instead. See [Self-healing](self-healing.md#tracked-selectors).

## Structured data

Many pages describe themselves for search engines and social networks:
a product's name, price and rating in JSON-LD, an article's title and image
in Open Graph tags, reviews marked up as microdata. When it's there, it's
usually the cleanest copy of the data on the page, and `page.metadata()`
reads all of it at once:

```python
meta = page.metadata()

for thing in meta["json_ld"]:  # every <script type="application/ld+json">
    if thing.get("@type") == "Product":
        price = thing["offers"]["price"]

meta["microdata"]   # [{"type": "https://schema.org/Review", "properties": {...}}]
meta["opengraph"]   # {"og:title": "...", "og:image": [...], "product:price:amount": "..."}
meta["twitter"]     # {"twitter:card": "summary_large_image", ...}
meta["dublin_core"] # {"dc.title": "...", ...}
```

JSON-LD comes as written, with a block that's a list giving its items one
by one; a block that isn't valid JSON is skipped. Microdata follows the
HTML standard: nested items, `itemref`, several names in one `itemprop`,
and each element's value where the standard says to find it (`content`,
`href` and `src` made absolute against the page, `datetime`, `value`, or
the text). In the meta tag sections and in microdata, a key given more than
once has a list of its values. A parsed document has it too:
`netweir.parse(html).metadata(base_url)`.


## Prices, dates and data in scripts

Three things turn up on nearly every job, and each is a regular
expression that works on the first site and breaks on the second. netweir
reads all three in Rust.

### Prices

```python
netweir.parse_price("£1,234.50")       # Price(amount=Decimal('1234.50'), currency='£')
netweir.parse_price("1.234,50 €")      # Price(amount=Decimal('1234.50'), currency='€')
netweir.parse_price("From USD 1,099")  # Price(amount=Decimal('1099'), currency='USD')
netweir.parse_price("Sold out")        # None

netweir.parse_price("£5").code         # 'GBP'
netweir.parse_price("$5").code         # None: US, Canadian, Australian?
```

The amount is a `Decimal`, never a float, so `19.99` stays `19.99`.
`currency` is what the page wrote next to the number (`"£"`, `"zł"`,
`"US$"`, `"EUR"`), and `code` is its ISO 4217 code when it names one
currency only: `$`, `¥` and `kr` don't.

Which separator is the decimal point comes from the text. The last of
`.` and `,` is the decimal point when one or two digits follow it, or
when the other separator comes before it: `12,99` is 12.99 and
`1.234,56` is 1234.56. Three digits after a lone separator make it a
thousands separator, so `1,234` and `1.234` are both 1234, except where
that can't be right (`0.500` is a half). Spaces, thin spaces and
apostrophes (`1'234.50`) group thousands too, and so do Indian lakhs
and crores (`₹1,23,456`). The first number in the text is the price:
`From £10 to £20` is 10.

A few more shapes are read as you'd expect: `$.99` is 0.99; `299,- kr`
and `49,– €` are whole amounts with their currency after the dash; and
full-width digits (`￥１，２３４`) are ordinary digits. A minus sign
makes the amount negative when it's straight before the number or
before a currency straight before it (`-£12.00`, `−5 €`), or between
the currency and the number (`£-5`, `€ -12,50`). Refunds and discounts
export as `"-12.00"`. It has to stand on its own, at the start or after
a space or an opening bracket, so the hyphens in `2-for-$5` and the
`:-` in `MRP:-₹499`, which Indian shops use to label a price, aren't
signs.

### Dates

```python
netweir.parse_date("2026-10-12T09:30:00Z")           # 2026-10-12 09:30:00+00:00
netweir.parse_date("Mon, 12 Oct 2026 09:30:00 GMT")  # 2026-10-12 09:30:00+00:00
netweir.parse_date("October 12th, 2026 at 9:30 PM")  # 2026-10-12 21:30:00
netweir.parse_date("12 de octubre de 2026")          # 2026-10-12 00:00:00
netweir.parse_date("03/04/2026")                     # 4 March
netweir.parse_date("03/04/2026", day_first=True)     # 3 April
netweir.parse_date("3 days ago", now=page_fetched_at)
```

It reads ISO 8601 and RFC 3339, RFC 2822 and HTTP dates, numeric dates
(`12/10/2026`, `12-10-26`, `12.10.2026`, `2026/10/12`), and month names,
full or short, with or without weekdays and ordinals, in English, French,
German, Spanish, Italian, Portuguese and Dutch (`12. Oktober 2026, 09:30
Uhr`, `am 12. 10. 2026 um 9 Uhr`, `lundi 12 octobre 2026 à 9h30`).
Times can be `09:30`, `9:30:15.25`, `9.30pm`, `9 a.m.`, `21h30` or
`21 Uhr`.

A few rules decide the hard cases:

- Numeric dates with slashes or dashes are month first, as US English
  writes them, and ones with dots (`03.04.2026`) are day first, as every
  country that writes dots does. `day_first=True` or `False` decides for
  all of them. A number over 12 can only be the day, so `13/04/2026` is
  13 April either way.
- A two-digit year is 19xx or 20xx, whichever is nearer `now`.
- A zone makes the datetime aware: `Z`, offsets like `+01:00` or
  `-0500`, `UTC+1`, and the names UTC, UT, GMT, EST, EDT, CST, CDT, MST,
  MDT, PST, PDT, AKST, AKDT, HST, WET, WEST, BST, CET, CEST, MET, MEST,
  MEZ, MESZ, EET, EEST, MSK, JST, KST, HKT, SGT, PHT, AWST, ACST, ACDT,
  AEST, AEDT, NZST and NZDT. CST is US Central, as in RFC 2822, not China;
  IST is left out because it could be India, Ireland or Israel. What
  JavaScript's `Date.toString()` writes works too: once there's an offset,
  a trailing name in brackets (`GMT+0100 (Central European Standard
  Time)`) is skipped, in any language, though a single zone name in
  brackets that disagrees with the offset (`-05:00 (CST)`) makes the
  date None. An offset can also stand alone at the end, after a space
  (`12 Oct 2026 +0100`). Without a zone, the datetime is naive.
- Relative phrases are English: `now`, `today`, `yesterday at 9:30`,
  `3 days ago`, `an hour ago`, `in 2 weeks`, `2 hours, 5 minutes ago`.
  They count from `now`, the current time unless you pass one (a `date`
  counts as its midnight), and keep its zone. Pass the time you fetched
  the page and `3 days ago` means what the page meant. `today`,
  `yesterday` and `tomorrow` without a time are midnight, like any date
  without a time.
- In a zone with summer time, hours, minutes and seconds are elapsed
  time, so `1 hour ago` is always an hour before, even across the night
  the clocks change. Days and longer are on the calendar: `1 day ago` is
  the same clock time yesterday.

Anything netweir doesn't fully understand is None, not a guess: a date
without a year (`12 Oct`), an impossible one (`31/04/2026`), or a date
with words around it (`Posted 12 Oct 2026`; take the date out with `re=`
first).

### Data in scripts

Plenty of pages ship their data as JavaScript: `window.__INITIAL_STATE__
= {products: [...], 'total': 42,}`. `json.loads` gives up on the
unquoted keys, single quotes and trailing comma. `netweir.js_data` reads
it:

```python
netweir.js_data("var s = {name: 'Lamp', tags: ['a', `b`], sold: undefined,};")
# {'name': 'Lamp', 'tags': ['a', 'b'], 'sold': None}

page.js_data("__INITIAL_STATE__")  # the value assigned to that name
page.js_data("__NEXT_DATA__")      # Next.js: the JSON script with that id
page.js_data()                     # every object and array in every script
```

`js_data(text)` gives the first object or array literal in `text`, and
`js_data(text, all=True)` a list of every one. Empty `{}` and `[]` are
skipped: in code they're nearly always a function body or a default. So
is a `[` straight after a name or a bracket (`items[0]`, `f()[1]`),
which is indexing, not an array, unless the name is a keyword that
minified code writes against an array: `return[1,2]`, `of[`, `in[`,
`case[`, `typeof[`, `void[`, `else[`, `do[`, `yield[`, `await[`,
`delete[` and `throw[` all start arrays.
Beyond JSON it reads unquoted and single-quoted keys, single-quoted and
backtick strings (`${...}` kept as written), trailing commas, comments,
`undefined` (None), `NaN`, `Infinity`, hexadecimal numbers, minified
`!0`, `!1` and `void 0`, `new Date(...)` (its argument), and
`JSON.parse('...')` (the string decoded, then read).

It reads data, not code. Nuxt 2's `window.__NUXT__=(function(a,b){return
{...}}(...))`, object shorthand (`{a, b}`), spread (`...rest`),
functions, regular expressions, BigInt (`10n`) and numeric separators
(`1_000`) aren't read: a literal with any of them in it is skipped, though
an object or array nested inside it can still turn up on its own when
you search without a name.

`page.js_data(name)` looks through the page's scripts for `name` being
given a value: `window.name = ...`, `var name = ...`, `name: ...`,
`"name": ...` or `window["name"] = ...`, or a JSON script whose id is
`name`. It gives the first one it finds, or None. Without a name it lists
every object and array in every script, reading a `<script
type="application/json">` whole. On a page full of code that's noisier
than asking for a name: minified bundles hold plenty of small literals
that aren't the data you want. Templates (`<script type="text/template">`)
aren't scripts and are left alone. A parsed document or any element has
it too: `netweir.parse(html).js_data("__STATE__")`.

Script text can be built to hurt a parser, so nesting stops at 400 levels
and the search through a script stays proportional to its length.

### In items

All three are plain functions, so they work as `into=` in an `Item`:

```python
class Product(netweir.Item):
    name = netweir.css("h1::text")
    price = netweir.css(".price::text", into=netweir.parse_price)
    published = netweir.css("time::attr(datetime)", into=netweir.parse_date)
```

A `Price` exports as `{"amount": "1234.50", "currency": "£"}`, the amount
as a string so no digits are lost, and a datetime as ISO 8601.
