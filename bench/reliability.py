"""What happens to a crawl when things go wrong: Netweir against Scrapy and a
self-hosted Firecrawl, on the public shop at webscraper.io/test-sites.

    uv run python bench/reliability.py speed
    uv run python bench/reliability.py browser
    uv run python bench/reliability.py stop      # a clean stop, then a resume
    uv run python bench/reliability.py kill      # kill -9, then a resume
    uv run python bench/reliability.py throttle --rate 5
    uv run python bench/reliability.py selectors # Netweir, Scrapling, Scrapy

The last two serve their own pages; the rest use the public shop, which has 179 pages and 147 product pages. It is someone else's site, kept
for practice: stay on /test-sites/e-commerce/, which its robots.txt allows, and
don't run this in a loop.

Firecrawl has to be running on localhost:3002, from its own docker-compose.yaml
with USE_DB_AUTHENTICATION=false, and ALLOW_LOCAL_WEBHOOKS=true for the
throttle scenario only (see below). A stop or a kill
goes to its API container, which holds its workers. FIRECRAWL_DIR says where
its checkout is.

Every crawler runs with its own defaults, apart from what the scenario needs.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

STATIC = "https://webscraper.io/test-sites/e-commerce/static"
SCROLL = "https://webscraper.io/test-sites/e-commerce/scroll/computers/laptops"
PAGES = 179
PRODUCTS = 147
LAPTOPS = 117
FIRECRAWL = "http://localhost:3002/v2"
FIRECRAWL_DIR = os.environ.get("FIRECRAWL_DIR", "firecrawl")
PRODUCT = re.compile(r"/product/\d+$")


# --- the crawlers, each run in a process of its own ----------------------------


def run_netweir(output: str, checkpoint: str | None) -> None:
    import netweir

    class Shop(netweir.Spider):
        start_urls = [STATIC]
        settings = netweir.Settings(checkpoint=checkpoint) if checkpoint else netweir.Settings()

        def parse(self, page):
            yield {"url": page.url, "markdown": page.markdown()}
            for href in page.css("a::attr(href)").getall():
                if page.urljoin(href).startswith(STATIC):
                    yield page.follow(href)

    stats = Shop().run(output=output)
    print(json.dumps({"fetched": stats["fetched"]}))


def run_scrapy(output: str, checkpoint: str | None) -> None:
    import scrapy
    from scrapy.crawler import CrawlerProcess

    class Shop(scrapy.Spider):
        name = "shop"
        start_urls = [STATIC]

        def parse(self, response):
            yield {"url": response.url}
            for href in response.css("a::attr(href)").getall():
                if response.urljoin(href).startswith(STATIC):
                    yield response.follow(href)

    settings = {"FEEDS": {output: {"format": "jsonlines"}}, "LOG_LEVEL": "ERROR"}
    if checkpoint:
        settings["JOBDIR"] = checkpoint
    process = CrawlerProcess(settings)
    crawler = process.create_crawler(Shop)
    process.crawl(crawler)
    process.start()
    print(json.dumps({"fetched": crawler.stats.get_value("downloader/response_count")}))


def start_crawler(name: str, output: str, checkpoint: str | None) -> subprocess.Popen:
    args = [sys.executable, __file__, "--crawler", name, "--output", output]
    if checkpoint:
        args += ["--checkpoint", checkpoint]
    return subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)


def read_output(path: str) -> list[str]:
    urls = []
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                try:
                    urls.append(json.loads(line)["url"])
                except ValueError:
                    urls.append("<broken line>")
    return urls


def tally(urls: list[str]) -> dict:
    unique = {url for url in urls if url != "<broken line>"}
    return {
        "lines": len(urls),
        "pages": len(unique),
        "products": sum(1 for url in unique if PRODUCT.search(url)),
        "repeated": len(urls) - len(unique),
    }


def lines_in(path: str) -> int:
    if not os.path.exists(path):
        return 0
    with open(path, "rb") as f:
        return sum(1 for _ in f)


# --- Firecrawl -----------------------------------------------------------------


def firecrawl_post(path: str, body: dict) -> dict:
    request = urllib.request.Request(
        FIRECRAWL + path, json.dumps(body).encode(), {"content-type": "application/json"}
    )
    try:
        with urllib.request.urlopen(request, timeout=200) as response:
            return json.load(response)
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


def firecrawl_get(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=60) as response:
        return json.load(response)


def firecrawl_start() -> str:
    body = {
        "url": STATIC,
        "limit": 400,
        "maxDiscoveryDepth": 10,
        "sitemap": "skip",
        "scrapeOptions": {"formats": ["markdown"], "maxAge": 0},
    }
    return firecrawl_post("/crawl", body)["id"]


def firecrawl_status(job: str) -> dict:
    return firecrawl_get(f"{FIRECRAWL}/crawl/{job}")


def firecrawl_urls(status: dict) -> list[str]:
    pages = list(status.get("data", []))
    following = status.get("next")
    while following:
        more = firecrawl_get(following)
        pages += more.get("data", [])
        following = more.get("next")
    return [page["metadata"].get("sourceURL") or page["metadata"]["url"] for page in pages]


def firecrawl_wait(job: str, limit: float) -> dict:
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        try:
            status = firecrawl_status(job)
        except (urllib.error.URLError, OSError, ValueError):
            time.sleep(2)
            continue
        if status.get("status") != "scraping":
            return status
        time.sleep(1)
    return {"status": "still scraping when the wait ran out", **status}


def compose(*args: str) -> None:
    subprocess.run(["docker", "compose", *args], cwd=FIRECRAWL_DIR, check=True, capture_output=True)


def api_ready(limit: float = 120) -> None:
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        try:
            firecrawl_get("http://localhost:3002/")
            return
        except urllib.error.HTTPError:
            return
        except (urllib.error.URLError, OSError, ValueError):
            time.sleep(1)
    raise TimeoutError("Firecrawl's API didn't come back")


# --- the scenarios -------------------------------------------------------------


def speed(crawlers: list[str]) -> dict:
    results = {}
    for name in crawlers:
        with tempfile.TemporaryDirectory() as folder:
            started = time.monotonic()
            if name == "firecrawl":
                status = firecrawl_wait(firecrawl_start(), 400)
                urls = firecrawl_urls(status)
            else:
                output = os.path.join(folder, "out.jsonl")
                process = start_crawler(name, output, None)
                process.communicate(timeout=400)
                urls = read_output(output)
            results[name] = {"seconds": round(time.monotonic() - started, 1), **tally(urls)}
    return results


def interrupted(crawlers: list[str], how: signal.Signals, target: int = 60) -> dict:
    """Stops each crawler once about a third of the shop is done, then resumes it."""
    results = {}
    for name in crawlers:
        with tempfile.TemporaryDirectory() as folder:
            results[name] = (
                firecrawl_interrupted(how, target)
                if name == "firecrawl"
                else process_interrupted(name, folder, how, target)
            )
    return results


def process_interrupted(name: str, folder: str, how: signal.Signals, target: int) -> dict:
    output = os.path.join(folder, "out.jsonl")
    checkpoint = os.path.join(folder, "state")
    process = start_crawler(name, output, checkpoint)
    deadline = time.monotonic() + 120
    while lines_in(output) < target and process.poll() is None and time.monotonic() < deadline:
        time.sleep(0.02)
    process.send_signal(how)
    process.communicate(timeout=120)
    kept = tally(read_output(output))
    process = start_crawler(name, output, checkpoint)
    stdout, _ = process.communicate(timeout=400)
    last = stdout.strip().splitlines()[-1] if stdout.strip() else "{}"
    return {
        "kept_at_stop": kept["pages"],
        "fetched_on_resume": json.loads(last).get("fetched"),
        **tally(read_output(output)),
    }


def firecrawl_interrupted(how: signal.Signals, target: int) -> dict:
    job = firecrawl_start()
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        if firecrawl_status(job).get("completed", 0) >= target:
            break
        time.sleep(0.1)
    compose("kill" if how == signal.SIGKILL else "stop", "api")
    time.sleep(2)
    compose("up", "-d", "api")
    api_ready()
    status = firecrawl_wait(job, 400)
    return {"status": status.get("status"), **tally(firecrawl_urls(status))}


def browser_scroll() -> dict:
    """The laptop page adds products as you scroll. How many does each one read?"""
    results = {"laptops_on_the_page": LAPTOPS}
    code = (
        "import asyncio, json, netweir\n"
        "async def main():\n"
        "    async with netweir.browser() as b:\n"
        "        p = await b.new_page()\n"
        f"        await p.goto({SCROLL!r})\n"
        "        last, same = -1, 0\n"
        "        while same < 4:\n"
        "            await p.evaluate('window.scrollTo(0, document.body.scrollHeight)')\n"
        "            await asyncio.sleep(0.5)\n"
        "            n = await p.evaluate(\"document.querySelectorAll('.thumbnail .title').length\")\n"
        "            same = same + 1 if n == last else 0\n"
        "            last = n\n"
        "        print(last)\n"
        "asyncio.run(main())\n"
    )
    done = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=200)
    results["netweir"] = int(done.stdout.strip().splitlines()[-1])
    plain = firecrawl_post(
        "/scrape", {"url": SCROLL, "formats": ["markdown"], "maxAge": 0, "waitFor": 3000}
    )
    results["firecrawl_waiting_3s"] = len(
        set(re.findall(r"/product/\d+", plain.get("data", {}).get("markdown", "")))
    )
    scrolled = firecrawl_post(
        "/scrape",
        {
            "url": SCROLL,
            "formats": ["markdown"],
            "maxAge": 0,
            "actions": [{"type": "scroll", "direction": "down"}],
        },
    )
    results["firecrawl_scroll_action"] = scrolled.get("code") or "ran"
    return results


# --- on a local server: a site that rate-limits, and a site that's redesigned ---
# The public shop does neither, so these two serve their own pages. Firecrawl
# reaches the host as host.docker.internal, and refuses private addresses
# unless ALLOW_LOCAL_WEBHOOKS=true is set for both its api and playwright
# services, which these two need and the others don't.

LOCAL_PAGES = 200


class Limited:
    """Pages /shop/0 to /shop/199, each linking to two more, from a server
    that answers 429 with Retry-After: 1 past `rate` requests a second."""

    def __init__(self, rate: int):
        import http.server
        import threading

        self.rate = rate
        self.lock = threading.Lock()
        self.reset()
        limited = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self):
                found = re.fullmatch(r"/shop/(\d+)", self.path)
                status, body = limited.answer(int(found[1])) if found else (404, b"")
                self.send_response(status)
                if status == 429:
                    self.send_header("retry-after", "1")
                self.send_header("content-type", "text/html")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.ThreadingHTTPServer(("0.0.0.0", 0), Handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.port = server.server_port

    def reset(self) -> None:
        self.second, self.in_second, self.blocked_until = 0, 0, 0.0
        self.served, self.refused, self.too_soon = set(), 0, 0

    def answer(self, page: int) -> tuple[int, bytes]:
        now = time.monotonic()
        with self.lock:
            if int(now) != self.second:
                self.second, self.in_second = int(now), 0
            self.in_second += 1
            if now < self.blocked_until:
                self.too_soon += 1
            if self.in_second > self.rate:
                self.refused += 1
                self.blocked_until = now + 1
                return 429, b""
            self.served.add(page)
        links = "".join(
            f'<a href="/shop/{child}">more</a>'
            for child in (2 * page + 1, 2 * page + 2)
            if child < LOCAL_PAGES
        )
        return 200, (
            f"<html><title>{page}</title><body><main><h1>Item {page}</h1>"
            f"<p>Some text about item {page}, enough to count as content.</p>{links}"
            "</main></body></html>"
        ).encode()

    def result(self, started: float) -> dict:
        return {
            "pages": len(self.served),
            "seconds": round(time.monotonic() - started, 1),
            "429s": self.refused,
            "requests_during_retry_after": self.too_soon,
        }


THROTTLED_NETWEIR = """
import netweir, sys
class Shop(netweir.Spider):
    start_urls = [sys.argv[1]]
    def parse(self, page):
        yield {"url": page.url}
        for href in page.css("a::attr(href)").getall():
            yield page.follow(href)
