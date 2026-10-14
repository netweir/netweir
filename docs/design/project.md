# Project commands

Status: draft, October 2026
Author: James Conway

## What it is for

A Scrapy user's day is a handful of commands: `startproject` once,
`genspider` for each new site, `crawl NAME`, `list` to remember what's
there, `parse URL` to see what one callback makes of one page, `check`
before a deploy, `version -v` for a bug report. Netweir has `crawl`,
`fetch`, `view`, `shell` and `serve`, and a Scrapy user who reaches for the
rest finds nothing. This milestone adds them, so the habits carry over.

None of them adds a concept. They work on what Netweir already has: spider
files, `netweir.Settings`, `netweir.Follow` rules, sitemaps and the crawl
engine.

## What a project is

`netweir serve --project NAME=DIR` already decided it: a project is a
directory of spider files, every `.py` directly in it read, subdirectories
left for helpers, a spider named by its `name` or its class name. There's
no config file and no package to install, because a spider is loaded from
its path.

The project commands keep that. A project is a directory with a `spiders/`
directory in it:

```
books/
  README.md
  spiders/
    settings.py      # SETTINGS = netweir.Settings(...), shared
    catalogue.py     # one spider per file, from genspider
```

`spiders/` is exactly what `serve --project books=spiders/` takes, so a
project made here can be served as it is. Commands find it by looking in
the current directory and then each one above it for a `spiders/`
directory, the way Scrapy looks for `scrapy.cfg`, so they work from
anywhere inside the project.

`spiders/` isn't a package (no `__init__.py`): nothing imports it by its
name. A spider file imports `settings.py` beside it as `from settings
import SETTINGS`, and any other file beside it the same way. So loading a
spider file puts its folder first on `sys.path` for the rest of the
process, exactly as running a script does: `import settings`, helpers and
imports made late, inside a callback at crawl time, all find the files
beside it. Worker processes load the file the same way.

That makes the rule one project per process. Python caches a module by its
bare name, so the `settings`, `helpers` or `consts` of a second folder
would be the first one's, silently. Renaming them module by module can't
be made right (a settings.py importing consts.py, a helper imported in a
callback), so a spider file from a second, different folder raises an
error naming both folders, and loading happens under a lock. Commands that
take files from several folders, which only `check` does, run a process
per folder (`python -m netweir check` with the same options) and combine
what they report into one summary, the exit status the worst of them.
`serve` never loads spiders itself: it lists a project's in a child
process of its own and runs each job as `netweir crawl` in another.

Shared settings are a `Settings` object, not a module of constants: a spider
uses it as it is (`settings = SETTINGS`) or changes a field for itself
(`dataclasses.replace(SETTINGS, max_depth=3)`). That's how settings work in
Netweir everywhere else; a second, Scrapy-style system of upper-case names
would be a parallel convention.

## The commands

### startproject NAME [DIR]

Writes `README.md` and `spiders/settings.py` into `DIR` (default `./NAME`)
and says what to type next. NAME is a Python identifier in ASCII, as in
Scrapy, which also fits `serve`'s rule for a project's name. It never
overwrites: if either file is there already, a symlink to nowhere
included, it stops, naming it, and writes nothing. Files are opened with
`"x"`, so one that appears in the meantime is refused, not written through.

### genspider NAME DOMAIN [-t TEMPLATE] [--force] | -l

Writes `spiders/NAME.py` inside a project, `./NAME.py` outside one, as
Scrapy does. DOMAIN may be a domain or a URL, as in Scrapy 2.11 and later:
the host goes in `allowed_domains`, the URL (or `https://DOMAIN/`) in
`start_urls`. A name that isn't a valid file name, or that is an
importable module's (the standard library's, an installed package's,
`netweir`, `settings`), is refused, as Scrapy does: `spiders/asyncio.py`
would hide asyncio from anything run in that folder, serve's jobs
included. An existing file, or (in a project) a spider of that name
already, are refused unless `--force`.

Templates are only for what Netweir has:

| template | Netweir feature | Scrapy's nearest |
|---|---|---|
| `basic` (default) | a `parse` callback | `basic` |
| `crawl` | `rules` of `netweir.Follow` and a `netweir.Item` extracted in Rust | `crawl` |
| `sitemap` | `sitemap_urls` and `sitemap_rules` | none (SitemapSpider has no template) |

Scrapy's `xmlfeed` and `csvfeed` have nothing to map onto: Netweir has no
feed spiders. `-l` lists the templates with a line on each. Inside a
project the spider uses the project's `SETTINGS`. Each template has a
contract, so `netweir check` has something to run from the first day; the
crawl template, whose items come from Rust, has a `check_entry` method for
it that the crawl never calls.
Generated files pass ruff with the repository's configuration.

### list

Prints every spider's name in the project, sorted, one per line. The files
are read in this process, not a child as `serve` does: a command that runs
once and exits has nothing to protect. A file that raises on import stops
the command, naming the file, as Scrapy's does. A name defined twice is
listed once with a warning naming both files.

### crawl NAME and runspider FILE

`netweir crawl` takes a file. Inside a project it now also takes a spider's
name, found as `list` finds it; a name and a file are told apart by whether
a file of that name exists. `runspider` is an alias of `crawl`, for the
fingers of people who type `scrapy runspider spider.py`: it costs one
line, and nothing about it differs.

### check [SPIDER ...] [-l] [-v]

Scrapy's spider contracts. A callback's docstring holds lines starting with
`@`:

