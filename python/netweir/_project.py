"""Scrapy's project commands for Netweir: startproject, genspider, list,
check, parse and version, and finding a spider by name for crawl.

A project is a directory with a ``spiders/`` directory in it, the spider
files directly inside that, as ``netweir serve --project`` reads them.
See docs/design/project.md.
"""

from __future__ import annotations

import dataclasses
import importlib.util
import inspect
import json
import logging
import os
import platform
import pprint
import re
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from netweir._cli import (
    _login_from_environment,
    _parse_argument,
    _parse_setting,
    _spiders_in,
    _Usage,
)


class _Failed(Exception):
    """Something other than the command line went wrong: exit status 1."""


# --- finding the project and its spiders ------------------------------------


def spiders_dir() -> Path:
    """``spiders/`` in the current directory or the nearest one above it."""
    here = Path.cwd().resolve()
    for folder in (here, *here.parents):
        if (folder / "spiders").is_dir():
            return folder / "spiders"
    raise _Usage(
        "no project here: no spiders/ directory in this directory or any above it"
        " (netweir startproject makes one)"
    )


def project_spiders() -> dict[str, list[tuple[type, Path]]]:
    """{name: [(class, file)]} for every spider the project's files define."""
    found: dict[str, list[tuple[type, Path]]] = {}
    for path in sorted(spiders_dir().glob("*.py")):
        try:
            classes = _spiders_in(str(path))
        except _Usage:
            raise
        except (Exception, SystemExit) as e:
            raise _Failed(f"{path}: {type(e).__name__}: {e}") from None
        for cls in classes:
            found.setdefault(cls.name or cls.__name__, []).append((cls, path))
    return found


def by_name(name: str) -> type:
    """The project's spider called ``name``."""
    try:
        found = project_spiders()
    except _Usage as e:
        raise _Usage(f"{name}: not a file, and {e}") from None
    if name not in found:
        names = ", ".join(sorted(found)) or "none"
        raise _Usage(f"no spider named {name!r} in {spiders_dir()}; the spiders are: {names}")
    if len(found[name]) > 1:
        files = ", ".join(str(path) for _, path in found[name])
        raise _Usage(f"two spiders are named {name!r}: in {files}")
    return found[name][0][0]


def spider_classes(target: str | None) -> list[type]:
    """The spiders a check or parse is about: all of a file's, one of the
    project's by name, or (no target) all of the project's."""
    if target is None:
        return [cls for _, entries in sorted(project_spiders().items()) for cls, _ in entries]
    if Path(target).is_file():
        return sorted(_spiders_in(target), key=lambda c: c.name or c.__name__)
    if target.endswith(".py"):
        raise _Usage(f"no such file: {target}")
    return [by_name(target)]


def _quiet_settings(base: Any, overrides: list[str]) -> Any:
    """The spider's settings with ``-s`` over them, and nothing that would
    leave a trace or move callbacks out of this process."""
    from netweir import Settings

    fields = {f.name: f for f in dataclasses.fields(Settings)}
    changes = dict(_parse_setting(text, fields) for text in overrides)
    _login_from_environment(base, changes, fields)
    return dataclasses.replace(
        base,
        **{
            **changes,
            "checkpoint": None,
            "deltafetch": None,
            "deltafetch_reset": False,
            "queue": None,
            "warc": None,
            "har": None,
            "workers": 1,
        },
    )


def _instance(cls: type, args) -> Any:
    arguments = dict(_parse_argument(a) for a in args.arguments)
    try:
        inspect.signature(cls).bind()
    except TypeError:
        raise _Usage(
            f"{cls.__name__}.__init__ needs arguments, and Netweir makes spiders with"
            " none, as crawl and serve do. Give them defaults; -a NAME=VALUE sets"
            " attributes after __init__, for opened() and the callbacks to read"
        ) from None
    spider = cls()
    # Over the spider's own settings, those __init__ set included.
    spider.settings = _quiet_settings(spider.settings, args.set)
    for name, value in arguments.items():
        setattr(spider, name, value)
    # End-of-crawl checks would judge a crawl that isn't one.
    spider.checks = []
    return spider


def _run(spider: Any, requests: list) -> None:
    """Crawls ``requests`` in place of the spider's own start."""
    from netweir._crawl import _run as run

    spider.start = lambda: requests
    run(spider.crawl())


# --- startproject and genspider ----------------------------------------------


