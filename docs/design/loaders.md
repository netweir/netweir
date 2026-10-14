# Item loaders

Most Scrapy projects of any size fill their items through loaders:
`ItemLoader(item=Product(), response=response)`, a run of `add_css()`
and `add_xpath()` calls, `MapCompose(str.strip)` on the way in,
`TakeFirst()` on the way out, `load_item()`. A project moving to Netweir
brings dozens of those callbacks with it, and rewriting each one as
straight selector calls is the most tedious part of the move. This note
is how `netweir.loader` gives them somewhere to land. The user-facing
description is in [loaders.md](../loaders.md).

## Our own, or the itemloaders package?

Scrapy's loaders live in the `itemloaders` package, BSD licensed and
small. Depending on it was the obvious first idea. Three things argued
against it.

**What it pulls in.** itemloaders requires parsel, itemadapter and
jmespath; parsel requires lxml, cssselect and w3lib. Netweir has no
runtime dependencies at all: its selectors, its parser and its JMESPath
glue are its own, and jmespath is an optional extra. Taking itemloaders
would install lxml next to a Rust DOM that exists so you don't need
lxml.

**Its selector contract is parsel's.** itemloaders calls
`selector.css(q).getall()` and `selector.xpath(q, **kw).getall()`, which
Netweir's Page, Node and Selection answer. It also calls
`selector.jmespath(q).getall()`, which they don't: Netweir's `jmespath()`
gives the plain value, as the selecting docs promise. And its `re=` and
flattening come from `parsel.utils`.

**Its speed.** Every `add_*()` call in itemloaders builds an
`ItemAdapter` to look up the field's metadata, and every processor
passes through a check that reads the function's signature (cached by
function, so a `MapCompose(str.strip)` made inside the call, as most
code makes it, misses every time). `bench/loaders.py` loads every product
of the generated shop listing, seven fields each, with the same
callback four ways, on an Apple M2 Max with free-threaded Python 3.14:

| | 100 KB, 100 items | 1 MB, 998 items |
|---|---|---|
| no loader, Netweir's selectors straight | 8.8 ms | 88.0 ms |
| `netweir.loader` | 10.5 ms | 104.8 ms |
| itemloaders + parsel | 14.6 ms | 142.2 ms |
| itemloaders on Netweir's selectors | 15.6 ms | 155.2 ms |

On the same selectors, itemloaders adds about 67 µs to an item and
`netweir.loader` about 17 µs.

So it's written here, from itemloaders' documented behaviour and its
test-suite rather than its code. The proof it behaves the same: with a
two-line shim that maps `itemloaders` and `itemloaders.processors` to
`netweir.loader`, itemloaders 1.5.0's own tests (all but the ones for its
private helpers) pass against it, parsel selectors and all, bar one that
checks the wording of an error naming parsel. `tests/test_loaders.py`
also runs one callback through both on the same page and compares the
items, wherever Scrapy has brought itemloaders along.

## What it keeps

All of the API: `add_`, `replace_` and `get_` for values, CSS, XPath and
JMESPath; `nested_css()` and `nested_xpath()`; `load_item()`,
`get_output_value()` and `get_collected_values()`; `default_item_class`,
`default_input_processor` and `default_output_processor`; `<field>_in`
and `<field>_out`; `input_processor` and `output_processor` in a
field's metadata; `context`; `stats=` and its `parser/...` counts.
`response=` comes from Scrapy's subclass, and takes a Netweir Page (which
is its own selector) or a Scrapy response (whose `.selector` is used).

The processors are Identity, TakeFirst, Join, Compose, MapCompose and
SelectJmes, with their rules for None and empty values exactly: TakeFirst
skips None and `""` but not `0`; Compose stops at None unless
`stop_on_none=False`; MapCompose drops None, spreads a returned list and
treats a lone value as a list of one; an input processor's answer is
collected only if it's truthy; an output answer of None leaves the field
alone. A function with a `loader_context` parameter gets the context, a
`partial` that already fixed it doesn't, and a processor's keyword
arguments are defaults the loader's context overrides, seen through a
`ChainMap` so writes reach the loader. A plain function set as `name_in`
isn't bound as a method; a method defined in the loader is. Errors are
wrapped in ValueError with itemloaders' wording, so code that matches
them still does.

