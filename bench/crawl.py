"""A whole crawl, timed: netweir against Scrapy, Scrapling, Crawlee (for
Python and for Node.js), Crawl4AI, spider-rs, Colly, a plain httpx +
selectolax script and self-hosted Firecrawl.

    uv run python bench/crawl.py                     # every crawler it can reach
    uv run python bench/crawl.py --only netweir      # just one
    uv run python bench/crawl.py --sweep             # 10 to 500 in flight
    uv run python bench/crawl.py --boss              # the final boss pages

bench/README.md has the steps that set up a machine from nothing.

A local server plays 100 sites (site0.localhost ... site99.localhost) of 100
pages each, every answer 50 ms late, the way a real server keeps you
waiting. Each crawler starts at every site's front page, follows every link
and pulls a title and a price from each of the 10,000 pages, with the same
limits: 100 requests in flight (or each of --sweep's 10, 50, 100, 200 and
500), 8 per site, no robots.txt, no throttling. Each runs in its own
process, so its CPU time and peak memory are its own.

--boss serves the final boss pages from bench/boss.py instead: its 1,000
pages (tiny broken fragments, real pages re-encoded, giants of 10 to 30
MB), 10 per site, with the HTTP charset where the page has one. Each page
gets the crawl's title, price and links put in at the start of its <body>,
in the page's own encoding, or after its BOM where it has no <body> tag;
the price's pound sign is `&pound;` there, so it reads the same in every
encoding. bench/boss.py builds the pages (see its docstring); --boss
FOLDER takes another batch of them, such as its --smoke one.

Every run is repeated (--repeats, 3 by default, a different crawler first
each time). Each round of crawlers at a setting waits for the one-minute
load average to fall below 3, unless --now says not to, and every run
records the load it started at. A crawl still running after --timeout
seconds (600) is killed and counted as failed. The results go to bench/results/<date>-<host>/,
as crawl.json or boss.json (the machine, every version, every run and the
median, min and max of each figure) and a .md of the tables. raw/ beside
them holds every run's items and server timings, gzipped.

What a run measures:

- pages/s: items over the wall time of the crawler's process.
- latency: from the request arriving at the server to the crawler having
  the page's title and price, per page, p50 and p95. Every crawler stamps
  each item with the wall clock when it has it; the server stamps each
  request when it arrives and when its answer is written. Both are the
  same clock on the same machine.
- server: arrival to answer written, which is the 50 ms plus the
  server's own delay; the rest of a page's latency (client, p50 and p95)
  is the crawler's: reading the answer, waiting for its turn, parsing and
  extracting. That split is the network-against-parse figure for every
  crawler, measured from outside. For Netweir, its response_received
  signal also splits the client time in two (in crawl.json): until the
  response reached Python (reading it, Netweir's Rust side and its
  queue), and the callback's CSS queries and the item after.
- CPU: user + system time, per page and over the wall time (1.0 is one
  core busy throughout).
- peak memory: the process's peak resident set, and that divided by the
  most requests the server saw in flight at once, which it records too.
- correctness: each item's title and price against the page's own, with
  the pages wrongly read, missing and read more than once.

Install what you want to compare against (Scrapling needs Python 3.13 or
older for now); the crawl group pins each to the version the published
table used:

    uv sync --group crawl

The ones that aren't Python are small programs in bench/others. Build them
once (each is skipped until it's built):

    (cd bench/others/spider && cargo build --release --locked)
    (cd bench/others/colly && go build -o bench-colly .)
    (cd bench/others/crawlee-node && npm ci)

Not every tool can be held to every limit, and where one can't, its runner
says what it does instead: Crawlee has no per-site limit; spider-rs and
Crawl4AI keep the total near the limit by crawling a few sites at a time
(at 10 in flight, spider-rs runs one site at 8, and Crawl4AI one at its
own 20), and Crawl4AI's per-site limit is its own 20, not 8.

Firecrawl runs only if its API answers on localhost:3002, and only at 100
in flight, the one setting it has. Start it from its own checkout, with
USE_DB_AUTHENTICATION=false and ALLOW_LOCAL_WEBHOOKS=true (it refuses local
addresses otherwise) in its .env, and the same ALLOW_LOCAL_WEBHOOKS=true on
its api service:

    cd "$FIRECRAWL_DIR" && docker compose up -d

FIRECRAWL_DIR says where that checkout is; its containers are the ones
measured. See run_firecrawl for how its numbers are taken.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import datetime
import functools
import gzip
import http.client
import json
import math
import os
import platform
import random
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

SITES = 100
PAGES = 100
LATENCY = 0.05
CONCURRENCY = 100
SWEEP = [10, 50, 100, 200, 500]
PER_SITE = 8
QUIET = 3.0
MAX_WAIT = 2 * 60 * 60
TIMEOUT = 600.0
HERE = Path(__file__).resolve().parent
BOSS = HERE / ".cache" / "boss" / "1000-20261119-0"


# --- the sites ---------------------------------------------------------------


def page(site: int, n: int) -> bytes:
    rng = random.Random(site * 1000 + n)
    links = "".join(
        f'<li><a class="next" href="/p/{(n * 3 + k) % PAGES}">more {k}</a></li>' for k in (1, 2, 3)
    )
    filler = "".join(
        f"<article class='card'><h3>{rng.random():.6f}</h3><p>{'lorem ipsum ' * 8}</p></article>"
        for _ in range(12)
    )
    return (
        f"<!doctype html><html><head><title>Site {site}</title></head><body>"
        f"<h1 class='title'>Site {site} page {n}</h1><p class='price'>£{rng.randint(1, 99)}.99</p>"
        f"<ul class='nav'>{links}</ul>{filler}</body></html>"
    ).encode()


def answer(site: int, n: int) -> tuple[str, str]:
    """The title and price page(site, n) holds: its price comes after the
    twelve numbers of its filler."""
    rng = random.Random(site * 1000 + n)
    for _ in range(12):
        rng.random()
    return f"Site {site} page {n}", f"£{rng.randint(1, 99)}.99"


def boss_page(body: bytes, site: int, n: int) -> bytes:
    """A final boss page with the crawl's title, price and links in it, at
    the start of its <body>, or after its BOM where it has none, written in
    the page's encoding."""
    title, price = answer(site, n)
    links = "".join(
        f'<li><a class="next" href="/p/{(n * 3 + k) % PAGES}">more {k}</a></li>' for k in (1, 2, 3)
    )
    part = (
        f"<h1 class='title'>{title}</h1><p class='price'>{price.replace('£', '&pound;')}</p>"
        f"<ul class='nav'>{links}</ul>"
    )
    if body.startswith((b"\xff\xfe", b"\xfe\xff")):
        encoding, start, step = ("utf-16-le" if body[0] == 0xFF else "utf-16-be"), 2, 2
    else:
        encoding, start, step = "ascii", 3 if body.startswith(b"\xef\xbb\xbf") else 0, 1

    def find(what: str, at: int) -> int:
        # In UTF-16 a match must start on a character, not halfway in.
        needle = what.encode(encoding)
        while (at := lower.find(needle, at)) != -1 and (at - start) % step:
            at += 1
        return at

    lower = body.lower()
    at = find("<body", start)
    end = find(">", at) if at != -1 else -1
    cut = end + step if end != -1 else start
    return body[:cut] + part.encode(encoding) + body[cut:]