Shop().run()
"""

THROTTLED_SCRAPY = """
import scrapy, sys
from scrapy.crawler import CrawlerProcess
class Shop(scrapy.Spider):
    name = "shop"
    start_urls = [sys.argv[1]]
    def parse(self, response):
        yield {"url": response.url}
        for href in response.css("a::attr(href)").getall():
            yield response.follow(href)
process = CrawlerProcess({"LOG_LEVEL": "ERROR"})
process.crawl(Shop)
process.start()
"""


def throttled(crawlers: list[str], rate: int) -> dict:
    site = Limited(rate)
    results = {"rate": rate}
    for name in crawlers:
        site.reset()
        started = time.monotonic()
        if name == "firecrawl":
            body = {
                "url": f"http://host.docker.internal:{site.port}/shop/0",
                "limit": 400,
                "maxDiscoveryDepth": 12,
                "sitemap": "skip",
                "crawlEntireDomain": True,
                "scrapeOptions": {"formats": ["markdown"], "maxAge": 0},
            }
            status = firecrawl_wait(firecrawl_post("/crawl", body)["id"], 400)
            results[name] = {**site.result(started), "returned": len(firecrawl_urls(status))}
        else:
            code = THROTTLED_NETWEIR if name == "netweir" else THROTTLED_SCRAPY
            url = f"http://127.0.0.1:{site.port}/shop/0"
            subprocess.run([sys.executable, "-c", code, url], capture_output=True, timeout=400)
            results[name] = site.result(started)
    return results


def before(n: int) -> str:
    return (
        f'<html><body><div class="product"><h1 class="title">Laptop {n}</h1>'
        f'<p class="was">Was ${n + 100}.00</p><p class="price">${n}.99</p>'
        '<p class="ship">Shipping $5.00</p></div></body></html>'
    )


def renamed(n: int) -> str:
    return (
        f'<html><body><div class="a1"><h1 class="x9">Laptop {n}</h1>'
        f'<p class="k2">Was ${n + 100}.00</p><p class="q7">${n}.99</p>'
        '<p class="m3">Shipping $5.00</p></div></body></html>'
    )


def rebuilt(n: int) -> str:
    return (
        f'<html><body><section class="a1"><div class="w"><h2 class="x9">Laptop {n}</h2></div>'
        f'<div class="w2"><span class="k2">Was ${n + 100}.00</span><span class="q7">${n}.99</span></div>'
        '<footer><span class="m3">Shipping $5.00</span></footer></section></body></html>'
    )


LEARN = range(100, 150)
AFTER = range(200, 250)


def score(read, pages) -> dict:
    """Prices read correctly, read wrong (the was price or the shipping), or not at all."""
    right = wrong = missing = 0
    for n in pages:
        value = read(n)
        if not value:
            missing += 1
        elif value.strip() == f"${n}.99":
            right += 1
        else:
            wrong += 1
    return {"right": right, "wrong": wrong, "missing": missing}


def selectors() -> dict:
    """Each one learns `.price` on 50 pages, then reads 50 pages after a
    redesign: one that renames every class, and one that also changes the
    tags and the layout around them. A 'was' price and a shipping price sit
    beside the price on every page, to catch a guess."""
    import http.server
    import threading

    import parsel
    from scrapling.parser import Selector

    import netweir

    design = {"page": before}

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            found = re.fullmatch(r"/p/(\d+)", self.path)
            body = design["page"](int(found[1])).encode() if found else b""
            self.send_response(200 if found else 404)
            self.send_header("content-type", "text/html")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    root = f"http://127.0.0.1:{server.server_port}"

    def crawl(checkpoint: str, pages) -> dict:
        read = {}

        class Shop(netweir.Spider):
            start_urls = [f"{root}/p/{n}" for n in pages]
            settings = netweir.Settings(checkpoint=checkpoint)

            def parse(self, page):
                read[int(page.url.rsplit("/", 1)[1])] = page.css(
                    ".price::text", track="price"
                ).get()

        Shop().run()
        return read

    results = {}
    for name, redesign in (("classes renamed", renamed), ("rebuilt", rebuilt)):
        with tempfile.TemporaryDirectory() as folder:
            design["page"] = before
            crawl(os.path.join(folder, "state"), LEARN)
            design["page"] = redesign
            read = crawl(os.path.join(folder, "state"), AFTER)
            storage = {
                "storage_file": os.path.join(folder, "scrapling.db"),
                "url": "http://shop.test/",
            }
            options = {"adaptive": True, "url": "http://shop.test/", "storage_args": storage}
            for n in LEARN:
                Selector(before(n), **options).css(".price", auto_save=True)

            def scrapling(n, redesign=redesign, options=options):
                found = Selector(redesign(n), **options).css(".price", adaptive=True)
                return found[0].text if found else None

            results[name] = {
                "netweir": score(read.get, AFTER),
                "scrapling": score(scrapling, AFTER),
                "scrapy": score(
                    lambda n, r=redesign: parsel.Selector(r(n)).css(".price::text").get(), AFTER
                ),
            }
    return results


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "scenario",
        nargs="?",
        choices=["speed", "browser", "stop", "kill", "throttle", "selectors"],
    )
    parser.add_argument("--rate", type=int, default=5)
    parser.add_argument("--crawler", help=argparse.SUPPRESS)
    parser.add_argument("--output", help=argparse.SUPPRESS)
    parser.add_argument("--checkpoint", help=argparse.SUPPRESS)
    parser.add_argument("--only", nargs="*", default=["netweir", "scrapy", "firecrawl"])
    args = parser.parse_args()
    if args.crawler:
        {"netweir": run_netweir, "scrapy": run_scrapy}[args.crawler](args.output, args.checkpoint)
    elif args.scenario == "speed":
        print(json.dumps(speed(args.only), indent=2))
    elif args.scenario == "browser":
        print(json.dumps(browser_scroll(), indent=2))
    elif args.scenario == "stop":
        print(json.dumps(interrupted(args.only, signal.SIGTERM), indent=2))
    elif args.scenario == "kill":
        print(json.dumps(interrupted(args.only, signal.SIGKILL), indent=2))
    elif args.scenario == "throttle":
        print(json.dumps(throttled(args.only, args.rate), indent=2))
    elif args.scenario == "selectors":
        print(json.dumps(selectors(), indent=2))


if __name__ == "__main__":
    main()
