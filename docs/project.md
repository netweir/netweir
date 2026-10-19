# Projects: startproject, genspider, check and the rest

One spider fits in one file, and `netweir crawl books.py` is all it needs.
By the fifth spider you want somewhere to keep them, settings they share,
and a way to know which ones a site redesign broke. If you've used Scrapy,
you know the commands for that already, and they're here under the same
names:

```
netweir startproject shop          # a project: spiders/ and a README
cd shop
netweir genspider books books.toscrape.com
netweir list                       # the spiders here
netweir crawl books -o books.jsonl # by name, from anywhere in the project
netweir parse https://books.toscrape.com/ --spider books
netweir check                      # the contracts in your docstrings
netweir version -v                 # for a bug report
```

## A project

`netweir startproject NAME` makes a directory called NAME (or the one you
give after it) holding:

```
shop/
  README.md
  spiders/
    settings.py
```

That's the whole of it. Every `.py` file directly in `spiders/` is read for
spiders; a subdirectory isn't, so helpers can live in one. It's the same
layout `netweir serve` takes, so `netweir serve --project shop=spiders/`
runs the project's spiders as jobs with nothing to change. See
[Running spiders as a service](serve.md).

`spiders/settings.py` holds one `netweir.Settings` that every generated
spider starts from:

```python
import netweir

SETTINGS = netweir.Settings(cache=".cache", concurrency=16)
```

A spider takes it whole with `settings = SETTINGS`, or changes a field for
itself:

```python
import dataclasses

import netweir
from settings import SETTINGS


class Books(netweir.Spider):
    settings = dataclasses.replace(SETTINGS, max_depth=3)
```

`from settings import SETTINGS` works because loading a spider file puts
its folder first on Python's path, as running a script does. Any file
beside your spiders can be imported the same way, as `import helpers`,
when the file loads or later in a callback.

Python keeps one module per name, so a process holds one project: two
folders' `settings.py` would otherwise be the same module. Loading spiders
from a second folder in one process is an error that names both. When you
give `netweir check` files from several folders, it checks each folder in a
process of its own and reports them as one run.

The commands below find the project by looking for `spiders/` in the
current directory, then in each directory above it, so you can run them
from anywhere inside.

## Making a spider

```
netweir genspider books books.toscrape.com
netweir genspider -t crawl catalogue https://shop.example/catalogue/
```

writes `spiders/books.py` (or `./books.py` outside a project) with the name,
`allowed_domains` and `start_urls` filled in. Give a URL instead of a
domain to start somewhere other than the home page. Five templates, `-l`
lists them:

