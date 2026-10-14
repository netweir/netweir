"""Crawls that use Chrome: requests that ask for it, and a site that keeps
blocking until a browser passes its challenge. Skipped without Chrome,
unless NETWEIR_REQUIRE_CHROME is set."""

import asyncio
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

RENDERED = b"""<!DOCTYPE html><ul></ul><script>
for (const name of ['tea', 'cake']) document.querySelector('ul').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
</script>"""

CHALLENGE = b"""<title>Just a moment...</title><script>
setTimeout(() => { document.cookie = 'pass=1; path=/'; location.reload() }, 200)
</script>"""


class Handler(BaseHTTPRequestHandler):
    hits: list[tuple[str, str]] = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        Handler.hits.append((self.path, self.headers.get("User-Agent", "")))
        if self.path.startswith("/guarded") and "pass=1" not in self.headers.get("Cookie", ""):
            self.send_response(403)
            self.send_header("cf-mitigated", "challenge")
            body = CHALLENGE
        else:
            self.send_response(200)
            body = RENDERED
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
        try:
            asyncio.run(_probe())
        except netweir.BrowserError as e:
            pytest.skip(str(e))
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


async def _probe():
    async with netweir.browser():
        pass


def settings(**kw):
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        retries=1,
        backoff_base=0.01,
        backoff_max=0.05,
        **kw,
    )


def test_a_browser_request_reaches_its_callback_rendered_with_the_page_open(base):
    seen = {}

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/", browser=True)
            yield netweir.Request(base + "/plain")

        async def parse(self, page):
            seen[page.url] = (
                page.css("li::text").getall(),
                page.browser is not None
                and await page.browser.evaluate("document.querySelectorAll('li').length"),
            )
            self.kept = getattr(self, "kept", []) + [page.browser]

    spider = Shop()
    spider.settings = settings()
    stats = spider.run()
    assert seen[base + "/"] == (["tea", "cake"], 2)
    assert seen[base + "/plain"] == ([], False), "only the request that asked used Chrome"
    assert stats["browser_fetches"] == 1
    assert all(p is None or p.closed for p in spider.kept), "pages close after the callback"


def test_pages_fetched_in_chrome_are_left_out_of_the_warc_and_har_files(base, tmp_path):
    from warcio.archiveiterator import ArchiveIterator

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/rendered", browser=True)
            yield netweir.Request(base + "/plain")

        def parse(self, page):
            pass

    spider = Shop()
    spider.settings = settings(warc=str(tmp_path / "c.warc"), har=str(tmp_path / "c.har"))
    assert spider.run()["browser_fetches"] == 1
    with open(tmp_path / "c.warc", "rb") as f:
        uris = [
            r.rec_headers.get_header("WARC-Target-URI")
            for r in ArchiveIterator(f)
            if r.rec_type == "response"
        ]
    assert uris == [base + "/plain"]
    with open(tmp_path / "c.har", encoding="utf-8") as f:
        entries = json.load(f)["log"]["entries"]
    assert [e["request"]["url"] for e in entries] == [base + "/plain"]


def test_on_block_gets_through_in_chrome_and_carries_on_over_http(base):
    names = []

    class Guarded(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/guarded/1", callback=self.first)

        def first(self, page):
            names.extend(page.css("li::text").getall())
            yield page.follow("/guarded/2", callback=self.second)

        def second(self, page):
            assert page.browser is None, "fetched over HTTP with Chrome's cookie"
            names.extend(page.css("li::text").getall())

    spider = Guarded()
    spider.settings = settings(browser="on_block")
    stats = spider.run()
    # The second page came over HTTP, so its script never ran.
    assert names == ["tea", "cake"]
    assert (stats["browser_fetches"], stats["browser_unblocked"]) == (1, 1)
    assert stats["blocked"] == 2  # the first request's two tries over HTTP


def test_browser_settings_are_checked():
    with pytest.raises(ValueError, match="browser must be"):
        netweir.Settings(browser="sometimes")
    with pytest.raises(ValueError, match="browser_pages"):
        netweir.Settings(browser_pages=0)


def test_a_resumed_crawl_still_fetches_browser_requests_in_chrome(base, tmp_path):
    class Stop(Exception):
        pass

    runs = []

    class Once(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/again", browser=True)

        def parse(self, page):
            runs.append(page.browser is not None)
            if len(runs) == 1:
                raise Stop

    spider = Once()
    spider.settings = settings(checkpoint=str(tmp_path / "state"), fail_fast=True)
    with pytest.raises(Stop):
        spider.run()
    spider = Once()
    spider.settings = settings(checkpoint=str(tmp_path / "state"))
    spider.run()
    assert runs == [True, True]
