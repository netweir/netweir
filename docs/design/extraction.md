# Auto-extraction

What `netweir.auto` is for, how it reads a page, and the calls behind its
shape. The user guide is [docs/extraction.md](../extraction.md).

## Intent

People point Netweir at sites they know: shops, job boards, public
records, library catalogues, wikis, forums, news. They want the data out
without writing selectors first. Auto-extraction gives them a good first
guess as a typed item, and when one field is wrong, they pin that field
with a CSS selector and keep the rest automatic.

The bar is Zyte's automatic extraction and Scrapy's page objects, with
three things neither has together: it runs locally, in Rust, with no
per-page fee; every field says where it came from; and a pinned field is
an ordinary tracked selector, so self-healing keeps the fix working.

## Shape

- **Typed items with the common schema's fields.** Product, ProductList,
  ProductNavigation, JobPosting and Article match zyte-common-items field
  for field, in snake_case, with the same value shapes: decimal strings
  for money, ISO 4217 currencies, ISO 8601 dates, `{name}` brands,
  `[{type, value}]` GTINs with check digits verified (an ISBN-10 ending in
  X becomes its ISBN-13, since the schema allows digits only). Exporters
  take `field_names="zyte"` for its camelCase.
- **Dicts, like every Item.** An auto item is an `Item` subclass whose
  `ItemSpec` carries a schema. Rust runs the schema's reader, then the
  subclass's own fields, and emits the type's fields in order, then any
  extra fields, then `metadata` (`probability`, `date_downloaded`,
  `sources`, `validation_messages`). A declarative crawl does all of it
  without Python.
- **No invented values.** A field the page doesn't hold is None. A
  currency is named only when structured data gives a code, the text has
  one, or the symbol belongs to one currency; `$` alone leaves `currency`
  None and `currency_raw` `"$"`. Never the domain or the locale.

## How a page is read

1. `auto::schema_org::things` reads every typed schema.org object on the
   page into one shape: JSON-LD (with `@graph` flattened and `{"@id"}`
   references replaced by what they name) and microdata (reshaped to
   match, keeping each property's element). Type URLs shorten to `Product`
   or `Offer`; subtypes such as ProductGroup and NewsArticle count as
   their parent. Cycles stay references and nesting stops at 64 levels,
   so a hostile page can't exhaust the stack. Microdata's `content`
   attribute is read on any element, as search engines read it.
2. Each type's reader picks the thing the page is about (for products:
   the one whose URL is the page's, else the first with an offer, else the
   first; products that hang off another as variants or accessories don't
   count) from JSON-LD and from microdata, and offers each field to a
   builder in that order: JSON-LD, then microdata, then meta tags (Open
   Graph's `product:` tags, `og:title`, `og:image`, the canonical link).
   The first source to give a value wins and is recorded. On key fields
   (name, price, currency, availability, SKU, job title), a different
   answer from the other structured source is a validation message.
3. Field readers in `auto::fields` turn what was found into the schema's
   shapes: prices through `price::parse_price`, dates through
   `date::parse_date`, and new readers for availability, GTINs, brands,
   ratings, breadcrumbs, salaries, employment types and images. Each also
   reads an element a person picked (`from_element`) or a string
   (`from_text`), which is how a pinned field is read as its field.
4. Articles add what `content::article` reads from the page itself, so a
   page with no structured data still gives its headline and body.
5. `auto::js_state` reads the app's state between microdata and meta
   tags: JSON scripts with an id like `__NEXT_DATA__` (Nuxt 3's
   `__NUXT_DATA__` decoded from devalue's flat array), Nuxt 2's
   `__NUXT__` (the object its function returns, parameters filled from
   the call: `js::nuxt`), Apollo's cache with `__ref`s followed, and the
   usual `window.__X__` names. Every object is weighed by its keys
   (matched without case, `_` or `-`): a product needs a name and a
   price, a job a title, two facts and one only adverts have (a salary,
   a job type, an apply link); codes, images, brand, currency and a
   `__typename` saying Product add weight. The page's product is the
   heaviest with more than a name and a price that's in no list of two
   or more and under no key saying basket, delivery, recommendations or
   the like, or else one in a list whose URL is the page's (whole path
   segments). The longest list of three or more priced products is a
   listing, unless the page has a product of its own. `salePrice` and
   its kind come before `price`, which is then the regular price. Money
   in `centAmount`, `priceInCents` and the like is divided by 100 (by
   `fractionDigits` when given, up to six). Paths in sources are built
   only for what's kept, with keys holding dots or brackets quoted. The walk stops after
   100,000 values, looks at the first 1,000 entries of a list, and
   reference following shares one budget, so shared references can't
   multiply.
6. `auto::dom` reads the layout last, for any field still empty. It finds
   every short text that reads as a price (with a currency, or inside an
   element marked as a price, and not a sentence with a price in it),
   then for each the nearest element above it that holds a link, and that
   element's look-alikes (`Node::similar`): a group of three or more, at
   least half of them priced, is a listing's cards. A price is old when
   it's struck, classed as old (`was`, `compare`) or says so ("Was",
   "RRP"); one only its class calls `list` or `regular` is old only when
   there's another price to be the current one, since some themes class
   the price that way when nothing is on sale. A product is the page
   heading with the nearest current price within 300 nodes, outside
   cards, boilerplate and links, that is marked as a price or has a buy
   button beside it; a page with three or more buy buttons is a listing.
   Its images, stock text, buy button and labelled codes are read from
   the smallest element holding heading and price, widened (never past
   `<main>` or `<article>`) to take in the gallery. A listing is the
   largest card group outside boilerplate; its pager is read only when
   there is one. A job needs the heading plus an apply button ("Apply
   now", not "Apply filters"), a salary line that starts with the money,
   or two labelled facts. Each reader walks the page a fixed number of
   times; texts read from an element above a price stop after 40 bytes,
   at most 2,000 prices and 16 card searches are made, and the stress
   pages in `tests/auto_layout.rs` and `tests/auto.rs` hold them to time
   limits.

Meta-tag fallbacks for products apply only when the page has a Product
or says `og:type` product: a news page asked for a product gives its URL
and a probability near zero, not its title as a product name.

## Page type

`auto::page_type` builds the page's context once (structured data, meta
tags, app state) and runs the Product, JobPosting, ProductList and
Article readers on it, keeping each probability. The highest wins if it
reaches 0.4, the earlier type winning a tie. An article found only from
its main content counts as 0.45 rather than 0.6, since any page with a
few paragraphs reads that way, product pages with long descriptions
included. A product reader that finds several products, none of them
the page's own URL, or a product on a CollectionPage, gives 0.5: that's
a listing. The corpus's answers each carry the type `page_type` must
give.