def _create(path: Path, text: str, overwrite: bool = False) -> None:
    """Writes a new file; one there already, a symlink included, is
    refused rather than written through, even if it appeared just now."""
    try:
        with open(path, "w" if overwrite else "x", encoding="utf-8") as f:
            f.write(text)
    except FileExistsError:
        raise _Usage(f"{path} is there already; not written") from None


README = """\
# {name}

A Netweir project. The spiders are in `spiders/`, one per file, and they
share the settings in `spiders/settings.py`.

Make a spider for a site and run it:

```
netweir genspider books books.toscrape.com
netweir crawl books -o books.jsonl
```

`netweir list` shows the spiders here, `netweir parse URL` what a spider
makes of one page, and `netweir check` runs the contracts in your
callbacks' docstrings. To run the spiders as jobs over HTTP:

```
netweir serve --project {name}=spiders/
```
"""

SETTINGS = '''\
"""Settings every spider in this project starts from.

A spider takes them as they are with ``settings = SETTINGS``, or changes
one for itself with ``settings = dataclasses.replace(SETTINGS, max_depth=3)``.
"""

import netweir

SETTINGS = netweir.Settings(
    # While you write a spider, keep the pages it fetches, so thirty runs
    # cost the site one:
    # cache=".cache",
)
'''


def startproject(args) -> int:
    if not (args.name.isidentifier() and args.name.isascii()):
        raise _Usage(
            f"{args.name!r}: a project's name is letters, digits and '_', not starting with a digit"
        )
    root = Path(args.dir if args.dir is not None else args.name)
    files = {
        root / "README.md": README.format(name=args.name),
        root / "spiders" / "settings.py": SETTINGS,
    }
    # lexists: a symlink, even one to nowhere, is something there.
    there = [str(path) for path in files if os.path.lexists(path)]
    if there:
        raise _Usage(f"{', '.join(there)} already there; nothing written")
    for path, text in files.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        _create(path, text)
    print(f"Project {args.name} made in {root.resolve()}. Make its first spider with:")
    print(f"    cd {root}")
    print("    netweir genspider example example.com")
    return 0


_SPIDER_NAME = re.compile(r"[A-Za-z][A-Za-z0-9_-]*")
_HOST = re.compile(r"[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*(:\d+)?")

TEMPLATES = {
    "basic": '''\
{imports}

class {cls}(netweir.Spider):
    name = {name}
    allowed_domains = [{host}]
    start_urls = [{url}]
{settings}
    async def parse(self, page):
        """What to take from each page, and which links to follow.

        @url {raw_url}
        @returns items 1 1
        @scrapes title url
        """
        yield {{"title": page.css("title::text").get(), "url": page.url}}
        # Links to follow are yielded too:
        # for href in page.css("a.next::attr(href)").getall():
        #     yield page.follow(href)
''',
    "crawl": '''\
{imports}

class Entry(netweir.Item):
    """What to take from each page the rules lead to, extracted in Rust."""

    title = netweir.css("title::text", strip=True)


class {cls}(netweir.Spider):
    name = {name}
    allowed_domains = [{host}]
    start_urls = [{url}]
{settings}    rules = [
        # Pages to read for more links: listings, categories, next pages.
        # The patterns are examples: change them to fit the site's URLs.
        netweir.Follow("a", allow=[r"/category/"]),
        # Pages to take an Entry from.
        netweir.Follow("a", allow=[r"/item/"], extract=Entry),
    ]

    def check_entry(self, page):
        """Not part of the crawl, which extracts Entry in Rust: what
        netweir check runs, to see that Entry still fits an item page.

        @url {raw_url}
        @returns items 1 1
        @scrapes title
        """
        yield Entry.extract(page)
''',
    "sitemap": '''\
{imports}

class {cls}(netweir.Spider):
    name = {name}
    allowed_domains = [{host}]
    # The site's robots.txt names its sitemaps.
    sitemap_urls = [{robots}]
    # (a regex for the URL, the callback for the pages it matches)
    sitemap_rules = [("", "parse")]
{settings}
    async def parse(self, page):
        """What to take from each page the sitemaps list.

        @url {raw_url}
        @returns items 1 1
        @scrapes title url
        """
        yield {{"title": page.css("title::text").get(), "url": page.url}}
''',
    "xmlfeed": '''\
{imports}

class {cls}(netweir.XMLFeedSpider):
    name = {name}
    allowed_domains = [{host}]
    start_urls = [{xml_feed}]
    # "iternodes" reads one node at a time; "xml" parses the whole feed
    # first, and "html" reads markup too broken to be XML.
    iterator = "iternodes"
    # The elements to read: <item> in RSS. An Atom feed's are in its
    # namespace: namespaces = [("atom", "http://www.w3.org/2005/Atom")]
    # and itertag = "atom:entry".
    itertag = "item"
{settings}
    def parse_node(self, page, node):
        """What to take from each node of the feed: a selection of one, as
        Scrapy's Selector, with xpath(), css(), get() and attrib."""
        yield {{
            "title": node.xpath("title/text()").get(),
            "link": node.xpath("link/text()").get(),
        }}
''',
    "csvfeed": '''\
{imports}

class {cls}(netweir.CSVFeedSpider):
    name = {name}
    allowed_domains = [{host}]
    start_urls = [{csv_feed}]
    delimiter = ","
    quotechar = '"'
    # The column names; without them, the first row names the columns.
    # headers = ["id", "name", "description"]
{settings}
    def parse_row(self, page, row):
        """What to take from each row: a dict keyed by the headers."""
        yield row
''',
}


