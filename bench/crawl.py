"""A whole crawl, timed: netweir against Scrapy, Scrapling, a plain
httpx + selectolax script and self-hosted Firecrawl.

    uv run python bench/crawl.py                     # every crawler it can reach
    uv run python bench/crawl.py --only netweir      # just one

A local server plays 100 sites (site0.localhost ... site99.localhost) of 100
pages each, every answer 50 ms late, the way a real server keeps you
waiting. Each crawler starts at every site's front page, follows every link
and pulls a title and a price from each of the 10,000 pages, with the same
limits: 100 requests in flight, 8 per site, no robots.txt, no throttling.
Each runs in its own process, so its CPU time and peak memory are its own.

Install what you want to compare against (Scrapling needs Python 3.13 or
older for now):

    uv pip install scrapy "scrapling[fetchers]" httpx selectolax

Firecrawl runs only if its API answers on localhost:3002. Start it from its
own checkout, with USE_DB_AUTHENTICATION=false and ALLOW_LOCAL_WEBHOOKS=true
(it refuses local addresses otherwise) in its .env, and the same
ALLOW_LOCAL_WEBHOOKS=true on its api service:

    cd "$FIRECRAWL_DIR" && docker compose up -d

FIRECRAWL_DIR says where that checkout is; its containers are the ones
measured. See run_firecrawl for how its numbers are taken.
"""

from __future__ import annotations

import argparse
import asyncio
import functools
import http.client
import json
import os
import random
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

SITES = 100
PAGES = 100
LATENCY = 0.05
CONCURRENCY = 100
PER_SITE = 8


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


async def handle(
    reader: asyncio.StreamReader, writer: asyncio.StreamWriter, site: int | None = None
) -> None:
    try:
        while True:
            head = await reader.readuntil(b"\r\n\r\n")
            lines = head.decode("latin-1").split("\r\n")
            path = lines[0].split(" ")[1]
            host = next(
                (
                    line.split(":", 1)[1].strip()
                    for line in lines[1:]
                    if line.lower().startswith("host:")
                ),
                "",
            )
            await asyncio.sleep(LATENCY)
            try:
                n = int(path.rsplit("/", 1)[1]) if path.startswith("/p/") else 0
                body = page(
                    int(host.split(".")[0].removeprefix("site")) if site is None else site, n
                )
                status = b"200 OK"
            except ValueError:
                body, status = b"not found", b"404 Not Found"
            writer.write(
                b"HTTP/1.1 " + status + b"\r\nContent-Type: text/html; charset=utf-8\r\n"
                b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
            )
            await writer.drain()
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


def start_urls(port: int, path: str = "p/0") -> list[str]:
    """Every site's first page. Page 0 is also linked from page 33, so a
    crawler sees each of the 10,000 pages once."""
    return [f"http://site{s}.localhost:{port}/{path}" for s in range(SITES)]


# --- the crawlers --------------------------------------------------------------
# Each prints the number of items it extracted.


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
    items = []

    if declarative:

        class Page(netweir.Item):
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
                    "title": page.css("h1.title::text").get(),
                    "price": page.css("p.price::text").get(),
                }
                for href in page.css("a.next::attr(href)").getall():
                    yield page.follow(href)

        Crawl.start_urls = start_urls(port)
    Crawl.settings = settings
    Crawl.pipelines = [lambda item: items.append(item) or item]
    logging_off()
    Crawl().run()
    return len(items)


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
            yield {
                "title": response.css("h1.title::text").get(),
                "price": response.css("p.price::text").get(),
            }
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
            yield {
                "title": response.css("h1.title::text").get(),
                "price": response.css("p.price::text").get(),
            }
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
                    _ = (title and title.text(), price and price.text())
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


FIRECRAWL = "http://localhost:3002/v2"
FIRECRAWL_DIR = os.environ.get("FIRECRAWL_DIR", "firecrawl")