| contract | Netweir |
|---|---|
| `@url URL` | the page to fetch |
| `@returns items\|requests [MIN [MAX]]` | count of items or Requests the callback gave |
| `@scrapes field ...` | every item has these keys (or dataclass fields) |
| `@cb_kwargs JSON` | keyword arguments for the callback |
| `@meta JSON` | the request's `meta` |
| `@method`, `@body`, `@header NAME VALUE` | the request's method, body and headers |

`@cookie` isn't there: `Request` has no cookies of its own, and a `Cookie`
header would fight the session's jar. `@cb_kwargs` works without a
`cb_kwargs` on `Request`: the check calls the callback with them.

Custom contracts keep Scrapy's API: a subclass of
`netweir.contracts.Contract` with a `name` and any of
`adjust_request_args(args)`, `pre_process(page)` and
`post_process(output)`, failing with `ContractFail`. Scrapy registers them
by dotted path in `SPIDER_CONTRACTS`; Netweir lists objects rather than
paths everywhere, so a spider lists the classes, `contracts = [HasHeader]`.
`args` are `netweir.Request`'s keyword arguments (plus `cb_kwargs`), so a
contract can set `method`, `json`, `headers` and the rest.
`request_cls` isn't there: there's one Request class.

Each spider is run once with `start()` replaced by one request per
contracted callback (`dont_filter=True`, as in Scrapy), through the real
engine, so robots.txt, the profile, middlewares and the response cache
apply: with `cache` set, a check runs offline from the cache. What the
callbacks yield is collected and dropped, never followed or piped. The run
leaves nothing behind: `checkpoint`, `deltafetch`, `queue`, `warc` and
`har` are turned off, pipelines and end-of-crawl checks aren't run, and
callbacks run in this process (`workers=1`).

Results are reported with `unittest`'s own text result, as Scrapy does:
`FAIL: [spider] method (@returns post-hook)` and the message, `Ran N
contracts in Xs`, `OK` or `FAILED (failures=F, errors=E)`, on stderr. Exit
status 1 when anything failed, 2 for a mistake on the command line, 0
otherwise.

`@returns items` counts what the crawl takes as an item: anything but
None, a Request, or a string, number or set. A page with an error status
still reaches its callback, as it does in a crawl (Netweir has no
`HTTPERROR_ALLOWED_CODES` filter), so on a 404 `@returns items 0` passes
where Scrapy would report an errback error. A spider is made as `crawl`
makes it, with no arguments (one whose `__init__` needs them is refused,
saying to give them defaults, as `-a` only sets attributes afterwards),
and its settings, those `__init__` set included, get `-s` over them.

One difference on purpose: a callback with contracts but no `@url` (after
every contract has adjusted the request) is an error, and so is a contracted
request whose page never reached its callback or errback (robots.txt, a
limit). Scrapy skips both in silence, and a contract that never runs looks
just like one that passes.

`-l` lists each spider's contracted callbacks without fetching anything.

### parse URL [options]

Fetches URL through the crawl engine and runs it through a callback,
printing the items and Requests it gave, then follows those Requests up to
`--depth` levels, as Scrapy's `parse` does:

- `--spider NAME` or `--spider FILE`; without it, the project's spider
  whose `allowed_domains` (or name) covers the URL's host, if exactly one
  does.
- `-c CALLBACK`: the method to use. Without it the page is treated as a
  start page: `parse`, or for a spider with rules, the rules (and `parse`
  as well, if the spider writes its own).
- `-r/--rules`: the first rule whose `allow`, `deny` and domains admit the
  URL decides how it's handled, as a page reached by that rule: its
  `extract=` item and its callback, or `parse` for a rule with neither, as
  in Scrapy.
- `-d/--depth N` (default 1), `-m/--meta JSON`, `--cbkwargs JSON`,
  `--pipelines` (items go through the spider's pipelines),
  `--noitems`, `--nolinks`, `-v` (every level, not just the last),
  `-s` and `-a` as in `crawl`.

On a rules page the Requests shown include the links each rule finds,
worked out in Python with the rule's own query, `allow`, `deny` and
domains, less those `allowed_domains` drops; the crawl does the same in
Rust.

At depth 2 and beyond, a Request without a callback goes where the crawl
would send it: `parse`, or the rules. Scrapy sends it to the `-c`
callback. Netweir's way shows what the crawl will do, which is the point. That's a second implementation of
link-finding, kept to a few lines, for the sake of a debugging command: a
page whose links `parse` shows and a crawl doesn't follow would be a bug in
one of them.

Output follows Scrapy's: `>>> STATUS DEPTH LEVEL N <<<`, then `# Scraped
Items` and `# Requests` with the items pretty-printed and each Request as
`<GET url>`. Without `-v`, the items of every level and the Requests of the
last are printed; with it, each level's. Exit status 1 if the page couldn't
be fetched or a callback raised.

Like `check`, it turns off what would leave a trace (`checkpoint`,
`deltafetch`, `queue`, `warc`, `har`) and runs callbacks in this process.

### version [-v]

`Netweir 0.3.1`. With `-v`, one line each for Netweir, Python, the platform,
the Rust compiler that built the extension, lexbor's version, and the
browser profiles built in with the short name each answers to. All of it is
known at build time, so it costs nothing to print; the Chrome a crawl might
drive isn't launched to ask its version.

## What's left out

- Scrapy's `bench`, `settings` and `edit`: `bench/` in the repository is the
  benchmark, settings are the `Settings` docstring and `docs/settings.md`,
  and `$EDITOR spiders/NAME.py` is shorter than `edit`.
- `genspider -d` (print a template) and `-e` (open the new file).
- `parse -o`: `crawl -o` writes items.
