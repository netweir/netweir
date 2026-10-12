"""Middlewares, signals and pipeline lifecycles."""

import collections
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    served: collections.Counter = collections.Counter()
    # The server answers in threads, and += on a Counter isn't atomic
    # without the GIL.
    lock = threading.Lock()

    def log_message(self, *args):
        pass

    def do_GET(self):
        with Handler.lock:
            Handler.served[self.path] += 1
            n = Handler.served[self.path]
        if self.path == "/":
            body = b"<a href='/a'>a</a><a href='/b'>b</a>"
        elif self.path.startswith("/echo"):
            body = json.dumps({k.lower(): v for k, v in self.headers.items()}).encode()
        elif self.path.startswith("/ready"):
            # Not ready the first time it's asked for.
            body = b"ready" if n > 1 else b"not yet"
        else:
            body = f"<p>{self.path}</p>".encode()
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


def spider(base, start="/", follow=True, **attrs):
    """A spider that yields an item per page and follows links."""
    pages = []

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + start]

        def parse(self, page):
            pages.append((page.url, page.text))
            yield {"url": page.url}
            if follow:
                yield from page.follow_all(page.css("a::attr(href)"))

    for name, value in attrs.items():
        setattr(Site, name, value)
    return Site, pages


# --- process_request -------------------------------------------------------


def test_process_request_sees_every_request_and_can_change_it(base):
    seen = []

    class Headers:
        async def process_request(self, request, spider):
            seen.append(request.url)
            request.headers = {**(request.headers or {}), "X-Token": "42"}
            return request

    Site, pages = spider(base, start="/echo", middlewares=[Headers()])
    Site().run()
    assert seen == [base + "/echo"]
    assert json.loads(pages[0][1])["x-token"] == "42"

    seen.clear()
    Site, pages = spider(base, middlewares=[Headers()])
    Site().run()
    # Start requests and the requests a callback yields.
    assert sorted(seen) == [base + "/", base + "/a", base + "/b"]


def test_process_request_can_replace_or_drop(base):
    dropped = []

    class Rewrite:
        def process_request(self, request, spider):
            if request.url.endswith("/a"):
                raise netweir.IgnoreRequest
            if request.url.endswith("/b"):
                return netweir.Request(request.url + "2", callback=request.callback)
            return None  # as in Scrapy: carry on with it as it is

        def request_dropped(self, request, spider):
            dropped.append(request.url)

    Site, pages = spider(base, middlewares=[Rewrite()])
    stats = Site().run()
    assert sorted(url for url, _ in pages) == [base + "/", base + "/b2"]
    assert dropped == [base + "/a"]
    assert stats["requests_dropped"] == 1


def test_middlewares_run_in_scrapy_s_order(base):
    calls = []

    def recorder(name):
        class Recorder:
            def process_request(self, request, spider):
                calls.append(("request", name))
                return request

            def process_response(self, request, page, spider):
                calls.append(("response", name))
                return page

        return Recorder()

    Site, _ = spider(base, start="/x", middlewares=[recorder("first"), recorder("second")])
    Site().run()
    assert calls == [
        ("request", "first"),
        ("request", "second"),
        ("response", "second"),
        ("response", "first"),
    ]


# --- process_response ------------------------------------------------------


def test_process_response_can_send_a_request_instead(base):
    class Patient:
        def process_response(self, request, page, spider):
            if page.text == "not yet":
                return netweir.Request(page.url, callback=request.callback, dont_filter=True)
            return page

    Site, pages = spider(base, start="/ready/1", middlewares=[Patient()])
    Site().run()
    assert pages == [(base + "/ready/1", "ready")]


def test_process_response_can_drop_or_replace_a_page(base):
    class Choosy:
        async def process_response(self, request, page, spider):
            if page.url.endswith("/a"):
                raise netweir.IgnoreRequest
            if page.url.endswith("/b"):
                return netweir.get(base + "/echo-instead")
            return None  # unchanged

    Site, pages = spider(base, middlewares=[Choosy()])
    stats = Site().run()
    urls = sorted(url for url, _ in pages)
    assert urls == [base + "/", base + "/echo-instead"]
    assert stats["responses_dropped"] == 1