DESCRIPTIONS = {
    "basic": "a parse callback (the default)",
    "crawl": "Follow rules, with an Item extracted in Rust",
    "sitemap": "the pages the site's sitemaps list",
    "xmlfeed": "an XML feed (RSS, Atom), node by node",
    "csvfeed": "a CSV feed, row by row",
}


def _shadows(module: str, path: Path) -> bool:
    """Whether a file called ``module``.py would hide an importable module,
    as Scrapy refuses: one of the standard library's, an installed one,
    netweir, or the project's settings."""
    if module in ("netweir", "settings") or module in sys.stdlib_module_names:
        return True
    try:
        spec = importlib.util.find_spec(module)
    except (ImportError, ValueError):
        return False
    # The file itself, found on sys.path when run from its folder.
    return spec is not None and not (spec.origin and Path(spec.origin) == path.resolve())


def genspider(args) -> int:
    if args.list:
        for name, what in DESCRIPTIONS.items():
            print(f"{name:<8} {what}")
        return 0
    if args.name is None or args.domain is None:
        raise _Usage("genspider takes a NAME and a DOMAIN (or a URL)")
    if args.template not in TEMPLATES:
        raise _Usage(f"no template {args.template!r}; the templates are: {', '.join(TEMPLATES)}")
    if not _SPIDER_NAME.fullmatch(args.name):
        raise _Usage(
            f"{args.name!r}: a spider's name starts with a letter and is letters, digits,"
            " '_' and '-'"
        )
    if "://" in args.domain:
        url = args.domain
        parts = urlsplit(url)
        host = parts.netloc
        if parts.scheme not in ("http", "https") or not _HOST.fullmatch(host):
            raise _Usage(f"{url!r}: not an http(s) URL")
        if re.search(r"[\s\"'\\]", url):
            raise _Usage(f"{url!r}: a URL with spaces, quotes or backslashes")
        url = url if parts.path else url + "/"
    else:
        host = args.domain
        if not _HOST.fullmatch(host):
            raise _Usage(f"{host!r}: not a domain, such as example.com, or a URL")
        url = f"https://{host}/"
    origin = "{0.scheme}://{0.netloc}".format(urlsplit(url))
    try:
        folder: Path | None = spiders_dir()
    except _Usage:
        folder = None
    module = args.name.replace("-", "_")
    path = (folder or Path.cwd()) / (module + ".py")
    if _shadows(module, path):
        raise _Usage(
            f"{args.name!r} is the name of a Python module; a spider file of that name would"
            " hide it from code run beside it. Choose another name"
        )
    if not args.force:
        if os.path.lexists(path):
            raise _Usage(f"{path} is there already (--force writes over it)")
        if folder is not None:
            taken = project_spiders().get(args.name)
            if taken:
                raise _Usage(f"the project has a spider named {args.name!r}, in {taken[0][1]}")
    imports = "import netweir\n"
    if folder is not None:
        imports += "from settings import SETTINGS\n"
    cls = "".join(part.capitalize() for part in re.split(r"[_-]", args.name)) + "Spider"
    text = TEMPLATES[args.template].format(
        imports=imports,
        cls=cls,
        name=json.dumps(args.name),
        host=json.dumps(host),
        url=json.dumps(url),
        raw_url=url,
        robots=json.dumps(origin + "/robots.txt"),
        # A URL given is the feed; a domain's feed is guessed at.
        xml_feed=json.dumps(url if "://" in args.domain else origin + "/feed.xml"),
        csv_feed=json.dumps(url if "://" in args.domain else origin + "/feed.csv"),
        settings="    settings = SETTINGS\n" if folder is not None else "",
    )
    _create(path, text, overwrite=args.force)
    shown = path.relative_to(Path.cwd()) if path.is_relative_to(Path.cwd()) else path
    print(f"Spider {args.name} ({args.template}) written to {shown}")
    return 0