## Pinning

A field the subclass declares under a schema field's name replaces it.
It compiles to `Convert::Auto(kind)`, with the kind from
`Schema::kind_of`, so a price element gives `"9.50"`. `into=` replaces
that reading. The field's source becomes `css:<query>` or
`xpath:<query>`. A pinned field that finds nothing gives None and a
validation message, and one that finds text that doesn't read as its
field ("free" for a price) gives None and says what it found. Neither
falls back to the automatic value: a quiet fallback would hide a broken
selector.

## Probability

Fixed steps for now, set where the evidence changes: about 0.95 for a
complete schema.org item (a product with a name and an offer, SKU or
GTIN), 0.8 for a thinner one, 0.5 for a product only Open Graph names,
0.6 for an article found only by its main content, 0.9 for a listing of
two or more products, and 0.05 when nothing points to the type. From the
layout alone: 0.7 for a product with a buy control, 0.55 without, 0.75
for a listing of cards (0.3 when the page also shows a product of its
own, whose cards are related products), and 0.5 for a job. They're
to be calibrated against a corpus of real pages once the layout readers
exist.

## Testing

`crates/netweir-dom/tests/fixtures/auto/` holds pages written for the
purpose, each with a JSON file of its answer: the type, the URL, every
field that must have a value (the rest must be null), some sources, the
probability's range and which fields carry validation messages. Real
sites' pages aren't committed. `tests/auto.rs` also feeds every reader
JSON-LD nested 5,000 deep and a 5 MB description and requires them to
finish in bounded time. `tests/test_auto.py` covers pinning, tracking
through a redesign, declarative crawls, exports and the command.

Two scripts measure it on real pages, which are never committed.
`bench/auto_eval.py` takes pages with complete structured data, reads
them, strips the JSON-LD, microdata, Open Graph tags and app state, and
reads them again: the first reading is the answer key for the layout
readers. `bench/zyte_compare.py` has Zyte API fetch and extract each
page once, reads the same HTML with Netweir, and reports agreement per
field; it keeps what it fetched so a comparison can be run again for
free.

## Still to come

Some fields of the schema have no reader yet: a job's `headline`, a
product's `features` and `style`, and the subcategories of navigation.
They can be pinned meanwhile.

- Generic records: the main repeated region of any page (table rows,
  cards, list entries) as rows with inferred column names, for public
  records, catalogues and directories.
- The rest of the schema (ForumThread, ArticleList,
  BusinessPlace, RealEstate, Serp and others), and spider templates run
  with a URL.