def test_the_callback_gets_the_page_the_middleware_returned(base):
    returned = []

    class Same:
        def process_response(self, request, page, spider):
            returned.append(page)
            return page

    got = []

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]
        middlewares = [Same()]

        def parse(self, page):
            got.append(page)

    Site().run()
    assert got[0] is returned[0]


# --- process_exception -----------------------------------------------------


def test_process_exception_can_queue_a_request_instead_of_the_errback(base):
    errors = []

    class Fallback:
        def process_exception(self, request, error, spider):
            errors.append(type(error).__name__)
            if request.errback is None:
                return netweir.Request(base + "/fallback")
            return None  # on to the errback

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        middlewares = [Fallback()]

        def start(self):
            return [
                netweir.Request("http://127.0.0.1:1/x"),
                netweir.Request("http://127.0.0.1:1/y", errback=self.lost),
            ]

        def parse(self, page):
            self.pages.append(page.url)

        def lost(self, request, error):
            self.failed.append(request.url)

    site = Site()
    site.pages, site.failed = [], []
    site.run()
    assert errors == ["FetchError", "FetchError"]
    assert site.pages == [base + "/fallback"]
    assert site.failed == ["http://127.0.0.1:1/y"]


# --- signals and lifecycles ------------------------------------------------


def test_signals_reach_middlewares_pipelines_and_the_spider(base):
    calls = []

    class Watcher:
        def spider_opened(self, spider):
            calls.append("watcher opened")

        async def spider_closed(self, spider, reason):
            calls.append(f"watcher closed {reason}")

        def item_scraped(self, item, spider):
            calls.append(f"scraped {item['url'].rsplit('/', 1)[1]}")

        def item_dropped(self, item, spider):
            calls.append(f"dropped {item['url'].rsplit('/', 1)[1]}")

    class Store:
        def open_spider(self, spider):
            calls.append("store open")

        async def process_item(self, item, spider):
            return None if item["url"].endswith("/a") else item

        def close_spider(self, spider):
            calls.append("store close")

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/"]
        middlewares = [Watcher()]
        pipelines = [Store()]

        def opened(self):
            calls.append("spider opened")

        def closed(self, reason):
            calls.append(f"spider closed {reason}")

        def parse(self, page):
            yield {"url": page.url}
            yield from page.follow_all(page.css("a::attr(href)"))

    Site().run()
    # Scrapy's order: pipelines open, then spider_opened, then the spider's
    # own; at the end, close_spider, then spider_closed, then closed().
    assert calls[:3] == ["store open", "watcher opened", "spider opened"]
    assert sorted(calls[3:6]) == ["dropped a", "scraped ", "scraped b"]
    assert calls[6:] == ["store close", "watcher closed finished", "spider closed finished"]


def test_spider_closed_gets_the_limit_that_stopped_it(base):
    reasons = []

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "max_items": 1})
        start_urls = [base + "/"]

        def parse(self, page):
            yield {"url": page.url}
            yield {"url": page.url + "#again"}

        def closed(self, reason):
            reasons.append(reason)

    Site().run()
    assert reasons == ["max_items"]


def test_a_failing_crawl_still_closes_with_error(base):
    calls = []

    class Store:
        def process_item(self, item, spider):
            raise RuntimeError("disk full")

        def close_spider(self, spider):
            calls.append("closed")

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "fail_fast": True})
        start_urls = [base + "/x"]
        pipelines = [Store()]

        def parse(self, page):
            yield {"url": page.url}

        def closed(self, reason):
            calls.append(reason)

    with pytest.raises(RuntimeError, match="disk full"):
        Site().run()
    assert calls == ["closed", "error"]


def test_spider_opened_failing_stops_the_crawl_before_any_fetch(base):
    Handler.served.clear()

    class Database:
        def open_spider(self, spider):
            raise ConnectionError("database is down")

    Site, pages = spider(base, start="/never", pipelines=[Database()])
    with pytest.raises(ConnectionError, match="database is down"):
        Site().run()
    assert Handler.served["/never"] == 0