# --- list --------------------------------------------------------------------


def list_spiders(args) -> int:
    found = project_spiders()
    for name in sorted(found):
        if len(found[name]) > 1:
            files = ", ".join(str(path) for _, path in found[name])
            print(f"netweir: two spiders are named {name!r}: in {files}", file=sys.stderr)
        print(name)
    return 0


# --- check -------------------------------------------------------------------


def _folder_of(target: str | None) -> Path:
    if target is not None and Path(target).is_file():
        return Path(target).resolve().parent
    if target is not None and target.endswith(".py"):
        raise _Usage(f"no such file: {target}")
    return spiders_dir().resolve()


def check(args) -> int:
    from netweir import contracts

    groups: dict[Path, list[str]] = {}
    for target in args.spiders or [None]:
        group = groups.setdefault(_folder_of(target), [])
        if target is not None:
            group.append(str(Path(target).resolve()) if Path(target).is_file() else target)
    if len(groups) > 1:
        return _check_apart(groups, args)
    classes = [c for target in args.spiders or [None] for c in spider_classes(target)]
    if args.list:
        for cls in classes:
            methods = contracts.tested_methods(cls)
            if methods:
                print(cls.name or cls.__name__)
                for method in methods:
                    print(f"  * {method}")
        return 0
    spiders = [_instance(cls, args) for cls in classes]
    result = contracts.new_result(sys.stderr, args.verbose)
    started = time.time()
    for spider in spiders:
        requests, waiting = contracts.requests_for(spider, result)
        if not requests:
            continue
        # Only the hooks see what the callbacks give: nothing is piped.
        spider.pipelines = []
        _run(spider, requests)
        contracts.never_reached(spider, waiting, result)
    found = contracts.outcome(result)
    if args.results is not None:
        # One of several processes: the first sums them up.
        Path(args.results).write_text(json.dumps(found), encoding="utf-8")
    else:
        contracts.summary(sys.stderr, found, started)
    return 0 if result.wasSuccessful() else 1


def _check_apart(groups: dict[Path, list[str]], args) -> int:
    """Checks each folder's spiders in a process of its own (a process
    loads one folder's), then reports as one run would."""
    from netweir import contracts

    options = [*(f"-s{s}" for s in args.set), *(f"-a{a}" for a in args.arguments)]
    options += ["-v"] * args.verbose + ["-l"] * args.list
    started = time.time()
    found: dict[str, Any] = {"run": 0, "failures": [], "errors": []}
    worst = 0
    with tempfile.TemporaryDirectory() as scratch:
        for n, targets in enumerate(groups.values()):
            results = Path(scratch) / f"{n}.json"
            command = [sys.executable, "-m", "netweir", "check", *options, "--results"]
            done = subprocess.run([*command, str(results), "--", *targets])
            worst = max(worst, done.returncode)
            if results.exists():
                part = json.loads(results.read_text(encoding="utf-8"))
                found["run"] += part["run"]
                found["failures"] += part["failures"]
                found["errors"] += part["errors"]
    if not args.list:
        contracts.summary(sys.stderr, found, started)
    return worst


# --- parse -------------------------------------------------------------------


class _Shown:
    """A Request as Scrapy's parse prints it."""

    def __init__(self, request: Any):
        self.text = f"<{request.method} {request.url}>"

    def __repr__(self) -> str:
        return self.text


def _covers(cls: type, url: str) -> bool:
    from netweir._native import _offsite

    for domain in [*cls.allowed_domains, *([cls.name] if cls.name else [])]:
        try:
            if not _offsite(url, [domain]):
                return True
        except ValueError:
            continue
    return False


