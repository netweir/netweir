# Item loaders

A callback that fills an item by hand ends up as a wall of
`.get().strip()` calls, each with its own idea of what happens when
nothing matches. An item loader puts the cleaning in one place: you say
where each field comes from, and the loader's processors decide how the
values are tidied on the way in and settled on the way out. It's Scrapy's
`ItemLoader`, so loaders you've already written carry over.

```python
from dataclasses import dataclass, field

import netweir
from netweir.loader import ItemLoader, MapCompose, TakeFirst


@dataclass
class Product:
    name: str | None = None
    price: netweir.Price | None = None
    url: str | None = None
    tags: list[str] = field(default_factory=list, metadata={"output_processor": list})


class ProductLoader(ItemLoader):
    default_item_class = Product
    default_output_processor = TakeFirst()
    name_in = MapCompose(str.strip)
    price_in = MapCompose(netweir.parse_price)


class Shop(netweir.Spider):
    start_urls = ["https://shop.example/"]

    def parse(self, page):
        loader = ProductLoader(response=page)
        loader.add_css("name", "h1::text")
        loader.add_xpath("price", "//p[@class='price']/text()")
        loader.add_value("url", page.url)
        loader.add_css("tags", "ul.tags li::text")
        yield loader.load_item()
```

`netweir.loader` has no dependencies, and runs on Netweir's own
selectors. It's also quicker than Scrapy's: loading a listing of a
thousand products, seven fields each, takes about three quarters of the
time itemloaders and parsel take (`bench/loaders.py`).

## Adding values

Every field collects a list of values, from any of these:

