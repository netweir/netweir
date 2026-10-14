import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Handler(BaseHTTPRequestHandler):
    """A site with no end: page n links to pages 2n and 2n+1."""

    protocol_version = "HTTP/1.1"
    delay = 0.0

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.startswith("/slow/"):
            time.sleep(0.3)
        prefix, _, n = self.path.rpartition("/")
        n = int(n) if n.isdigit() else 1
        body = f'<a href="{prefix}/{2 * n}">a</a><a href="{prefix}/{2 * n + 1}">b</a>'.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


def endless(base, path="/p/1", **limits):
    parsed = []

    class Endless(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), **limits})
        start_urls = [base + path]

        def parse(self, page):
            parsed.append(page.url)
            yield {"url": page.url}
            yield from page.follow_all(page.css("a::attr(href)"))

    return Endless, parsed


def test_max_items_stops_at_exactly_that_many(base, tmp_path):
    Endless, _ = endless(base, max_items=25)
    spider = Endless()
    out = tmp_path / "items.jsonl"
    stats = spider.run(str(out))
    assert stats["items"] == 25
    assert len(out.read_text().splitlines()) == 25
    assert spider.finish_reason == "max_items"


def test_max_pages_fetches_and_parses_exactly_that_many(base):
    Endless, parsed = endless(base, max_pages=20, concurrency=4)
    spider = Endless()
    stats = spider.run()
    assert spider.finish_reason == "max_pages"
    assert stats["fetched"] == 20
    assert len(parsed) == 20


def test_max_errors_stops_after_that_many_callback_errors(base):
    class Broken(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "max_errors": 3})
        start_urls = [f"{base}/e/1"]

        def parse(self, page):
            yield from page.follow_all(page.css("a::attr(href)"))
            raise RuntimeError("broken")

    spider = Broken()
    stats = spider.run()
    assert stats["callback_errors"] == 3
    assert spider.finish_reason == "max_errors"


def test_max_time_stops_a_crawl_that_would_run_forever(base):
    Endless, _ = endless(base, path="/slow/1", max_time=1.0, concurrency=2, per_domain=2)
    spider = Endless()
    started = time.monotonic()
    spider.run()
    assert time.monotonic() - started < 5
    assert spider.finish_reason == "max_time"


def test_a_crawl_that_runs_out_of_pages_finished(base):
    class One(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "max_items": 10})
        start_urls = [f"{base}/one/1"]

        def parse(self, page):
            yield {"url": page.url}

    spider = One()
    spider.run()
    assert spider.finish_reason == "finished"


def test_a_stopped_crawl_with_a_checkpoint_resumes_without_repeating_items(base, tmp_path):
    out = tmp_path / "items.jsonl"
    state = str(tmp_path / "state")
    Endless, _ = endless(base, path="/r/1", max_items=10, checkpoint=state)
    Endless().run(str(out))
    Endless().run(str(out))
    urls = [line for line in out.read_text().splitlines()]
    assert len(urls) == 20
    assert len(set(urls)) == 20


@pytest.mark.parametrize("field", ["max_items", "max_pages", "max_errors", "max_time"])
def test_a_limit_below_one_is_an_error(field):
    with pytest.raises(ValueError, match=field):
        netweir.Settings(**{field: 0})