def url_key(url: str, site_ports: list[int]) -> tuple[int, int] | None:
    """(site, page) of one of the server's URLs, or None if it isn't one."""
    parts = urllib.parse.urlsplit(url)
    host = parts.hostname or ""
    try:
        if host.startswith("site"):
            site = int(host.split(".")[0].removeprefix("site"))
        else:
            site = site_ports.index(parts.port)
        path = parts.path.rstrip("/")
        n = 0 if path == "" else int(path.removeprefix("/p/"))
    except ValueError:
        return None
    return (site, n) if 0 <= site < SITES and 0 <= n < PAGES else None


#: What the server answers and what it saw: (site, page, ...) -> (body,
#: content type), and (site, page or -1, arrived, answered) per request.
SITE_PAGES: dict[tuple[int, int], tuple[bytes, bytes]] = {}
SEEN: list[tuple[int, int, float, float]] = []


def load_pages(boss: Path | None) -> None:
    global PAGES
    if boss is None:
        for s in range(SITES):
            for n in range(PAGES):
                SITE_PAGES[s, n] = page(s, n), b"text/html; charset=utf-8"
        return
    sys.path.insert(0, str(HERE))
    from boss import load_corpus

    batch = load_corpus(boss)["batch"]
    PAGES = len(batch) // SITES
    for i, about in enumerate(batch):
        body = (boss / f"{i:04}.html").read_bytes()
        s, n = divmod(i, PAGES)
        ctype = "text/html"
        if about["transport"]:
            ctype += f"; charset={about['transport']}"
        SITE_PAGES[s, n] = boss_page(body, s, n), ctype.encode()


async def handle(
    reader: asyncio.StreamReader, writer: asyncio.StreamWriter, site: int | None = None
) -> None:
    try:
        while True:
            head = await reader.readuntil(b"\r\n\r\n")
            arrived = time.time()
            lines = head.decode("latin-1").split("\r\n")
            path = lines[0].split(" ")[1]
            if path == "/_bench":
                # What the server saw since it was last asked, for crawl.py.
                body = json.dumps(SEEN).encode()
                SEEN.clear()
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n" % len(body) + body)
                await writer.drain()
                continue
            host = next(
                (
                    line.split(":", 1)[1].strip()
                    for line in lines[1:]
                    if line.lower().startswith("host:")
                ),
                "",
            )
            await asyncio.sleep(LATENCY)
            n = -1
            try:
                s = int(host.split(".")[0].removeprefix("site")) if site is None else site
                # Any path but /p/N is page 0, so /robots.txt allows everything.
                if path.startswith("/p/"):
                    n = int(path.rsplit("/", 1)[1])
                elif path == "/":
                    n = 0
                body, ctype = SITE_PAGES[s, max(n, 0)]
                status = b"200 OK"
            except (ValueError, KeyError):
                s, body, ctype, status = -1, b"not found", b"text/plain", b"404 Not Found"
            writer.write(
                b"HTTP/1.1 " + status + b"\r\nContent-Type: " + ctype + b"\r\n"
                b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
            )
            await writer.drain()
            SEEN.append((s, n, arrived, time.time()))
    except (asyncio.IncompleteReadError, ConnectionError, IndexError):
        pass
    finally:
        writer.close()


async def serve(port_file: str) -> None:
    # *.localhost resolves to ::1 or 127.0.0.1 depending on the client, so
    # listen on both, on one port.
    v6 = await asyncio.start_server(handle, host="::1", port=0, backlog=1024)
    port = v6.sockets[0].getsockname()[1]
    v4 = await asyncio.start_server(handle, host="127.0.0.1", port=port, backlog=1024)
    # Firecrawl's containers can't resolve siteN.localhost; they reach the
    # host as host.docker.internal. So the same pages are also served on one
    # port per site, where the port picks the site instead of the Host
    # header: same handler, same latency, and the others never see them.
    by_port = [
        await asyncio.start_server(
            functools.partial(handle, site=s), host="127.0.0.1", port=0, backlog=1024
        )
        for s in range(SITES)
    ]
    ports = [port] + [server.sockets[0].getsockname()[1] for server in by_port]
    with open(port_file + ".tmp", "w") as f:
        f.write(" ".join(map(str, ports)))
    os.replace(port_file + ".tmp", port_file)
    servers = [v6, v4, *by_port]
    await asyncio.gather(*(server.serve_forever() for server in servers))


def server_seen(port: int) -> list[list]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/_bench", timeout=60) as response:
        return json.load(response)


def start_urls(port: int, path: str = "p/0") -> list[str]:
    """Every site's first page. Page 0 is also linked from page 33 (page 3
    of 10 in --boss), so a crawler sees each page once."""
    return [f"http://site{s}.localhost:{port}/{path}" for s in range(SITES)]


# --- the crawlers --------------------------------------------------------------
# Each returns the number of items it extracted, and keep()s each one.