| | Collects |
|---|---|
| `add_css(field, query)` | every string the CSS query finds |
| `add_xpath(field, query, **variables)` | every string the XPath finds; keyword arguments bind `$variables` |
| `add_jmes(field, expression)` | what a JMESPath expression finds in the JSON (see [JSON](#json)) |
| `add_value(field, value)` | the value itself: a list adds each item, None adds nothing |

Each takes a list of queries too, collecting what every one finds. After
the query come any processors to run on these values only, and `re=`,
which keeps what a regular expression matches before they run (the whole
match, or its groups, or a group named `extract`):

```python
loader.add_xpath("price", "//p[@class='price']/text()", MapCompose(str.strip), re=r"[\d,.]+")
loader.add_value("sku", page.css(".sku::text").get(), re=r"SKU: (\d+)")
loader.add_value(None, {"brand": "Acme", "source": page.url})  # several fields at once
```

`replace_css()`, `replace_xpath()`, `replace_jmes()` and
`replace_value()` throw away what the field had first. `get_css()`,
`get_xpath()`, `get_jmes()` and `get_value()` give you the processed
values without collecting them.

`load_item()` runs each field's output processor over what it collected
and sets the result on the item, which it returns. A field whose output
is None is left as it was. `get_output_value(field)` gives one field's
output without touching the item, and `get_collected_values(field)` the
list before it. Every `add_` and `replace_` method returns the loader, so
calls chain.

## Processors

Values go through the field's input processor as they're added, and
through its output processor when the item is loaded. Both default to
`Identity()`, so out of the box every field is a list of strings.

| | Does |
|---|---|
| `Identity()` | nothing |
| `TakeFirst()` | the first value that isn't None or `""` |
| `Join(sep=" ")` | the values joined into one string |
| `MapCompose(f, g, ...)` | each value through `f`, each result through `g`, and so on; a function returning None drops the value, and one returning a list adds each item |
| `Compose(f, g, ...)` | the whole list through `f`, the result through `g`; stops at None unless you pass `stop_on_none=False` |
| `SelectJmes(expression)` | a JMESPath search of one value (needs `netweir[jmespath]`) |

Any function of one argument works as a processor too:
`MapCompose(str.strip, netweir.parse_date)`, `Compose(len)`.

A field's processors come from the first of these that's set:

1. `<field>_in` and `<field>_out` on the loader class (`price_in`).
2. `input_processor` and `output_processor` in the item field's
   metadata: `field(metadata={...})` on a dataclass, `attr.ib(metadata=...)`
   on attrs, `scrapy.Field(input_processor=...)` on a Scrapy Item.
3. `default_input_processor` and `default_output_processor` on the
   loader.

Subclass a loader to change some of them and inherit the rest.

## Context

Everything you pass a loader beyond its item and selector goes into
`loader.context`, along with `item`, `selector` and `response`. A
processor function with a `loader_context` parameter gets it, which is
how one function serves several sites:

```python
def absolute(url, loader_context):
    return loader_context["response"].urljoin(url)


def in_currency(text, loader_context):
    return f"{text} {loader_context['currency']}"


loader = ProductLoader(response=page, currency="GBP")
loader.add_css("image", "img.main::attr(src)", MapCompose(absolute))
loader.add_css("price_text", ".price::text", MapCompose(in_currency))
```

`MapCompose` and `Compose` take keyword arguments as defaults for the
context: `MapCompose(in_currency, currency="EUR")` uses EUR unless the
loader says otherwise.

## Parts of a page

`nested_css(query)` and `nested_xpath(query)` give a loader for part of
the page that fills the same item, so long queries don't repeat their
start:

```python
loader = ProductLoader(response=page)
details = loader.nested_css("div.details")
details.add_css("name", "h1::text")
details.add_xpath("sku", ".//dd[@class='sku']/text()")
footer = loader.nested_xpath("//footer")
footer.add_css("seller", ".seller::text")
item = loader.load_item()
```

A nested loader has its parent's context, plus any keyword arguments you
give `nested_css()`.

A loader can also start from one element, which suits a listing with
several items on it:

```python
for card in page.css("article.product"):
    loader = ProductLoader(selector=card, response=page)
    loader.add_css("name", "h3 a::attr(title)")
    yield loader.load_item()
```

`selector=` takes a Page, a Node or a Selection; `response=` takes the
Page, which is then the selector unless you give one.

## JSON

`add_jmes()` searches JSON: a JSON API's body when the loader has the
Page, a node's text when it has a Node, each result's text when it has a
Selection. Lists in what it finds become separate values; objects stay
whole.

```python
api = ItemLoader(response=page)  # https://shop.example/api/lamp
api.add_jmes("name", "product.title")
api.add_jmes("sizes", "product.variants[].size")

# JSON inside an HTML page
data = ProductLoader(response=page).nested_css("script#product-data::text")
data.add_jmes("sku", "product.sku")
```

It needs `pip install 'netweir[jmespath]'`. On a page whose body isn't
JSON, `add_jmes()` raises ValueError; reach the script with
`nested_css()` as above.

## Items

The item can be a dict (the default), a dataclass, an attrs class, or a
Scrapy Item if you use Scrapy. Pass one with `item=`, or set
`default_item_class` and let the loader make it. A dataclass or Scrapy
Item refuses a field it doesn't declare, with a KeyError naming it.

An item you pass in keeps what it holds: its values count as collected,
so a loader can finish an item started on an earlier page.

```python
def parse(self, page):
    loader = ProductLoader(response=page)
    loader.add_css("name", "h1::text")
    yield page.follow("reviews", callback="reviews", meta={"item": loader.load_item()})

def reviews(self, page):
    loader = ProductLoader(item=page.meta["item"], response=page)
    loader.add_css("rating", ".average::text")
    yield loader.load_item()
```

Pass the item, not the loader: a loader holds the parsed page, so it
can't be sent to a [worker
process](crawling.md#callbacks-in-worker-processes) or saved in a
checkpoint. With a checkpoint, `meta` has to be JSON, so the item has to
be a dict there (`dataclasses.asdict(item)` turns a dataclass into one).
Loaders work in worker processes as they do in one; what comes back is
the item.

A [declarative item](declarative.md) can start a loader too.
`Book.extract(page)` does the plain fields in Rust and gives a dict, and
the loader adds what needs Python:

```python
loader = ItemLoader(item=Book.extract(page), response=page)
loader.add_css("isbn", ".details::text", re=r"ISBN (\d+)")
```

## Coming from Scrapy

Change the imports and the loaders work:

```python
# from scrapy.loader import ItemLoader
# from itemloaders.processors import MapCompose, TakeFirst
from netweir.loader import ItemLoader, MapCompose, TakeFirst
```

Every method, processor and rule above is Scrapy's, down to which empty
values count and how errors are worded. `stats=` takes anything with
Scrapy's `inc_value()`, and counts how often each query finds something.
The default item is a dict rather than a Scrapy `Item`. A Scrapy response
or a parsel selector works as the selector too, if you're running both.

Six things differ, all on purpose:

- `add_jmes()` keeps a JSON object as one value. Scrapy's gives you the
  object's keys.
- A field of the item that's None stays None when nothing is added to it.
  Scrapy's turns it into `[]`, so `price: float | None = None` loads as
  `price=[]`.
- A nested loader doesn't count the item's existing values a second
  time. With Scrapy's, `ItemLoader(item={"a": 1})` and one
  `nested_css()` load `{"a": [1, 1]}`.
- `add_jmes()` on text that isn't JSON raises ValueError: the body of an
  HTML page, or a node a nested loader picked (`nested_css("h1")`).
  Scrapy's finds nothing and carries on. Point the loader at the JSON
  with `nested_css("script#data::text")`.
- A loader you've dropped is let go, page and all. Scrapy's remembers
  every method used as a processor, and the loader each is bound to, so
  up to 1,024 loaders and their responses stay in memory.
- Pydantic models aren't supported as items.