def _admits(rule: Any, url: str) -> bool:
    """Whether ``rule`` takes ``url``, by its allow, deny and domains."""
    from netweir._native import _offsite

    if rule.allow and not any(re.search(p, url) for p in rule.allow):
        return False
    if any(re.search(p, url) for p in rule.deny):
        return False
    if rule.allow_domains and _offsite(url, rule.allow_domains):
        return False
    return not (rule.deny_domains and not _offsite(url, rule.deny_domains))


def _rule_hits(rule: Any, page: Any) -> list[Any]:
    """What a rule's query finds on ``page``: elements or strings."""
    if rule.kind == "css":
        return page.css(rule.query)
    if rule.kind == "xpath":
        return page.xpath(rule.query)
    if rule.kind == "navigation":
        from netweir import auto

        schema, part = rule.query.split(":")
        [kind] = [t for t in vars(auto).values() if getattr(t, "_schema", None) == schema]
        found = page.extract(kind)[part]
        if isinstance(found, dict):
            found = [found]
        return [link["url"] for link in found or [] if link.get("url")]
    return []


def _rule_links(spider: Any, page: Any, hows: dict[int, Any]) -> list[Any]:
    """The Requests the spider's rules make from ``page``: Python's version
    of what the crawl does in Rust, for showing them."""
    from netweir import Request
    from netweir._native import _offsite

    seen: set[str] = set()
    out = []
    for rule in spider.rules:
        for hit in _rule_hits(rule, page):
            href = hit if isinstance(hit, str) else hit.attr("href")
            if not href:
                continue
            url = page.urljoin(href.strip())
            if not url.startswith(("http://", "https://")) or url in seen:
                continue
            # As the crawl does: allowed_domains drops links off the sites.
            if spider.allowed_domains and _offsite(url, spider.allowed_domains):
                continue
            if _admits(rule, url):
                seen.add(url)
                request = Request(url, priority=rule.priority, referer=page.url)
                hows[id(request)] = ("rule", rule)
                out.append(request)
    return out


def _resolve(spider: Any, fn: Any) -> Any:
    return getattr(spider, fn) if isinstance(fn, str) else fn


async def _outputs(spider: Any, how: tuple, page: Any, hows: dict[int, Any]) -> list[Any]:
    """What handling ``page`` the way ``how`` says gives: a callback's
    output, a rule's item and links, or a rules page's links."""
    from netweir import Spider
    from netweir.contracts import _collect

    if how[0] == "callback":
        return await _collect(_resolve(spider, how[1])(page, **how[2]))
    if how[0] == "rule":
        rule, out = how[1], []
        if rule.extract is not None:
            out.append(rule.extract.extract(page))
        if rule.callback is not None:
            out += await _collect(_resolve(spider, rule.callback)(page))
        if rule.follow:
            out += _rule_links(spider, page, hows)
        return out
    # A page the rules apply to, as a start page is: its links, and parse
    # if the spider writes its own.
    out = []
    if type(spider).parse is not Spider.parse:
        out += await _collect(spider.parse(page))
    return out + _rule_links(spider, page, hows)