ITEMS: list[dict] = []


def keep(url: str, title: str | None, price: str | None, **more) -> None:
    ITEMS.append({"url": url, "title": title, "price": price, "t": time.time(), **more})


def run_netweir(port: int, declarative: bool) -> int:
    import netweir

    settings = netweir.Settings(
        concurrency=CONCURRENCY,
        per_domain=PER_SITE,
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        retries=0,
    )
    received: dict[str, float] = {}

    class Keep:
        """Keeps each item, with when its response reached Python, which
        Netweir's response_received signal tells a pipeline."""

        def response_received(self, response):
            received[response.url] = time.time()

        def process_item(self, item, spider):
            keep(item["url"], item["title"], item["price"], received=received.get(item["url"]))
            return item

    if declarative:

        class Page(netweir.Item):
            url = netweir.page_url()
            title = netweir.css("h1.title::text")
            price = netweir.css("p.price::text")

        class Crawl(netweir.Spider):
            rules = [netweir.Follow("a.next", extract=Page, follow=True)]

        # Rules extract from the pages they lead to, not from start pages: start
        # at a front page that /p/0 is linked from (the same content).
        Crawl.start_urls = start_urls(port, path="")
    else:

        class Crawl(netweir.Spider):
            def parse(self, page):
                yield {
                    "url": page.url,
                    "title": page.css("h1.title::text").get(),
                    "price": page.css("p.price::text").get(),
                }
                for href in page.css("a.next::attr(href)").getall():
                    yield page.follow(href)

        Crawl.start_urls = start_urls(port)

    Crawl.settings = settings
    Crawl.pipelines = [Keep()]
    logging_off()
    Crawl().run()
    return len(ITEMS)


def run_scrapy(port: int) -> int:
    import scrapy
    from scrapy.crawler import CrawlerProcess

    count = [0]

    class Crawl(scrapy.Spider):
        name = "bench"

        async def start(self):
            for url in start_urls(port):
                yield scrapy.Request(url)

        def parse(self, response):
            count[0] += 1
            title = response.css("h1.title::text").get()
            price = response.css("p.price::text").get()
            keep(response.url, title, price)
            yield {"title": title, "price": price}
            for href in response.css("a.next::attr(href)").getall():
                yield response.follow(href)

    process = CrawlerProcess(
        settings={
            "CONCURRENT_REQUESTS": CONCURRENCY,
            "CONCURRENT_REQUESTS_PER_DOMAIN": PER_SITE,
            "ROBOTSTXT_OBEY": False,
            "AUTOTHROTTLE_ENABLED": False,
            "DOWNLOAD_DELAY": 0,
            "RETRY_ENABLED": False,
            "LOG_LEVEL": "ERROR",
            "TELNETCONSOLE_ENABLED": False,
        }
    )
    process.crawl(Crawl)
    process.start()
    return count[0]


def run_scrapling(port: int) -> int:
    from scrapling.spiders import Request, Spider

    count = [0]

    class Crawl(Spider):
        name = "bench"
        start_urls = start_urls(port)
        concurrent_requests = CONCURRENCY
        concurrent_requests_per_domain = PER_SITE
        robots_txt_obey = False
        autothrottle_enabled = False
        download_delay = 0
        logging_level = 40

        async def parse(self, response):
            count[0] += 1
            title = response.css("h1.title::text").get()
            price = response.css("p.price::text").get()
            keep(response.url, title, price)
            yield {"title": title, "price": price}
            for href in response.css("a.next::attr(href)").getall():
                yield Request(response.urljoin(href), callback=self.parse)

    Crawl().start()
    return count[0]


def run_httpx(port: int) -> int:
    import httpx
    from selectolax.lexbor import LexborHTMLParser

    async def crawl() -> int:
        seen = set(start_urls(port))
        queue: asyncio.Queue[str] = asyncio.Queue()
        for url in seen:
            queue.put_nowait(url)
        per_site: dict[str, asyncio.Semaphore] = {}
        count = 0
        limits = httpx.Limits(max_connections=CONCURRENCY, max_keepalive_connections=CONCURRENCY)
        async with httpx.AsyncClient(limits=limits, timeout=30) as client:

            async def worker() -> None:
                nonlocal count
                while True:
                    url = await queue.get()
                    host = httpx.URL(url).host
                    gate = per_site.setdefault(host, asyncio.Semaphore(PER_SITE))
                    async with gate:
                        response = await client.get(url)
                    tree = LexborHTMLParser(response.text)
                    title = tree.css_first("h1.title")
                    price = tree.css_first("p.price")
                    keep(str(response.url), title and title.text(), price and price.text())
                    count += 1
                    for a in tree.css("a.next"):
                        link = str(response.url.join(a.attributes["href"]))
                        if link not in seen:
                            seen.add(link)
                            queue.put_nowait(link)
                    queue.task_done()

            workers = [asyncio.create_task(worker()) for _ in range(CONCURRENCY)]
            await queue.join()
            for w in workers:
                w.cancel()
        return count

    return asyncio.run(crawl())


def run_crawlee(port: int) -> int:
    from crawlee import ConcurrencySettings
    from crawlee.crawlers import ParselCrawler, ParselCrawlingContext
    from crawlee.storage_clients import MemoryStorageClient

    count = [0]
    # Crawlee scales its pool with the machine's load; pinning the bounds
    # holds it at the same limit the others get. It has no per-site limit.
    crawler = ParselCrawler(
        concurrency_settings=ConcurrencySettings(
            min_concurrency=CONCURRENCY,
            desired_concurrency=CONCURRENCY,
            max_concurrency=CONCURRENCY,
        ),
        max_request_retries=0,
        storage_client=MemoryStorageClient(),
        configure_logging=False,
    )

    @crawler.router.default_handler
    async def parse(context: ParselCrawlingContext) -> None:
        count[0] += 1
        keep(
            context.request.url,
            context.selector.css("h1.title::text").get(),
            context.selector.css("p.price::text").get(),
        )
        await context.enqueue_links(selector="a.next")

    asyncio.run(crawler.run(start_urls(port)))
    return count[0]