def run_firecrawl(site_ports: list[int]) -> dict:
    """Self-hosted Firecrawl, on its own worker defaults (no tuning, as in
    reliability.py). Its crawl API takes one site per job, so it gets 100
    jobs, all started at once, each held to its site's 100 pages and to 8
    requests at a time (maxConcurrency) to mirror 8 per site; how many run
    at once overall is up to its workers, not the 100 in flight the others
    get. It returns each page's raw HTML, and the title and price come out
    of that here, with the same parse the httpx crawler uses.

    Wall time runs from the first job's start to the last job's end. Firecrawl
    runs in its containers, not in a process of ours, so CPU is the
    difference in their summed cgroup CPU counters (Docker Engine API) across
    the crawl, plus our own parse; peak memory is the peak of their summed
    memory as docker stats counts it, sampled about once a second."""
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
        return {"name": "firecrawl", "error": f"no containers running in {FIRECRAWL_DIR}"}
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
    pages = items = 0
    for job in jobs:
        status = firecrawl(f"/crawl/{job}")
        while True:
            for doc in status.get("data", []):
                pages += 1
                tree = LexborHTMLParser(doc.get("rawHtml") or "")
                title = tree.css_first("h1.title")
                price = tree.css_first("p.price")
                items += bool(title and title.text() and price and price.text())
            if not status.get("next"):
                break
            status = firecrawl(status["next"])
    cpu = (cpu_after - cpu_before) / 1e9 + time.process_time() - parse_started
    result = {
        "name": "firecrawl",
        "items": items,
        "pages_per_sec": items / wall,
        "cpu_ms_per_page": cpu * 1000 / max(items, 1),
        "peak_rss_mb": peak[0] / 1e6,
        "wall_s": wall,
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
}
NEEDS = {
    "netweir": "netweir",
    "netweir-declarative": "netweir",
    "scrapy": "scrapy",
    "scrapling": "scrapling.spiders",
    "httpx+selectolax": "httpx",
}


# --- running and measuring ------------------------------------------------------


def measure(name: str, port: int) -> dict:
    started = time.perf_counter()
    proc = subprocess.Popen(
        [sys.executable, __file__, "--crawler", name, "--port", str(port)],
        stdout=subprocess.PIPE,
        text=True,
    )
    _, status, usage = os.wait4(proc.pid, 0)
    wall = time.perf_counter() - started
    out = proc.stdout.read().strip().splitlines()
    if status != 0 or not out:
        return {"name": name, "error": f"exit status {status}"}
    items = int(out[-1])
    # ru_maxrss is bytes on macOS, kilobytes on Linux.
    rss = usage.ru_maxrss / (1 if sys.platform == "darwin" else 1 / 1024) / 1e6
    cpu = usage.ru_utime + usage.ru_stime
    return {
        "name": name,
        "items": items,
        "pages_per_sec": items / wall,
        "cpu_ms_per_page": cpu * 1000 / max(items, 1),
        "peak_rss_mb": rss,
        "wall_s": wall,
    }


def available(name: str) -> bool:
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


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--crawler", help=argparse.SUPPRESS)
    ap.add_argument("--port", type=int, help=argparse.SUPPRESS)
    ap.add_argument("--serve", help=argparse.SUPPRESS)
    ap.add_argument("--only", action="append", choices=[*CRAWLERS, "firecrawl"])
    ap.add_argument("--json", action="store_true", help="print the results as JSON")
    args = ap.parse_args()
    if args.crawler:
        print(CRAWLERS[args.crawler](args.port))
        return
    if args.serve:
        asyncio.run(serve(args.serve))
        return

    port_file = os.path.join(tempfile.mkdtemp(prefix="netweir-bench-"), "port")
    server = subprocess.Popen([sys.executable, __file__, "--serve", port_file])
    try:
        for _ in range(100):
            if os.path.exists(port_file):
                break
            time.sleep(0.05)
        with open(port_file) as f:
            port, *site_ports = map(int, f.read().split())
        names = args.only or [*CRAWLERS, "firecrawl"]
        results = []
        for name in names:
            if not available(name):
                print(f"  {name:20} not installed, skipped", flush=True)
                continue
            result = run_firecrawl(site_ports) if name == "firecrawl" else measure(name, port)
            results.append(result)
            if "error" in result:
                print(f"  {name:20} failed: {result['error']}", flush=True)
                continue
            print(
                f"  {name:20} {result['pages_per_sec']:7.0f} pages/s  "
                f"{result['cpu_ms_per_page']:6.2f} ms CPU/page  "
                f"{result['peak_rss_mb']:6.0f} MB peak  ({result['items']} items)",
                flush=True,
            )
        if args.json:
            print(json.dumps(results, indent=2))
    finally:
        server.terminate()
        if os.path.exists(port_file):
            os.remove(port_file)


if __name__ == "__main__":
    main()