| template | what you get |
|---|---|
| `basic` | a `parse` callback, the default |
| `crawl` | `rules` of `netweir.Follow` and a `netweir.Item`, so items come out of Rust with no Python per page ([Declarative spiders](declarative.md)) |
| `sitemap` | `sitemap_urls` pointing at the site's robots.txt, and `sitemap_rules` ([Sitemaps](crawling.md#sitemaps)) |
| `xmlfeed` | an XMLFeedSpider reading an RSS feed node by node ([XML feeds](xml.md#xml-feeds-xmlfeedspider)) |
| `csvfeed` | a CSVFeedSpider reading a CSV file row by row ([CSV feeds](xml.md#csv-feeds-csvfeedspider)) |

Given a domain, the feed templates start from `/feed.xml` or `/feed.csv`
there; given a URL, from that URL. The first three come with a contract
for [`netweir check`](#contracts-check); a feed's nodes and rows don't
have a page of their own to check. genspider won't write over a file, or reuse a name the
project already has, unless you pass `--force`. It refuses a name that's
also a Python module's, such as `json` or `asyncio`, since a file of that
name would hide the module.

## Running one by name

Inside a project, `netweir crawl` takes a spider's name as well as a file:

```
netweir crawl books -o books.jsonl -s max_items=100
```

The name is the spider's `name`, or its class name if it has none, the same
name `netweir list` prints. `netweir runspider FILE` is `netweir crawl
FILE` under Scrapy's other name for it.

## Trying one page: parse

`netweir parse URL` fetches one page and runs it through a spider, then
prints the items and requests that came out:

```
$ netweir parse https://books.toscrape.com/ --spider books

>>> STATUS DEPTH LEVEL 1 <<<
# Scraped Items  ------------------------------------------------------------
[{'title': 'All products | Books to Scrape - Sandbox',
  'url': 'https://books.toscrape.com/'}]
# Requests  -----------------------------------------------------------------
[]
```

Without `--spider`, it uses the project's spider whose `allowed_domains`
cover the URL, if exactly one does. The page is fetched the way the crawl
fetches it: the spider's settings, middlewares, robots.txt and cache.

| option | what it does |
|---|---|
| `--spider NAME` | the spider, by name or by file |
| `-c CALLBACK` | the method to call; `parse` by default, or for a spider with rules, the rules |
| `-r`, `--rules` | handle the URL as the rule that would lead to it does: its `extract=` item and its callback, or `parse` if it has neither |
| `-d N`, `--depth N` | follow the requests that come out, N levels deep (default 1) |
| `-m JSON`, `--meta JSON` | the request's `meta` |
| `--cbkwargs JSON` | keyword arguments for the callback |
| `--pipelines` | send the items through the spider's pipelines |
| `--noitems`, `--nolinks` | leave items, or requests, out of the output |
| `-v` | print every level, not only the last |
| `-s KEY=VALUE`, `-a NAME=VALUE` | as for `crawl` |

Without `-v` you see every level's items and the last level's requests.
On a page the rules apply to, the requests include the links each rule
finds, so `parse` is a quick way to see whether a `Follow` matches what you
meant. The status is 1 if the page couldn't be fetched or a callback raised.

## Contracts: check

A contract is a line in a callback's docstring saying what it should do
with a given page:

```python
class Books(netweir.Spider):
    def parse(self, page):
        """A listing: 20 books and a next page.

        @url https://books.toscrape.com/
        @returns items 20 20
        @returns requests 1
        @scrapes title price
        """
```

`netweir check` fetches each `@url`, runs the callback on it and checks the
rest. It's how you find out, before a scheduled crawl does, that the site
moved its prices:

```
$ netweir check
.F
======================================================================
FAIL: [books] parse (@scrapes post-hook)
----------------------------------------------------------------------
netweir.contracts.ContractFail: Missing fields: price

----------------------------------------------------------------------
Ran 2 contracts in 0.412s

FAILED (failures=1)
```

| contract | what it says |
|---|---|
| `@url URL` | the page to check against; a callback with contracts needs one |
| `@returns items [MIN [MAX]]` | at least MIN items (default 1), at most MAX (default any) |
| `@returns requests [MIN [MAX]]` | the same for requests |
| `@scrapes FIELD ...` | every item has these fields |
| `@cb_kwargs JSON` | keyword arguments for the callback |
| `@meta JSON` | the request's `meta` |
| `@method METHOD`, `@body TEXT`, `@header NAME VALUE` | how the request is sent |

`netweir check books` checks one spider, and a file works as well as a
name. `-l` lists the callbacks that have contracts, without fetching
anything; `-v` prints a line for every contract. The exit status is 1 if
anything failed, so it fits in CI.

Each page goes through the crawl engine with the spider's settings, so
robots.txt, the profile and middlewares apply. If the spider has a
[cache](crawling.md#caching-while-you-develop), the check reads from it and
needs no network after the first run. What the callbacks give is checked
and then thrown away: nothing is followed, nothing reaches a pipeline, and
the check writes no checkpoint, WARC or HAR file. A page that never reached
its callback, because robots.txt or a block kept it out, is reported as an
error rather than skipped.

### Contracts of your own

A contract is a subclass of `netweir.contracts.Contract`, with a `name` and
any of three methods, which work as Scrapy's do:

```python
from netweir.contracts import Contract, ContractFail


class Fresh(Contract):
    """@fresh YEAR: every item is from YEAR or later."""

    name = "fresh"

    def adjust_request_args(self, args):
        # args are netweir.Request's arguments, plus cb_kwargs.
        args["headers"] = {"Cache-Control": "no-cache"}
        return args

    def pre_process(self, page):
        if page.status != 200:
            raise ContractFail(f"status {page.status}")

    def post_process(self, output):
        for item in output:
            if isinstance(item, dict) and item.get("year", 0) < int(self.args[0]):
                raise ContractFail(f"an item from {item['year']}")
```

List it on the spider, `contracts = [Fresh]`, and write `@fresh 2024` in a
docstring. The words after the name are `self.args`.

## version

`netweir version` prints the version. `-v` adds Python's, the platform,
the Rust compiler that built Netweir, the version of lexbor (the HTML
parser) and the browser profiles built in, which is what a bug report
needs.

## Coming from Scrapy

The commands take Scrapy's options where they mean something in Netweir.
What's different:

- A project is a `spiders/` directory, found by looking upwards, not a
  package with `scrapy.cfg`. Settings are a `netweir.Settings` in
  `spiders/settings.py`, not upper-case names.
- Custom contracts are listed on the spider (`contracts = [...]`), not in
  `SPIDER_CONTRACTS` by dotted path. There's no `@cookie` contract and no
  `request_cls`; set headers or `method` in `adjust_request_args`.
- `check` reports a contracted callback without `@url`, or whose page
  never arrived, as an error. Scrapy skips it.
- `parse` fetches the URL you give it even if `allowed_domains` leaves it
  out. There's no `-o`; use `crawl -o`.
- At depth 2 and beyond, `parse` sends a request without a callback where
  the crawl would, to `parse` or the rules, not to the `-c` callback.
- A page with an error status, a 404 say, still reaches the callback, as in
  a crawl, so `@returns items 0` passes on a 404 where Scrapy reports an
  error. Add `@returns items 1` if the page must have something on it.
- `genspider` has `basic`, `crawl`, `sitemap`, `xmlfeed` and `csvfeed`,
  and no `-e` or `-d`.
- There's no `bench`, `settings` or `edit` command.