def run_crawl4ai(port: int) -> int:
    os.environ["CRAWL4AI_NO_CLOUD_NOTICE"] = "1"
    from crawl4ai import AsyncWebCrawler, CacheMode, CrawlerRunConfig, HTTPCrawlerConfig
    from crawl4ai.async_crawler_strategy import AsyncHTTPCrawlerStrategy
    from crawl4ai.deep_crawling import BFSDeepCrawlStrategy
    from crawl4ai.extraction_strategy import JsonCssExtractionStrategy

    schema = {
        "name": "page",
        "baseSelector": "body",
        "fields": [
            {"name": "title", "selector": "h1.title", "type": "text"},
            {"name": "price", "selector": "p.price", "type": "text"},
        ],
    }

    async def crawl() -> int:
        # Its HTTP strategy, not a browser: the same job the others do.
        strategy = AsyncHTTPCrawlerStrategy(browser_config=HTTPCrawlerConfig())
        async with AsyncWebCrawler(crawler_strategy=strategy, verbose=False) as crawler:
            # A deep crawl goes level by level through Crawl4AI's default
            # dispatcher, which takes no limit from us and runs up to 20
            # pages at once; so CONCURRENCY // 20 sites at a time keeps the
            # whole crawl at CONCURRENCY in flight. Its per-site limit is
            # that 20, not PER_SITE.
            gate = asyncio.Semaphore(max(CONCURRENCY // 20, 1))

            async def site(url: str) -> list[str]:
                config = CrawlerRunConfig(
                    deep_crawl_strategy=BFSDeepCrawlStrategy(max_depth=PAGES),
                    extraction_strategy=JsonCssExtractionStrategy(schema),
                    cache_mode=CacheMode.BYPASS,
                    verbose=False,
                )
                async with gate:
                    results = await crawler.arun(url, config=config)
                # A site's pages come back together, when its deep crawl is done.
                for r in results:
                    if r.success:
                        found = json.loads(r.extracted_content or "[]") or [{}]
                        keep(r.url, found[0].get("title"), found[0].get("price"))
                return [r.url for r in results if r.success]

            fetched = [
                u for urls in await asyncio.gather(*map(site, start_urls(port))) for u in urls
            ]
            # It fetches each front page twice, once as the start and once
            # from page 33's link; the items are the pages, counted once.
            if len(fetched) != len(set(fetched)):
                print(f"{len(fetched) - len(set(fetched))} pages fetched twice", file=sys.stderr)
            return len(set(fetched))

    return asyncio.run(crawl())


# The ones that aren't Python: a program each under bench/others, given the
# port and the limits, printing the item count and writing the items to
# BENCH_ITEMS. Build them first (see the module's docstring).
OTHERS = HERE / "others"
PROGRAMS = {
    "spider-rs": [str(OTHERS / "spider" / "target" / "release" / "bench-spider")],
    "colly": [str(OTHERS / "colly" / "bench-colly")],
    "crawlee-node": ["node", str(OTHERS / "crawlee-node" / "crawl.js")],
}


def program_args(name: str, concurrency: int) -> list[int]:
    if name == "crawlee-node":
        return [SITES, concurrency]
    return [SITES, concurrency, PER_SITE]


FIRECRAWL = "http://localhost:3002/v2"
FIRECRAWL_DIR = os.environ.get("FIRECRAWL_DIR", "firecrawl")


def run_firecrawl(site_ports: list[int]) -> dict:
    """Self-hosted Firecrawl, on its own worker defaults (no tuning, as in
    reliability.py). Its crawl API takes one site per job, so it gets 100
    jobs, all started at once, each held to its site's pages and to 8
    requests at a time (maxConcurrency) to mirror 8 per site; how many run
    at once overall is up to its workers, not the 100 in flight the others
    get. It returns each page's raw HTML, and the title and price come out
    of that here, with the same parse the httpx crawler uses.

    Wall time runs from the first job's start to the last job's end. Firecrawl
    runs in its containers, not in a process of ours, so CPU is the
    difference in their summed cgroup CPU counters (Docker Engine API) across
    the crawl, plus our own parse; peak memory is the peak of their summed
    memory as docker stats counts it, sampled about once a second. Its
    pages come back when the crawl is over, so it has no latency figures
    but the server's."""
    from concurrent.futures import ThreadPoolExecutor

    from selectolax.lexbor import LexborHTMLParser

    containers = subprocess.run(
        ["docker", "compose", "ps", "-q"],
        cwd=FIRECRAWL_DIR,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    if not containers:
        return {"error": f"no containers running in {FIRECRAWL_DIR}"}
    sock = subprocess.run(
        ["docker", "context", "inspect", "--format", "{{.Endpoints.docker.Host}}"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    sock = sock.removeprefix("unix://")

    class Docker(http.client.HTTPConnection):
        def connect(self):
            self.sock = socket.socket(socket.AF_UNIX)
            self.sock.connect(sock)

    def usage() -> tuple[int, int]:
        """Summed CPU nanoseconds so far, and memory in bytes now."""
        cpu = memory = 0
        for container in containers:
            conn = Docker("docker")
            conn.request("GET", f"/containers/{container}/stats?stream=false&one-shot=true")
            stats = json.load(conn.getresponse())
            conn.close()
            cpu += stats["cpu_stats"]["cpu_usage"]["total_usage"]
            mem = stats["memory_stats"]
            memory += mem["usage"] - mem.get("stats", {}).get("inactive_file", 0)
        return cpu, memory

    peak = [0]
    stop = threading.Event()

    def sample() -> None:
        while not stop.is_set():
            peak[0] = max(peak[0], usage()[1])
            stop.wait(1)

    def start(site_port: int) -> str:
        answer = firecrawl(
            "/crawl",
            {
                "url": f"http://host.docker.internal:{site_port}/p/0",
                "limit": PAGES,
                "maxConcurrency": PER_SITE,
                "crawlEntireDomain": True,  # /p/1 isn't under /p/0
                "sitemap": "skip",
                # ignoreRobotsTxt is enterprise-only; /robots.txt answers with a
                # page, which allows everything, so robots costs it one request.
                "scrapeOptions": {"formats": ["rawHtml"], "maxAge": 0},
            },
        )
        if not answer.get("success"):
            raise RuntimeError(f"Firecrawl refused the crawl: {answer}")
        return answer["id"]

    cpu_before, _ = usage()
    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    started = time.perf_counter()
    deadline = time.monotonic() + 1800
    try:
        with ThreadPoolExecutor(SITES) as pool:
            jobs = list(pool.map(start, site_ports))
        running = set(jobs)
        while running and time.monotonic() < deadline:
            time.sleep(0.25)
            ongoing = {crawl["id"] for crawl in firecrawl("/crawl/ongoing")["crawls"]}
            # A job off the ongoing list is checked once, in case it isn't on it yet.
            running = {
                job
                for job in running
                if job in ongoing or firecrawl(f"/crawl/{job}?limit=1")["status"] == "scraping"
            }
        wall = time.perf_counter() - started
        cpu_after, _ = usage()
    finally:
        stop.set()
        sampler.join()

    parse_started = time.process_time()
    pages = 0
    items = []
    for job in jobs:
        status = firecrawl(f"/crawl/{job}")
        while True:
            for doc in status.get("data", []):
                pages += 1
                tree = LexborHTMLParser(doc.get("rawHtml") or "")
                title = tree.css_first("h1.title")
                price = tree.css_first("p.price")
                meta = doc.get("metadata") or {}
                items.append(
                    {
                        "url": meta.get("sourceURL") or meta.get("url") or "",
                        "title": title and title.text(),
                        "price": price and price.text(),
                    }
                )
            if not status.get("next"):
                break
            status = firecrawl(status["next"])
    cpu = (cpu_after - cpu_before) / 1e9 + time.process_time() - parse_started
    result = {
        "count": sum(1 for i in items if i["title"] and i["price"]),
        "items": items,
        "wall_s": wall,
        "cpu_s": cpu,
        "peak_rss_mb": peak[0] / 1e6,
    }
    if running:
        result["error"] = f"{len(running)} of {SITES} crawls unfinished after 1800 s"
        result["pages_returned"] = pages
    return result


def firecrawl(path: str, body: dict | None = None) -> dict:
    url = path if path.startswith("http") else FIRECRAWL + path
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data, {"content-type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return json.load(response)
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


def logging_off() -> None:
    import logging

    logging.getLogger("netweir").setLevel(logging.ERROR)


CRAWLERS = {
    "netweir": lambda port: run_netweir(port, declarative=False),
    "netweir-declarative": lambda port: run_netweir(port, declarative=True),
    "scrapy": run_scrapy,
    "scrapling": run_scrapling,
    "httpx+selectolax": run_httpx,
    "crawlee": run_crawlee,
    "crawl4ai": run_crawl4ai,
}
NEEDS = {
    "netweir": "netweir",
    "netweir-declarative": "netweir",
    "scrapy": "scrapy",
    "scrapling": "scrapling.spiders",
    "httpx+selectolax": "httpx",
    "crawlee": "crawlee.crawlers",
    "crawl4ai": "crawl4ai",
}
ALL = [*CRAWLERS, *PROGRAMS, "firecrawl"]


# --- running and measuring ------------------------------------------------------


def run_one(name: str, port: int, site_ports: list[int], concurrency: int) -> dict:
    """One crawl: the crawler's process's figures, its items, what the
    server saw, and its output."""
    server_seen(port)  # forget anything before this run
    if name == "firecrawl":
        out = run_firecrawl(site_ports)
        out.setdefault("items", [])
        out.update(stdout="", stderr="", exit=0 if "error" not in out else 1)
        out["seen"] = server_seen(port)
        return out
    with tempfile.TemporaryDirectory(prefix="netweir-bench-") as tmp:
        items_file = os.path.join(tmp, "items.jsonl")
        if name in PROGRAMS:
            command = [*PROGRAMS[name], str(port), *map(str, program_args(name, concurrency))]
        else:
            command = [sys.executable, __file__, "--crawler", name, "--port", str(port)]
            command += ["--concurrency", str(concurrency), "--pages", str(PAGES)]
        env = {**os.environ, "BENCH_ITEMS": items_file}
        with (
            open(os.path.join(tmp, "out"), "w+") as out,
            open(os.path.join(tmp, "err"), "w+") as err,
        ):
            started = time.perf_counter()
            proc = subprocess.Popen(command, stdout=out, stderr=err, env=env, text=True)
            # A crawl still running after TIMEOUT is killed, and counts as failed.
            killer = threading.Timer(TIMEOUT, proc.kill)
            killer.start()
            _, status, usage = os.wait4(proc.pid, 0)
            wall = time.perf_counter() - started
            killer.cancel()
            out.seek(0)
            err.seek(0)
            stdout, stderr = out.read(), err.read()
        items = []
        if os.path.exists(items_file):
            with open(items_file) as f:
                items = [json.loads(line) for line in f if line.strip()]
    lines = stdout.strip().splitlines()
    result = {
        "exit": os.waitstatus_to_exitcode(status),
        "stdout": stdout[-2000:],
        "stderr": stderr[-4000:],
        "items": items,
        "seen": server_seen(port),
        "wall_s": wall,
        "cpu_s": usage.ru_utime + usage.ru_stime,
        # ru_maxrss is bytes on macOS, kilobytes on Linux.
        "peak_rss_mb": usage.ru_maxrss / (1 if sys.platform == "darwin" else 1 / 1024) / 1e6,
    }
    if wall >= TIMEOUT:
        result["error"] = f"still running after {TIMEOUT:.0f} s, killed"
    elif result["exit"] != 0 or not lines or not lines[-1].isdigit():
        result["error"] = f"exit status {result['exit']}"
    else:
        result["count"] = int(lines[-1])
    return result


def percentile(values: list[float], p: float) -> float | None:
    """The nearest-rank percentile, or None for no values."""
    if not values:
        return None
    values = sorted(values)
    return values[max(0, math.ceil(p / 100 * len(values)) - 1)]


def most_in_flight(seen: list) -> int:
    """The most requests the server was answering at once."""
    events = sorted([(r[2], 1) for r in seen] + [(r[3], -1) for r in seen])
    now = peak = 0
    for _, step in events:
        now += step
        peak = max(peak, now)
    return peak


def figures(run: dict, site_ports: list[int]) -> dict:
    """A run's figures from its process, its items and the server's log."""
    items, seen = run["items"], run["seen"]
    count = run.get("count", 0)
    wall, cpu = run["wall_s"], run["cpu_s"]
    answered: dict[tuple[int, int], list[tuple[float, float]]] = {}
    for s, n, arrived, sent in seen:
        if n >= 0:
            answered.setdefault((s, n), []).append((arrived, sent))
    latency, client, to_python, parse = [], [], [], []
    wrong = 0
    got: dict[tuple[int, int], int] = {}
    for item in items:
        key = url_key(item.get("url") or "", site_ports)
        if key is None:
            wrong += 1
            continue
        got[key] = got.get(key, 0) + 1
        title, price = answer(*key)
        if ((item.get("title") or "").strip(), (item.get("price") or "").strip()) != (title, price):
            wrong += 1
        t = item.get("t")
        # The last answer for that page written before the item was made.
        before = [r for r in answered.get(key, []) if t is not None and r[1] <= t]
        if before:
            arrived, sent = before[-1]
            latency.append(t - arrived)
            client.append(t - sent)
            if item.get("received") and item["received"] >= sent:
                to_python.append(item["received"] - sent)
                parse.append(t - item["received"])
    in_flight = most_in_flight(seen)
    ms = lambda v, p: None if (x := percentile(v, p)) is None else x * 1000  # noqa: E731
    out = {
        "items": count,
        "pages_per_sec": count / wall,
        "wall_s": wall,
        "cpu_s": cpu,
        "cpu_ms_per_page": cpu * 1000 / max(count, 1),
        "cpu_util": cpu / wall,
        "peak_rss_mb": run["peak_rss_mb"],
        "peak_in_flight": in_flight,
        "mb_per_in_flight": run["peak_rss_mb"] / max(in_flight, 1),
        "requests": len(seen),
        "server_ms_p50": ms([r[3] - r[2] for r in seen], 50),
        "server_ms_p95": ms([r[3] - r[2] for r in seen], 95),
        "latency_ms_p50": ms(latency, 50),
        "latency_ms_p95": ms(latency, 95),
        "client_ms_p50": ms(client, 50),
        "client_ms_p95": ms(client, 95),
        "wrong": wrong,
        "missing": SITES * PAGES - len(got),
        "twice": sum(n - 1 for n in got.values()),
    }
    if to_python:
        out |= {
            "to_python_ms_p50": ms(to_python, 50),
            "to_python_ms_p95": ms(to_python, 95),
            "parse_ms_p50": ms(parse, 50),
            "parse_ms_p95": ms(parse, 95),
        }
    return out


def available(name: str) -> bool:
    if name in PROGRAMS:
        program = PROGRAMS[name][-1]
        return os.path.exists(program) and (
            name != "crawlee-node"
            or os.path.isdir(os.path.join(os.path.dirname(program), "node_modules", "crawlee"))
        )
    if name == "firecrawl":
        try:
            urllib.request.urlopen("http://localhost:3002/", timeout=5).close()
        except urllib.error.HTTPError:
            return True
        except (urllib.error.URLError, OSError):
            return False
        return True
    try:
        __import__(NEEDS[name])
    except Exception:  # noqa: BLE001 - anything that stops the import
        return False
    return True


def wait_for_quiet(deadline: float) -> float:
    while True:
        load = os.getloadavg()[0]
        if load < QUIET or time.time() >= deadline:
            return load
        time.sleep(20)


# --- the machine and the versions ------------------------------------------------


def command_output(*command: str, cwd: Path | str | None = None) -> str | None:
    try:
        out = subprocess.run(command, capture_output=True, text=True, timeout=30, cwd=cwd)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return (out.stdout or out.stderr).strip() or None if out.returncode == 0 else None


def environment() -> dict:
    from importlib.metadata import PackageNotFoundError, version

    if sys.platform == "darwin":
        cpu = command_output("sysctl", "-n", "machdep.cpu.brand_string")
        memory = int(command_output("sysctl", "-n", "hw.memsize") or 0)
        os_name = f"macOS {platform.mac_ver()[0]}"
    else:
        cpu = memory = None
        with contextlib.suppress(OSError):
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.startswith("model name"):
                    cpu = line.split(":", 1)[1].strip()
                    break
        with contextlib.suppress(OSError, ValueError, IndexError):
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    memory = int(line.split()[1]) * 1024
        os_name = platform.platform()
    python = {}
    for name in ["netweir", "scrapy", "scrapling", "httpx", "selectolax", "crawlee", "crawl4ai"]:
        with contextlib.suppress(PackageNotFoundError):
            python[name] = version(name)
    others = {}
    with contextlib.suppress(OSError, ValueError):
        package = OTHERS / "crawlee-node" / "node_modules" / "crawlee" / "package.json"
        others["crawlee-node"] = json.loads(package.read_text())["version"]
    with contextlib.suppress(OSError):
        for line in (OTHERS / "colly" / "go.mod").read_text().splitlines():
            if line.startswith("require github.com/gocolly/colly"):
                others["colly"] = line.split()[-1]
    with contextlib.suppress(OSError):
        lock = (OTHERS / "spider" / "Cargo.lock").read_text().split("[[package]]")
        for block in lock:
            if '\nname = "spider"\n' in block:
                others["spider-rs"] = block.split('version = "')[1].split('"')[0]
    if Path(FIRECRAWL_DIR).is_dir():
        others["firecrawl"] = command_output(
            "git", "log", "-1", "--format=%h %cs", cwd=FIRECRAWL_DIR
        )
    commit = command_output("git", "rev-parse", "--short", "HEAD", cwd=HERE)
    dirty = command_output("git", "status", "--porcelain", "--untracked-files=no", cwd=HERE)
    return {
        "date": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
        "host": socket.gethostname().split(".")[0],
        "cpu": cpu,
        "cores": os.cpu_count(),
        "memory_gb": round(memory / 2**30) if memory else None,
        "os": os_name,
        "arch": platform.machine(),
        "load_at_start": [round(x, 2) for x in os.getloadavg()],
        "netweir_commit": f"{commit}{' (with changes)' if dirty else ''}" if commit else None,
        "python": platform.python_version(),
        "rust": command_output("rustc", "--version"),
        "go": command_output("go", "version"),
        "node": command_output("node", "--version"),
        "python_packages": python,
        "others": others,
    }


# --- the report ---------------------------------------------------------------------

SHOWN = [
    ("pages_per_sec", "Pages/s", "{:,.0f}"),
    ("latency_ms_p50", "Latency p50", "{:.0f} ms"),
    ("latency_ms_p95", "p95", "{:.0f} ms"),
    ("client_ms_p50", "Client p50", "{:.1f} ms"),
    ("client_ms_p95", "p95", "{:.1f} ms"),
    ("server_ms_p50", "Server p50", "{:.0f} ms"),
    ("cpu_ms_per_page", "CPU/page", "{:.2f} ms"),
    ("cpu_util", "CPU use", "{:.2f}"),
    ("peak_rss_mb", "Peak memory", "{:.0f} MB"),
    ("peak_in_flight", "In flight", "{:.0f}"),
    ("mb_per_in_flight", "MB/in flight", "{:.2f}"),
]
COUNTS = ["wrong", "missing", "twice"]


def summarise(runs: list[dict]) -> dict:
    """Median, min and max of each figure over a crawler's runs at one
    setting, and the worst run's correctness counts."""
    ok = [r["figures"] for r in runs if "figures" in r]
    out = {"runs": len(runs), "failed": len(runs) - len(ok)}
    if not ok:
        return out
    for key in ok[0]:
        values = [f[key] for f in ok if f.get(key) is not None]
        if not values:
            continue
        if key in COUNTS:
            out[key] = max(values)
        else:
            out[key] = {"median": statistics.median(values), "min": min(values), "max": max(values)}
    return out


def cell(summary: dict, key: str, fmt: str) -> str:
    value = summary.get(key)
    if value is None:
        return "-"
    if key == "pages_per_sec" and summary["runs"] - summary["failed"] > 1:
        lo, hi = fmt.format(value["min"]), fmt.format(value["max"])
        return f"{fmt.format(value['median'])} ({lo}–{hi})"
    return fmt.format(value["median"])


def markdown(report: dict) -> str:
    env, summary = report["environment"], report["summary"]
    boss = report["mode"] == "boss"
    pkgs = {**env["python_packages"], **env["others"]}
    md = [
        f"## {'The final boss pages' if boss else 'A whole crawl'}, {env['date'][:10]}",
        "",
        f"{env['cpu']}, {env['cores']} cores, {env['memory_gb']} GB, {env['os']} ({env['arch']});"
        f" load {', '.join(map(str, env['load_at_start']))} at the start. Python {env['python']},"
        f" {env['rust']}, {env['go']}, Node {env['node']}. Netweir at {env['netweir_commit']}.",
        "",
        "Versions: " + ", ".join(f"{k} {v}" for k, v in sorted(pkgs.items())) + ".",
        "",
        f"{SITES} sites of {PAGES} pages, {LATENCY * 1000:.0f} ms per answer, {PER_SITE} per site."
        f" Each figure is the median of {report['repeats']} runs; pages/s gives the slowest and"
        " fastest run in brackets. Wrong, missing and twice are the worst run's.",
        "",
    ]
    head = [label for _, label, _ in SHOWN] + ["Wrong", "Missing", "Twice"]
    for level in report["levels"]:
        md += [
            f"### {level} in flight",
            "",
            "| Crawler | " + " | ".join(head) + " |",
            "|---|" + "--:|" * len(head),
        ]
        for name, s in summary[str(level)].items():
            if not s.get("pages_per_sec"):
                md.append(f"| {name} | failed in {s['failed']} of {s['runs']} runs |")
                continue
            cells = [cell(s, key, fmt) for key, _, fmt in SHOWN] + [str(s[k]) for k in COUNTS]
            md.append(f"| {name} | " + " | ".join(cells) + " |")
        md.append("")
    if len(report["levels"]) > 1:
        names = list(dict.fromkeys(n for lv in summary.values() for n in lv))
        for key, title, fmt in [
            ("pages_per_sec", "Pages a second, by requests in flight", "{:,.0f}"),
            ("latency_ms_p95", "Latency p95, by requests in flight", "{:.0f} ms"),
            ("peak_rss_mb", "Peak memory, by requests in flight", "{:.0f} MB"),
        ]:
            md += [
                f"### {title}",
                "",
                "| Crawler | " + " | ".join(map(str, report["levels"])) + " |",
                "|---|" + "--:|" * len(report["levels"]),
            ]
            for name in names:
                row = [
                    fmt.format(summary[str(lv)][name][key]["median"])
                    if summary[str(lv)].get(name, {}).get(key)
                    else "-"
                    for lv in report["levels"]
                ]
                md.append(f"| {name} | " + " | ".join(row) + " |")
            md.append("")
    return "\n".join(md)


# --- main ---------------------------------------------------------------------------


def main() -> None:
    global CONCURRENCY, PAGES, TIMEOUT
    ap = argparse.ArgumentParser()
    ap.add_argument("--crawler", help=argparse.SUPPRESS)
    ap.add_argument("--port", type=int, help=argparse.SUPPRESS)
    ap.add_argument("--serve", help=argparse.SUPPRESS)
    ap.add_argument("--pages", type=int, help=argparse.SUPPRESS)
    ap.add_argument("--only", action="append", choices=ALL)
    ap.add_argument("--concurrency", type=int, default=CONCURRENCY, help="requests in flight")
    ap.add_argument("--sweep", action="store_true", help=f"run at each of {SWEEP} in flight")
    ap.add_argument("--repeats", type=int, default=3, help="runs of each crawler at each setting")
    ap.add_argument(
        "--boss",
        nargs="?",
        type=Path,
        const=BOSS,
        metavar="FOLDER",
        help="serve bench/boss.py's pages (from FOLDER, if given)",
    )
    ap.add_argument("--parsec", help="a Parsec checkout, to build the final boss pages")
    ap.add_argument("--now", action="store_true", help="don't wait for a quiet machine")
    ap.add_argument(
        "--timeout", type=float, default=TIMEOUT, help="seconds before a crawl is killed"
    )
    ap.add_argument("--results", type=Path, help="write the results here")
    ap.add_argument("--check", action="store_true", help="exit 1 if netweir gets a page wrong")
    ap.add_argument("--json", action="store_true", help="print the summary as JSON")
    args = ap.parse_args()
    CONCURRENCY = args.concurrency
    TIMEOUT = args.timeout
    PAGES = args.pages or PAGES
    if args.serve:
        load_pages(args.boss)
        asyncio.run(serve(args.serve))
        return
    if args.crawler:
        count = CRAWLERS[args.crawler](args.port)
        if path := os.environ.get("BENCH_ITEMS"):
            with open(path, "w") as f:
                f.writelines(json.dumps(item) + "\n" for item in ITEMS)
        print(count)
        return

    if args.boss:
        pages_json = args.boss / "pages.json"
        if not pages_json.exists() and args.parsec and args.boss == BOSS:
            sys.path.insert(0, str(HERE))
            import boss

            if boss.build_corpus(BOSS, boss.PAGES, boss.SEED, args.parsec, 0) != boss.FINGERPRINT:
                sys.exit(f"these aren't Parsec's pages: run bench/fetch-pages.sh in {args.parsec}")
        if not pages_json.exists():
            sys.exit(
                f"no final boss pages in {args.boss}: pass --parsec with a Parsec checkout"
                " to build them (see bench/README.md)"
            )
        batch = len(json.loads(pages_json.read_text())["batch"])
        if batch % SITES:
            sys.exit(f"{batch} pages don't make {SITES} sites")
        PAGES = batch // SITES
    levels = SWEEP if args.sweep else [args.concurrency]
    env = environment()
    mode = "boss" if args.boss else "crawl"
    folder = args.results or HERE / "results" / f"{env['date'][:10]}-{env['host']}"
    (folder / "raw").mkdir(parents=True, exist_ok=True)

    port_file = os.path.join(tempfile.mkdtemp(prefix="netweir-bench-"), "port")
    serve_command = [sys.executable, __file__, "--serve", port_file]
    server = subprocess.Popen(serve_command + ["--boss", str(args.boss)] * bool(args.boss))
    report = {
        "mode": mode,
        "environment": env,
        "settings": {
            "sites": SITES,
            "pages_per_site": PAGES,
            "latency_ms": LATENCY * 1000,
            "per_site": PER_SITE,
            "boss": str(args.boss) if args.boss else None,
        },
        "repeats": args.repeats,
        "levels": levels,
        "runs": [],
    }
    try:
        deadline = time.time() + 120
        while not os.path.exists(port_file):
            if server.poll() is not None or time.time() > deadline:
                sys.exit("the server didn't start")
            time.sleep(0.05)
        with open(port_file) as f:
            port, *site_ports = map(int, f.read().split())
        names = []
        for name in args.only or ALL:
            if available(name):
                names.append(name)
            else:
                print(f"  {name:20} not installed, skipped", flush=True)
        quiet_until = time.time() + MAX_WAIT
        for r in range(args.repeats):
            for level in levels:
                # Once a round: each crawl adds its own load, which takes a
                # minute or more to fade from the one-minute average.
                if not args.now:
                    wait_for_quiet(quiet_until)
                # A different crawler goes first each run.
                for k in range(len(names)):
                    name = names[(k + r) % len(names)]
                    if name == "firecrawl" and level != CONCURRENCY:
                        continue
                    load = os.getloadavg()[0]
                    run = run_one(name, port, site_ports, level)
                    raw = folder / "raw" / f"{mode}-{level}-{name}-{r + 1}.json.gz"
                    with gzip.open(raw, "wt") as f:
                        json.dump({"items": run["items"], "server": run["seen"]}, f)
                    entry = {
                        "crawler": name,
                        "in_flight": level,
                        "repeat": r + 1,
                        "load": round(load, 2),
                        "exit": run["exit"],
                        "stderr": run["stderr"],
                        "raw": str(raw.relative_to(folder)),
                    }
                    if "error" in run:
                        entry["error"] = run["error"]
                        print(f"  {level:3} {name:20} failed: {run['error']}", flush=True)
                    else:
                        entry["figures"] = f = figures(run, site_ports)
                        p95 = f["latency_ms_p95"]
                        print(
                            f"  {level:3} {name:20} {f['pages_per_sec']:7.0f} pages/s"
                            f"  p95 {'-' if p95 is None else f'{p95:.0f} ms':>7}"
                            f"  {f['cpu_ms_per_page']:5.2f} ms CPU/page"
                            f"  {f['peak_rss_mb']:6.0f} MB  {f['wrong']} wrong"
                            f"  {f['missing']} missing  (load {load:.1f})",
                            flush=True,
                        )
                    report["runs"].append(entry)
    finally:
        server.terminate()
        if os.path.exists(port_file):
            os.remove(port_file)

    report["summary"] = {
        str(level): {
            name: summarise(
                [x for x in report["runs"] if x["crawler"] == name and x["in_flight"] == level]
            )
            for name in names
            if any(x["crawler"] == name and x["in_flight"] == level for x in report["runs"])
        }
        for level in levels
    }
    (folder / f"{mode}.json").write_text(json.dumps(report, indent=1) + "\n")
    md = markdown(report)
    (folder / f"{mode}.md").write_text(md + "\n")
    print("\n" + md)
    print(f"results: {folder}", file=sys.stderr)
    if args.json:
        print(json.dumps(report["summary"], indent=2))
    if args.check:
        bad = [
            f"{x['crawler']} at {x['in_flight']}: {x.get('error') or x['figures']}"
            for x in report["runs"]
            if x["crawler"].startswith("netweir")
            and ("error" in x or any(x["figures"][k] for k in COUNTS))
        ]
        if bad:
            sys.exit("netweir got pages wrong:\n" + "\n".join(bad))


if __name__ == "__main__":
    main()