## Where it differs, on purpose

- **JSON objects stay whole.** itemloaders flattens what `add_jmes`
  finds with parsel's `flatten`, which treats a dict as a list of its
  keys: `add_jmes("dims", "product.dims")` collects `["w", "h"]`. Here
  lists are taken apart (nested ones too, as there) and objects are kept,
  so it collects `[{"w": 20, "h": 30}]`.
- **None isn't a value.** itemloaders reads a given item's fields into
  the collected values, and a field that's None becomes an empty list, so
  a dataclass with `price: float | None = None` (the shape itemloaders'
  own docs recommend) loads as `price=[]` whenever nothing was added to
  it, which then fails `netweir.validate`. Here a None field collects
  nothing and stays None.
- **Nesting doesn't read the item again.** In itemloaders each nested
  loader reads the item's values into the shared list once more, so
  `ItemLoader(item={"a": 1})` with one `nested_css()` loads `{"a": [1,
  1]}`. Here the values are read once, by the outermost loader.
- **`add_jmes` reads JSON as Netweir does.** On a Page, the body; on a
  Node, its text; on a Selection, each result. So an HTML page, or a
  nested loader over elements or text that aren't JSON, raises
  ValueError, where parsel quietly finds nothing; the JSON inside an HTML
  page is `nested_css("script#data::text")` away.
- **A dropped loader is let go.** itemloaders caches whether each
  processor takes the context with `lru_cache` keyed on the processor, and
  a method used as one (`def name_in(self, values)`, `MapCompose(self.clean)`)
  is a new bound method, holding its loader, every time: up to 1,024
  loaders and their pages stay alive. Here the answer is kept by what
  decides it, a method's function or a callable object's class, and a
  partial isn't kept at all.
- **Items:** dicts (and subclasses), dataclasses, attrs classes and Scrapy
  Items, which itemadapter covers too; not pydantic models, which nobody
  has asked for. Anything else is a TypeError when the loader is made,
  rather than at the first `add_*()`. A Scrapy Item is recognised only if
  Scrapy is already imported, so Netweir never imports it.

## Netweir's own items

A `netweir.Item` subclass is a description of how to extract an item,
compiled to Rust; what it extracts is a dict. So it isn't something a
loader can fill, and passing one says so and points at the shape that
works: `ItemLoader(item=Book.extract(page), response=page)`, where the
Rust extraction does the plain fields and the loader adds what needs
Python. Loaded dataclasses go through `validate`, coverage counting and
the exporters as any other dataclass does.

## Workers

A loader is used and finished inside one callback, so it never crosses
between processes; only what `load_item()` returns does, and dicts and
module-level dataclasses pickle. A loader doesn't pickle (it holds a
parsed page), so the Scrapy habit of passing a loader to the next
callback in `meta` doesn't carry over to workers or checkpoints. Pass the
item instead and make a new loader on the next page with `item=`; its
values count as collected there. A checkpoint keeps `meta` as JSON, so
there the item has to be a dict.

## As built

One module, `python/netweir/loader.py`, public as `netweir.loader` like
`netweir.export`. Two things keep it fast:

- The processors here are known to take the context (Compose,
  MapCompose) or not (the rest), by exact class, so they never have their
  signature read. For other callables the answer is cached by a key that
  holds no loader (see above); partials and unhashable callables are read
  each time.
- A dataclass's or attrs class's fields and their metadata are read once
  per class. A dict has no metadata and costs nothing to ask. That cache holds
  classes only, never items or loaders.

`re=` uses `netweir._regex.extract`, the same function behind
`Selection.re()`, so the two agree on groups, the `extract` group and
entities. No Rust: a loader's work is calling Python functions, which
Rust can't make cheaper, and the queries it makes are already Rust's.