def test_an_error_in_another_signal_is_logged_not_fatal(base, caplog):
    class Clumsy:
        def item_scraped(self, item, spider):
            raise ValueError("oops")

    Site, pages = spider(base, start="/x", middlewares=[Clumsy()])
    stats = Site().run()
    assert stats["items"] == 1
    assert "oops" in caplog.text


def test_drop_item_drops_as_in_scrapy(base):
    class DropItem(Exception):  # as scrapy.exceptions names it
        pass

    class ScrapyStyle:
        def process_item(self, item, spider):
            if item["url"].endswith("/a"):
                raise DropItem("no a")
            if item["url"].endswith("/b"):
                raise netweir.DropItem("no b")
            return item

    Site, _ = spider(base, pipelines=[ScrapyStyle()])
    stats = Site().run()
    assert (stats["items"], stats["items_dropped"], stats["callback_errors"]) == (1, 2, 0)


def test_plain_function_pipelines_still_work(base):
    Site, _ = spider(base, start="/x", follow=False, pipelines=[lambda item: {**item, "n": 1}])
    seen = []

    class Collect:
        def process_item(self, item, spider):
            seen.append(item)
            return item

    Site.pipelines = [*Site.pipelines, Collect()]
    Site().run()
    assert seen == [{"url": base + "/x", "n": 1}]


def test_middlewares_run_in_the_main_process_with_workers(base):
    import os

    import worker_spiders

    pids = []

    class Where:
        def process_response(self, request, page, spider):
            pids.append(os.getpid())
            return page

    site = worker_spiders.Linked()
    site.start_urls = [base + "/"]
    site.middlewares = [Where()]
    site.settings = netweir.Settings(**{**vars(FAST), "workers": 2})
    stats = site.run()
    assert stats["items"] == 3
    assert pids == [os.getpid()] * 3


# --- as in Scrapy ------------------------------------------------------------


def test_a_scrapy_style_middleware_that_returns_nothing_carries_on(base):
    class IgnoreRequest(Exception):  # as scrapy.exceptions names it
        pass

    class ScrapyStyle:
        def process_request(self, request, spider):
            if request.url.endswith("/a"):
                raise IgnoreRequest
            request.headers = {**(request.headers or {}), "X-Token": "7"}
            # No return: None, which in Scrapy means "carry on".

    Site, pages = spider(base, middlewares=[ScrapyStyle()])
    stats = Site().run()
    assert sorted(url for url, _ in pages) == [base + "/", base + "/b"]
    assert stats["requests_dropped"] == 1


def test_signals_take_scrapy_s_arguments_by_name(base):
    seen = []

    class ScrapyStyle:
        def spider_opened(self, spider):
            seen.append(("opened", type(spider).__name__))

        def item_scraped(self, item, response, spider):
            seen.append(("scraped", response.url))

        def item_dropped(self, item, response, exception, spider):
            seen.append(("dropped", type(exception).__name__))

        def spider_closed(self, spider, reason):
            seen.append(("closed", reason))

    def drop_a(item):
        if item["url"].endswith("/a"):
            raise netweir.DropItem("no a")
        return item

    Site, _ = spider(base, middlewares=[ScrapyStyle()], pipelines=[drop_a])
    Site().run()
    assert seen[0] == ("opened", "Site") and seen[-1] == ("closed", "finished")
    assert sorted(seen[1:-1]) == [
        ("dropped", "DropItem"),
        ("scraped", base + "/"),
        ("scraped", base + "/b"),
    ]


def test_drop_item_subclasses_drop(base):
    class Duplicate(netweir.DropItem):
        pass

    def no_a(item):
        if item["url"].endswith("/a"):
            raise Duplicate("seen it")
        return item

    Site, _ = spider(base, pipelines=[no_a])
    stats = Site().run()
    assert (stats["items_dropped"], stats["callback_errors"]) == (1, 0)


# --- depth and meta ----------------------------------------------------------