def parse(args) -> int:
    from netweir import Request

    try:
        meta = json.loads(args.meta) if args.meta else {}
        cb_kwargs = json.loads(args.cbkwargs) if args.cbkwargs else {}
    except ValueError as e:
        raise _Usage(f"--meta and --cbkwargs take JSON: {e}") from None
    if not isinstance(meta, dict) or not isinstance(cb_kwargs, dict):
        raise _Usage("--meta and --cbkwargs take a JSON object")
    if args.depth < 1:
        raise _Usage("--depth is 1 or more")
    if args.spider is not None:
        classes = spider_classes(args.spider)
        if len(classes) != 1:
            found = ", ".join(c.name or c.__name__ for c in classes) or "none"
            raise _Usage(f"{args.spider}: expected one Spider, found {found}")
        cls = classes[0]
    else:
        candidates = [c for c in spider_classes(None) if _covers(c, args.url)]
        if len(candidates) != 1:
            names = ", ".join(c.name or c.__name__ for c in candidates)
            raise _Usage(
                f"{len(candidates)} spiders cover {args.url}{': ' + names if names else ''};"
                " name one with --spider"
            )
        cls = candidates[0]
    spider = _instance(cls, args)
    if args.callback is not None:
        if not callable(getattr(spider, args.callback, None)):
            raise _Usage(f"{cls.__name__} has no method {args.callback!r}")
        how: tuple = ("callback", args.callback, cb_kwargs)
    elif args.rules:
        if not cls.rules:
            raise _Usage(f"{cls.__name__} has no rules; give a callback with -c")
        rule = next((r for r in cls.rules if _admits(r, args.url)), None)
        if rule is None:
            raise _Usage(f"no rule of {cls.__name__} takes {args.url}")
        # A rule that only follows links has nothing to say: parse, as in Scrapy.
        how = ("rule", rule) if rule.extract or rule.callback else ("callback", "parse", cb_kwargs)
    elif cls.rules:
        how = ("start",)
    else:
        how = ("callback", "parse", cb_kwargs)
    if not args.pipelines:
        spider.pipelines = []

    levels: dict[int, tuple[list, list]] = {}
    hows: dict[int, Any] = {}
    failed = []

    def wrap(request: Any, how: tuple, level: int) -> Any:
        async def callback(page: Any):
            try:
                out = await _outputs(spider, how, page, hows)
            except Exception:
                failed.append(page.url)
                print(f"netweir: the callback raised for {page.url}:", file=sys.stderr)
                traceback.print_exc()
                return
            items = [x for x in out if not isinstance(x, Request)]
            requests = [x for x in out if isinstance(x, Request)]
            levels.setdefault(level, ([], []))
            levels[level][0].extend(items)
            levels[level][1].extend(requests)
            if args.pipelines:
                for item in items:
                    yield item
            if level < args.depth:
                for r in requests:
                    yield wrap(r, _how_of(r), level + 1)

        def lost(request: Any, error: BaseException) -> None:
            failed.append(request.url)
            print(f"netweir: {request.url}: {error}", file=sys.stderr)

        return request.replace(callback=callback, errback=lost)

    def _how_of(request: Any) -> tuple:
        if id(request) in hows:
            return hows[id(request)]
        if request.callback is not None:
            return ("callback", request.callback, {})
        return ("start",) if spider.rules else ("callback", "parse", {})

    # Asked for by name, so fetched even if the spider's domains leave it out.
    _run(spider, [wrap(Request(args.url, meta=meta, dont_filter=True), how, 1)])
    if 1 not in levels and not failed:
        print(
            f"netweir: {args.url} never reached the callback (robots.txt or a block kept it out?)",
            file=sys.stderr,
        )
        return 1
    _print_levels(levels, args)
    return 1 if failed else 0


def _print_levels(levels: dict[int, tuple[list, list]], args) -> None:
    def items(found: list) -> None:
        if not args.noitems:
            print("# Scraped Items ", "-" * 60)
            plain = [dataclasses.asdict(x) if dataclasses.is_dataclass(x) else x for x in found]
            pprint.pprint(plain)

    def requests(found: list) -> None:
        if not args.nolinks:
            print("# Requests ", "-" * 65)
            pprint.pprint([_Shown(r) for r in found])

    deepest = max(levels, default=1)
    if args.verbose:
        for level in range(1, deepest + 1):
            print(f"\n>>> DEPTH LEVEL: {level} <<<")
            items(levels.get(level, ([], []))[0])
            requests(levels.get(level, ([], []))[1])
    else:
        print(f"\n>>> STATUS DEPTH LEVEL {deepest} <<<")
        items([x for level in sorted(levels) for x in levels[level][0]])
        requests(levels.get(deepest, ([], []))[1])


# --- version -----------------------------------------------------------------


def version(args) -> int:
    import netweir
    from netweir._native import _build_info

    if not args.verbose:
        print(f"Netweir {netweir.__version__}")
        return 0
    info = _build_info()
    profiles = ", ".join(f"{name} ({short})" if short else name for name, short in info["profiles"])
    rows = [
        ("Netweir", netweir.__version__),
        ("Python", sys.version.replace("\n", " ")),
        ("Platform", platform.platform()),
        ("Rust", info["rustc"]),
        ("lexbor", info["lexbor"]),
        ("Profiles", profiles),
    ]
    for name, value in rows:
        print(f"{name:<8} : {value}")
    return 0


def main(args) -> int:
    """Runs one of this module's commands, with its exit status."""
    if args.command in ("check", "parse"):
        logging.basicConfig(level=logging.WARNING, format="%(levelname)s %(message)s")
    command = {
        "startproject": startproject,
        "genspider": genspider,
        "list": list_spiders,
        "check": check,
        "parse": parse,
        "version": version,
    }[args.command]
    try:
        return command(args)
    except (_Usage, ValueError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    except (_Failed, OSError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("netweir: interrupted", file=sys.stderr)
        return 130
