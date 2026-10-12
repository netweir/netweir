"""max_memory and memory_warning: stopping a crawl that uses too much memory."""

import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
from netweir import _memory

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)
MiB = 1024 * 1024


class Handler(BaseHTTPRequestHandler):
    """A site with no end: page n links to pages 2n and 2n+1, slowly."""

    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/slow":
            time.sleep(5)
        time.sleep(0.02)
        tail = self.path.rsplit("/", 1)[1]
        n = int(tail) if tail.isdigit() else 1
        body = f'<a href="/{2 * n}">a</a><a href="/{2 * n + 1}">b</a>'.encode()
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


def endless(base, **settings):
    class Endless(netweir.Spider):
        start_urls = [base + "/1"]

        def parse(self, page):
            yield {"url": page.url}
            yield from page.follow_all(page.css("a::attr(href)"))

    Endless.settings = netweir.Settings(**{**vars(FAST), "max_time": 10, **settings})
    return Endless


def test_peak_memory_can_be_read():
    used = _memory.peak()
    assert used is not None and 5 * MiB < used < 64 * 1024 * MiB


def test_max_memory_stops_the_crawl(base, monkeypatch):
    monkeypatch.setattr(_memory, "peak", lambda: 900 * MiB)
    spider = endless(base, max_memory=800)()
    started = time.monotonic()
    stats = spider.run()
    assert spider.finish_reason == "max_memory"
    assert stats["fetched"] == 0  # stopped at the first look, before fetching
    assert time.monotonic() - started < 5


def test_memory_warning_is_said_once(base, monkeypatch, caplog):
    monkeypatch.setattr(_memory, "peak", lambda: 600 * MiB)
    spider = endless(base, memory_warning=500, max_items=30)()
    spider.run()
    assert spider.finish_reason == "max_items"
    warnings = [r for r in caplog.records if "memory_warning is 500" in r.getMessage()]
    assert len(warnings) == 1 and "600 MiB" in warnings[0].getMessage()


def test_memory_is_read_at_most_once_a_second(base, monkeypatch):
    reads = []

    def peak():
        reads.append(time.monotonic())
        return 10 * MiB

    monkeypatch.setattr(_memory, "peak", peak)
    spider = endless(base, max_memory=10_000, max_time=1.5)()
    stats = spider.run()
    assert stats["fetched"] > 10
    assert 1 <= len(reads) <= 2


def test_where_memory_can_t_be_read_the_limit_says_so(base, monkeypatch, caplog):
    monkeypatch.setattr(_memory, "peak", lambda: None)
    spider = endless(base, max_memory=1, max_items=5)()
    spider.run()
    assert spider.finish_reason == "max_items"
    assert "can't read memory use" in caplog.text


def test_workers_memory_counts_too(base, monkeypatch):
    import worker_spiders

    # This process says it has used 10 MiB; the workers, started afresh,
    # report what they really use, which is more than the 2 MiB left.
    monkeypatch.setattr(_memory, "peak", lambda: 10 * MiB)

    def crawl(**settings):
        spider = worker_spiders.Linked()
        spider.start_urls = [base + "/1"]
        spider.settings = netweir.Settings(**{**vars(FAST), "max_memory": 12, **settings})
        spider.run()
        return spider.finish_reason

    assert crawl(max_time=1.5) == "max_time"  # alone, under the limit
    assert crawl(workers=2, max_time=10) == "max_memory"


def _child_peak(out):
    out.put(_memory.peak())


def test_a_worker_reports_its_own_peak_not_its_parent_s():
    import multiprocessing

    # A parent that has used far more than a fresh process does: a child
    # started as the workers are must not report the parent's peak.
    ballast = bytearray(300 * MiB)
    ballast[::4096] = b"x" * len(ballast[::4096])
    parent = _memory.peak()
    queue = multiprocessing.get_context("spawn").Queue()
    child = multiprocessing.get_context("spawn").Process(target=_child_peak, args=(queue,))
    child.start()
    child_peak = queue.get(timeout=60)
    child.join(timeout=60)
    del ballast
    assert parent > 300 * MiB
    assert child_peak < parent - 200 * MiB


def test_slow_pages_don_t_hide_the_memory_limit(base, monkeypatch):
    # Memory goes over the limit while the crawl waits on a page that
    # takes seconds: it's still seen within about a second.
    readings = iter([10 * MiB, 10 * MiB])
    monkeypatch.setattr(_memory, "peak", lambda: next(readings, 900 * MiB))

    class Slow(netweir.Spider):
        start_urls = [base + "/1"]
        settings = netweir.Settings(**{**vars(FAST), "max_memory": 100})

        def parse(self, page):
            yield netweir.Request(base + "/slow")

    started = time.monotonic()
    spider = Slow()
    spider.run()
    assert spider.finish_reason == "max_memory"
    assert time.monotonic() - started < 6


@pytest.mark.parametrize("name", ["max_memory", "memory_warning"])
def test_limits_must_be_positive(name):
    with pytest.raises(ValueError, match=name):
        netweir.Settings(**{name: 0})