def test_a_replaced_page_keeps_the_request_s_depth_and_meta(base):
    seen = []

    class Refetch:
        def process_response(self, request, page, spider):
            return netweir.get(page.url)  # another Page for the same request

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "max_depth": 1})
        middlewares = [Refetch()]

        def start(self):
            return [netweir.Request(base + "/", meta={"from": "start"})]

        def parse(self, page):
            seen.append((page.url, page.depth, page.meta.get("from")))
            for href in page.css("a::attr(href)").getall():
                yield page.follow(href, meta={"from": page.url})

    Site().run()
    assert sorted(seen) == [
        (base + "/", 0, "start"),
        (base + "/a", 1, base + "/"),
        (base + "/b", 1, base + "/"),
    ]


def test_a_request_instead_of_a_failed_one_keeps_its_depth(base):
    depths = []

    class Fallback:
        def process_exception(self, request, error, spider):
            return netweir.Request(base + "/fallback", callback=request.callback)

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        start_urls = [base + "/"]
        middlewares = [Fallback()]

        def parse(self, page):
            depths.append((page.url, page.depth))
            if page.url == base + "/":
                yield netweir.Request("http://127.0.0.1:1/broken")

    Site().run()
    assert depths == [(base + "/", 0), (base + "/fallback", 1)]


def test_ignore_request_in_process_exception_skips_the_errback(base):
    class Quiet:
        def process_exception(self, request, error, spider):
            raise netweir.IgnoreRequest

    failed = []

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        middlewares = [Quiet()]

        def start(self):
            return [netweir.Request("http://127.0.0.1:1/x", errback=self.lost)]

        def lost(self, request, error):
            failed.append(request.url)

    stats = Site().run()
    assert failed == [] and stats["requests_dropped"] == 1


# --- errors ------------------------------------------------------------------


def test_an_error_in_a_middleware_is_a_callback_error(base, caplog):
    class Broken:
        def process_response(self, request, page, spider):
            raise RuntimeError("middleware bug")

    Site, pages = spider(base, start="/x", middlewares=[Broken()])
    stats = Site().run()
    assert pages == [] and stats["callback_errors"] == 1
    assert "middleware bug" in caplog.text

    Site.settings = netweir.Settings(**{**vars(FAST), "fail_fast": True})
    with pytest.raises(RuntimeError, match="middleware bug"):
        Site().run()


def test_a_middleware_must_return_a_request_page_or_none(base, caplog):
    class Wrong:
        def process_request(self, request, spider):
            return "not a request"

    Site, pages = spider(base, start="/x", middlewares=[Wrong()])
    stats = Site().run()
    assert pages == [] and stats["callback_errors"] == 1
    assert "returned a str" in caplog.text


def test_open_spider_failing_leaves_the_output_file_alone(base, tmp_path):
    out = tmp_path / "items.jsonl"
    out.write_text('{"old": 1}\n')

    class Database:
        def open_spider(self, spider):
            raise ConnectionError("database is down")

    Site, _ = spider(base, start="/x", pipelines=[Database()])
    with pytest.raises(ConnectionError):
        Site().run(output=str(out))
    assert out.read_text() == '{"old": 1}\n'


# --- rules -------------------------------------------------------------------


def test_pages_rules_send_to_a_callback_go_through_middlewares(base):
    seen, parsed = [], []

    class Watch:
        def process_response(self, request, page, spider):
            seen.append(page.url)

    class Ruled(netweir.Spider):
        settings = FAST
        start_urls = [base + "/"]
        rules = [netweir.Follow("a", callback="page")]
        middlewares = [Watch()]

        def page(self, page):
            parsed.append(page.url)

    Ruled().run()
    assert sorted(parsed) == [base + "/a", base + "/b"]
    # The start page is the rules' alone, read in Rust: it never comes to
    # Python, so the middleware sees only the pages sent to the callback.
    assert sorted(seen) == [base + "/a", base + "/b"]


def test_a_handler_takes_only_the_arguments_it_names(base, capsys):
    class Counter:  # as docs/crawling.md writes it
        def spider_opened(self):
            self.items = 0

        def item_scraped(self, item):
            self.items += 1

        def spider_closed(self, reason):
            print(f"{self.items} items, {reason}")

    Site, _ = spider(base, middlewares=[Counter()])
    Site().run()
    assert capsys.readouterr().out == "3 items, finished\n"


# --- second review -----------------------------------------------------------


def test_a_failed_open_on_resume_doesn_t_record_a_half_written_tail(base, tmp_path):
    out = tmp_path / "items.jsonl"
    checkpoint = str(tmp_path / "state")
    Site, _ = spider(base, follow=False)
    Site.settings = netweir.Settings(**{**vars(FAST), "checkpoint": checkpoint})
    Site().run(output=str(out))
    kept = out.read_text()
    with out.open("a") as f:
        f.write('{"half":')  # a run that died mid-line

    class Database:
        def open_spider(self, spider):
            raise ConnectionError("database is down")

    Site.pipelines = [Database()]
    with pytest.raises(ConnectionError):
        Site().run(output=str(out))
    Site.pipelines = []
    Site().run(output=str(out))
    assert out.read_text() == kept


def test_close_spider_runs_when_spider_opened_fails_on_the_same_object(base):
    calls = []

    class Store:
        def open_spider(self, spider):
            calls.append("open")

        def spider_opened(self, spider):
            raise RuntimeError("no")

        def close_spider(self, spider):
            calls.append("close")

    Site, _ = spider(base, start="/x", pipelines=[Store()])
    with pytest.raises(RuntimeError):
        Site().run()
    assert calls == ["open", "close"]


def test_an_interrupted_crawl_closes_with_interrupted(base):
    reasons = []

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]

        def parse(self, page):
            raise KeyboardInterrupt

        def closed(self, reason):
            reasons.append(reason)

    with pytest.raises(KeyboardInterrupt):
        Site().run()
    assert reasons == ["interrupted"]


def test_middleware_errors_count_toward_max_errors(base):
    class Broken:
        def process_response(self, request, page, spider):
            raise RuntimeError("broken")

    Site, _ = spider(base, middlewares=[Broken()])
    Site.settings = netweir.Settings(**{**vars(FAST), "max_errors": 1})
    site = Site()
    site.run()
    assert site.finish_reason == "max_errors"


def test_workers_with_pages_dropped_and_replaced(base):
    import worker_spiders

    class Choosy:
        def process_response(self, request, page, spider):
            if page.url.endswith("/a"):
                raise netweir.IgnoreRequest
            if page.url.endswith("/b"):
                return netweir.get(base + "/b-instead")

    site = worker_spiders.Linked()
    site.start_urls = [base + "/"]
    site.middlewares = [Choosy()]
    site.settings = netweir.Settings(**{**vars(FAST), "workers": 2})
    items = []
    site.pipelines = [lambda item: items.append(item["url"]) or item]
    stats = site.run()
    assert sorted(items) == [base + "/", base + "/b-instead"]
    assert stats["responses_dropped"] == 1


def test_closed_finds_the_output_file_complete(base, tmp_path):
    out = tmp_path / "items.jsonl"
    lines = []

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/"]

        def parse(self, page):
            yield {"url": page.url}
            yield from page.follow_all(page.css("a::attr(href)"))

        def closed(self, reason):
            lines.extend(out.read_text().splitlines())

    Site().run(output=str(out))
    assert len(lines) == 3


def test_scrapy_style_headers_assignment_works(base):
    class UserAgent:
        def process_request(self, request, spider):
            request.headers["User-Agent"] = "ported/1.0"

    Site, pages = spider(base, start="/echo", middlewares=[UserAgent()])
    Site().run()
    assert json.loads(pages[0][1])["user-agent"] == "ported/1.0"


def test_positional_only_handlers_and_scrapy_conveniences(base, caplog):
    seen = []

    class Watcher:
        def item_scraped(self, item, /, spider):
            seen.append(item["url"])

    class Retry:
        def process_response(self, request, page, spider):
            if page.text == "not yet":
                spider.logger.info("not ready: %s", page.url)
                return request.replace(dont_filter=True)

    Site, pages = spider(base, start="/ready/2", middlewares=[Watcher(), Retry()])
    caplog.set_level("INFO")
    Site().run()
    assert seen == [base + "/ready/2"]
    assert pages == [(base + "/ready/2", "ready")]
    assert any(r.name == "netweir.Site" and "not ready" in r.getMessage() for r in caplog.records)
