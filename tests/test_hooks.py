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


def test_a_failed_open_on_resume_doesn_t_record_a_half_written_tail(base, tmp_path, monkeypatch):
    out = tmp_path / "items.jsonl"
    checkpoint = str(tmp_path / "state")
    Site, _ = spider(base, follow=False)
    Site.settings = netweir.Settings(**{**vars(FAST), "checkpoint": checkpoint})
    # The run dies once its items are on record, before it's wound up: its
    # file isn't on record as closed.
    with monkeypatch.context() as m:
        m.setattr(netweir._crawl._Run, "finish", lambda self, closed=True: self.engine.close())
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


# --- a response from process_request or process_exception ---------------------


def test_process_request_can_answer_with_a_page_and_skip_the_fetch(base):
    Handler.served.clear()
    stored = netweir.get(base + "/stored")
    calls = []

    class Store:
        def process_request(self, request, spider):
            calls.append(("request", "store"))
            if request.url.endswith("/never"):
                return stored

        def process_response(self, request, page, spider):
            calls.append(("response", "store"))

    class After:
        def process_request(self, request, spider):
            calls.append(("request", "after"))

        def process_response(self, request, page, spider):
            calls.append(("response", "after"))

    class Signals:
        def response_downloaded(self, response):
            calls.append(("downloaded", response.url))

        def response_received(self, response, request):
            calls.append(("received", response.url, request.url))

    got = []

    class Site(netweir.Spider):
        settings = FAST
        middlewares = [Store(), After(), Signals()]

        def start(self):
            return [netweir.Request(base + "/never", meta={"m": 1})]

        def parse(self, page):
            got.append((page.url, page.text, page.meta, page.depth, page.request.url))

    Site().run()
    assert Handler.served["/never"] == 0
    assert got == [(base + "/stored", "<p>/stored</p>", {"m": 1}, 0, base + "/never")]
    # As in Scrapy: the middlewares after the one that answered don't see
    # the request, but every middleware's process_response sees the page,
    # and only response_received is sent, as it was never downloaded.
    assert calls == [
        ("request", "store"),
        ("response", "after"),
        ("response", "store"),
        ("received", base + "/stored", base + "/never"),
    ]


def test_a_page_from_process_request_is_still_screened_by_the_engine(base):
    stored = netweir.get(base + "/x")
    parsed = []

    class Store:
        def process_request(self, request, spider):
            return stored  # every request, as a store of pages would

    class Site(netweir.Spider):
        # max_pages and max_time: broken screening fails, rather than hangs.
        settings = netweir.Settings(
            **{**vars(FAST), "max_depth": 2, "max_pages": 50, "max_time": 20}
        )
        start_urls = [base + "/loop"]
        middlewares = [Store()]

        def parse(self, page):
            parsed.append(page.request.url)
            # Round in circles, and on and on.
            yield netweir.Request(base + "/loop")
            yield page.follow(f"/deeper/{page.depth}")

    Site().run()
    assert sorted(parsed) == [base + "/deeper/0", base + "/deeper/1", base + "/loop"]


def test_process_exception_can_answer_with_a_page(base):
    order = []

    class Offline:
        def process_exception(self, request, error, spider):
            order.append("exception")
            return netweir.get(base + "/offline")

        def process_response(self, request, page, spider):
            order.append(f"response {page.url.rsplit('/', 1)[1]}")

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        middlewares = [Offline()]

        def start(self):
            return [netweir.Request("http://127.0.0.1:1/x", errback=self.lost)]

        def parse(self, page):
            order.append(f"parse {page.url.rsplit('/', 1)[1]} for {page.request.url}")

        def lost(self, request, error):
            order.append("errback")

    Site().run()
    assert order == ["exception", "response offline", "parse offline for http://127.0.0.1:1/x"]


# --- rescheduling and IgnoreRequest, as in Scrapy ----------------------------


def test_a_request_from_process_request_starts_the_chain_again(base):
    calls = []

    def named(name, moves=False):
        class Middleware:
            def process_request(self, request, spider):
                calls.append((name, request.url.rsplit("/", 1)[1]))
                if moves and request.url.endswith("/old"):
                    return request.replace(url=base + "/new")

        return Middleware()

    scheduled = []

    class Watch:
        def request_scheduled(self, request):
            scheduled.append(request.url.rsplit("/", 1)[1])

    Site, pages = spider(
        base, start="/old", follow=False, middlewares=[named("a"), named("b", True), Watch()]
    )
    Site().run()
    assert calls == [("a", "old"), ("b", "old"), ("a", "new"), ("b", "new")]
    assert scheduled == ["old", "new"]
    assert [url for url, _ in pages] == [base + "/new"]


def test_a_middleware_that_always_gives_a_new_request_is_stopped(base, caplog):
    class Restless:
        def process_request(self, request, spider):
            return request.replace(meta={"n": request.meta.get("n", 0) + 1})

    Site, pages = spider(base, start="/x", middlewares=[Restless()])
    stats = Site().run()
    assert pages == [] and stats["callback_errors"] == 1
    assert "100" in caplog.text


def test_ignore_request_from_process_request_goes_to_process_exception_then_errback(base):
    seen = []

    class Gate:
        def process_request(self, request, spider):
            if request.url.endswith("/a"):
                raise netweir.IgnoreRequest("not /a")

        def process_exception(self, request, error, spider):
            seen.append(("exception", type(error).__name__, str(error)))

        def request_dropped(self, request):
            seen.append(("dropped", request.url.rsplit("/", 1)[1]))

        def item_scraped(self, item, response):
            if response is not None:  # None for the errback's
                seen.append(("scraped", response.url.rsplit("/", 1)[1]))

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/"]
        middlewares = [Gate()]

        def parse(self, page):
            yield netweir.Request(base + "/a", errback=self.lost)
            # Still this page's item: the errback waits for this callback.
            yield {"url": page.url}

        def lost(self, request, error):
            seen.append(("errback", request.url.rsplit("/", 1)[1], type(error).__name__))
            yield {"lost": request.url}

    stats = Site().run()
    assert seen == [
        ("scraped", ""),
        ("exception", "IgnoreRequest", "not /a"),
        ("dropped", "a"),
        ("errback", "a", "IgnoreRequest"),
    ]
    assert stats["requests_dropped"] == 1 and stats["items"] == 2


def test_ignore_request_from_process_response_goes_to_the_errback_only(base):
    seen = []

    class Gate:
        def process_response(self, request, page, spider):
            raise netweir.IgnoreRequest("no thanks")

        def process_exception(self, request, error, spider):
            seen.append("process_exception")

    class Site(netweir.Spider):
        settings = FAST
        middlewares = [Gate()]

        def start(self):
            return [
                netweir.Request(base + "/a", errback=self.lost),
                netweir.Request(base + "/b"),
            ]

        def parse(self, page):
            seen.append("parse")

        def lost(self, request, error):
            seen.append(f"errback {error}")

    stats = Site().run()
    assert seen == ["errback no thanks"]
    assert stats["responses_dropped"] == 2


def test_ignore_request_from_process_exception_goes_to_the_errback(base):
    class Quiet:
        def process_exception(self, request, error, spider):
            raise netweir.IgnoreRequest("given up")

    class Later:
        def process_exception(self, request, error, spider):
            raise AssertionError("never asked: Quiet, nearer the download, raised first")

    failed = []

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        middlewares = [Later(), Quiet()]

        def start(self):
            return [netweir.Request("http://127.0.0.1:1/x", errback=self.lost)]

        def lost(self, request, error):
            failed.append(str(error))

    stats = Site().run()
    assert failed == ["given up"] and stats["requests_dropped"] == 1
    assert stats["callback_errors"] == 0


# --- spider middlewares --------------------------------------------------------


def test_spider_middlewares_run_in_scrapy_s_order(base):
    inputs, via = [], []

    def named(name):
        class Middleware:
            async def process_start(self, start):
                async for request in start:
                    request.meta["via"] = [*request.meta.get("via", []), name]
                    yield request

            def process_spider_input(self, response, spider):
                inputs.append(name)

            async def process_spider_output(self, response, result, spider):
                async for out in result:
                    out["via"] = [*out.get("via", []), name]
                    yield out

        return Middleware()

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]
        spider_middlewares = [named("engine"), named("spider")]

        def parse(self, page):
            via.append(page.meta["via"])
            yield {"url": page.url}

    items = []
    Site.pipelines = [lambda item: items.append(item) or item]
    Site().run()
    # The first is nearest the engine: input in order, the rest in reverse,
    # so the one nearest the spider wraps its start and its output first.
    assert inputs == ["engine", "spider"]
    assert via == [["spider", "engine"]]
    assert items[0]["via"] == ["spider", "engine"]


def test_process_spider_output_sync_and_async(base):
    given = []

    class Sync:
        def process_spider_output(self, response, result, spider):
            given.append(type(result).__name__)
            for out in result:
                if isinstance(out, dict):
                    out["sync"] = True
                yield out

    class Async:
        async def process_spider_output(self, response, result, spider):
            async for out in result:
                if isinstance(out, dict) and out["url"].endswith("/a"):
                    continue  # dropped on the way
                yield out

    class Universal:
        def process_spider_output(self, response, result, spider):
            raise AssertionError("the async one is used")

        async def process_spider_output_async(self, response, result, spider):
            async for out in result:
                if isinstance(out, dict):
                    out["universal"] = True
                yield out

    items = []
    Site, _ = spider(
        base,
        spider_middlewares=[Sync(), Async(), Universal()],
        pipelines=[lambda item: items.append(item) or item],
    )
    Site().run()
    assert sorted(i["url"] for i in items) == [base + "/", base + "/b"]
    assert all(i["sync"] and i["universal"] for i in items)
    assert set(given) == {"list"}


def test_process_spider_exception_can_recover(base):
    seen = []

    class Recover:
        def process_spider_exception(self, response, exception, spider):
            seen.append(("recover", str(exception)))
            return [{"recovered": response.url}]

    class Outer:
        def process_spider_output(self, response, result, spider):
            for out in result:
                seen.append(("outer", sorted(out)))
                yield out

    class Inner:
        def process_spider_output(self, response, result, spider):
            for out in result:
                seen.append(("inner", sorted(out)))
                yield out

    class Watch:
        def spider_error(self, failure):
            seen.append("spider_error")

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]
        # Outer nearest the engine, Inner nearest the spider.
        spider_middlewares = [Outer(), Recover(), Inner()]
        middlewares = [Watch()]

        def parse(self, page):
            yield {"before": 1}
            raise ValueError("broke")

    stats = Site().run()
    assert stats["items"] == 2 and stats["callback_errors"] == 0
    # The callback's error passes Inner (nearer the spider) by, and the
    # recovered output goes through only the middlewares nearer the engine.
    assert ("recover", "broke") in seen
    assert ("outer", ["recovered"]) in seen and ("inner", ["recovered"]) not in seen
    assert "spider_error" not in seen


def test_an_exception_no_spider_middleware_takes_is_a_callback_error(base, caplog):
    seen = []

    class Declines:
        async def process_spider_exception(self, response, exception, spider):
            seen.append(type(exception).__name__)

    class Watch:
        def spider_error(self, failure, response, spider):
            seen.append((failure.value.args[0], failure.check(ValueError), response.url))
            assert "broke" in failure.getTraceback() and failure.getErrorMessage() == "broke"

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]
        spider_middlewares = [Declines(), Declines()]
        middlewares = [Watch()]

        def parse(self, page):
            raise ValueError("broke")

    stats = Site().run()
    assert stats["callback_errors"] == 1
    assert seen == ["ValueError", "ValueError", ("broke", ValueError, base + "/x")]


def test_an_error_in_process_spider_output_goes_to_the_middlewares_nearer_the_engine(base):
    seen = []

    class Recover:
        def process_spider_exception(self, response, exception, spider):
            seen.append(str(exception))
            return []

    class Broken:
        def process_spider_output(self, response, result, spider):
            yield from result
            raise RuntimeError("output broke")

        def process_spider_exception(self, response, exception, spider):
            seen.append("never: its own output")

    Site, _ = spider(base, start="/x", follow=False, spider_middlewares=[Recover(), Broken()])
    stats = Site().run()
    assert seen == ["output broke"] and stats["callback_errors"] == 0


def test_process_spider_input_errors_go_to_the_errback_or_process_spider_exception(base):
    seen = []

    class Picky:
        def process_spider_input(self, response, spider):
            if response.url.endswith("/a") or response.url.endswith("/b"):
                raise ValueError(f"not {response.url.rsplit('/', 1)[1]}")

        def process_spider_output(self, response, result, spider):
            for out in result:
                seen.append(("output", out))
                yield out

        def process_spider_exception(self, response, exception, spider):
            seen.append(("exception", str(exception)))
            return []

    class Site(netweir.Spider):
        settings = FAST
        spider_middlewares = [Picky()]

        def start(self):
            return [
                netweir.Request(base + "/a", errback=self.lost),
                netweir.Request(base + "/b"),
            ]

        def parse(self, page):
            seen.append("parse")

        def lost(self, request, error):
            yield {"lost": str(error)}

    stats = Site().run()
    assert ("output", {"lost": "not a"}) in seen and ("exception", "not b") in seen
    assert "parse" not in seen and stats["items"] == 1


def test_process_start_and_process_start_requests(base):
    class Extra:
        async def process_start(self, start):
            async for request in start:
                yield request
            yield netweir.Request(base + "/extra")

    class Legacy:
        def process_start_requests(self, start_requests, spider):
            assert isinstance(start_requests, list)
            for request in start_requests:
                if not request.url.endswith("/skip"):
                    yield request

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/one", base + "/skip"]
        spider_middlewares = [Extra(), Legacy()]

        def parse(self, page):
            yield {"url": page.url}

    items = []
    Site.pipelines = [lambda item: items.append(item["url"]) or item]
    Site().run()
    assert sorted(items) == [base + "/extra", base + "/one"]


def test_spider_middlewares_with_workers(base):
    import os

    import worker_spiders

    pids = []

    class Mark:
        def process_spider_input(self, response, spider):
            pids.append(os.getpid())

        async def process_spider_output(self, response, result, spider):
            async for out in result:
                pids.append(os.getpid())
                if isinstance(out, dict):
                    out["marked"] = True
                yield out

    items = []
    site = worker_spiders.Linked()
    site.start_urls = [base + "/"]
    site.spider_middlewares = [Mark()]
    site.pipelines = [lambda item: items.append(item) or item]
    site.settings = netweir.Settings(**{**vars(FAST), "workers": 2})
    site.run()
    assert len(items) == 3 and all(i["marked"] for i in items)
    assert set(pids) == {os.getpid()}


# --- the other signals -----------------------------------------------------------


def test_engine_started_and_stopped_come_in_scrapy_s_order(base):
    calls = []

    class Watch:
        def engine_started(self):
            calls.append("engine_started")

        def spider_opened(self):
            calls.append("spider_opened")

        def request_scheduled(self, request):
            calls.append("request_scheduled")

        def spider_idle(self):
            calls.append("spider_idle")

        def spider_closed(self):
            calls.append("spider_closed")

        def engine_stopped(self):
            calls.append("engine_stopped")

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]
        middlewares = [Watch()]

        def opened(self):
            calls.append("opened")

        def closed(self, reason):
            calls.append("closed")

        def parse(self, page):
            pass

    Site().run()
    assert calls == [
        "spider_opened",
        "opened",
        "engine_started",
        "request_scheduled",
        "spider_idle",
        "spider_closed",
        "closed",
        "engine_stopped",
    ]


def test_spider_idle_can_schedule_more(base):
    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/first"]

        def opened(self):
            self.left = [base + "/second", base + "/third"]
            self.crawler.signals.connect(self.idle, signal=netweir.signals.spider_idle)

        def idle(self, spider):
            if self.left:
                self.crawler.engine.crawl(netweir.Request(self.left.pop(0)))

        def parse(self, page):
            yield {"url": page.url}

    items = []
    Site.pipelines = [lambda item: items.append(item["url"]) or item]
    site = Site()
    site.run()
    assert items == [base + "/first", base + "/second", base + "/third"]
    assert site.finish_reason == "finished"


def test_dont_close_spider_and_close_spider_from_spider_idle(base, monkeypatch):
    monkeypatch.setattr(netweir._crawl, "_IDLE_WAIT", 0.01)
    idles = []

    class Keep:
        def spider_idle(self, spider):
            idles.append(len(idles))
            if len(idles) < 3:
                raise netweir.DontCloseSpider
            raise netweir.CloseSpider("enough")

    Site, _ = spider(base, start="/x", middlewares=[Keep()])
    site = Site()
    site.run()
    assert idles == [0, 1, 2] and site.finish_reason == "enough"


def test_close_spider_from_a_callback(base):
    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/"]

        def parse(self, page):
            yield {"url": page.url}
            raise netweir.CloseSpider("seen enough")

    site = Site()
    stats = site.run()
    assert site.finish_reason == "seen enough"
    assert stats["items"] == 1 and stats["callback_errors"] == 0


def test_request_scheduled_can_drop_and_request_dropped_hears_the_engine(base):
    dropped = []

    class Watch:
        def request_scheduled(self, request, spider):
            if request.url.endswith("/a"):
                raise netweir.IgnoreRequest

        def request_dropped(self, request):
            dropped.append(request.url)

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/"]
        middlewares = [Watch()]

        def parse(self, page):
            yield {"url": page.url}
            yield page.follow("/a")
            yield page.follow("/")  # a duplicate: the engine refuses it

    stats = Site().run()
    assert stats["items"] == 1
    assert dropped == [base + "/"]


def test_download_signals(base):
    seen = []

    class Watch:
        def response_downloaded(self, response, request, spider):
            seen.append(("downloaded", request.url))

        def request_left_downloader(self, request, spider):
            seen.append(("left", request.url))

        def process_response(self, request, page, spider):
            seen.append(("process_response", request.url))

        def response_received(self, response, request, spider):
            seen.append(("received", response.url))

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        middlewares = [Watch()]

        def start(self):
            return [netweir.Request(base + "/x"), netweir.Request("http://127.0.0.1:1/y")]

        def parse(self, page):
            pass

    Site().run()
    assert sorted(seen) == sorted(
        [
            ("downloaded", base + "/x"),
            ("left", base + "/x"),
            ("process_response", base + "/x"),
            ("received", base + "/x"),
            ("left", "http://127.0.0.1:1/y"),
        ]
    )
    x = [s for s in seen if s[1] == base + "/x"]
    assert [s[0] for s in x] == ["downloaded", "left", "process_response", "received"]


def test_item_error(base):
    seen = []

    class Store:
        def process_item(self, item, spider):
            raise RuntimeError("disk full")

        def item_error(self, item, response, spider, failure):
            seen.append((item["url"], response.url, str(failure.value)))

        def spider_error(self, failure):
            seen.append("not a spider error")

    Site, _ = spider(base, start="/x", follow=False, pipelines=[Store()])
    stats = Site().run()
    assert seen == [(base + "/x", base + "/x", "disk full")]
    assert stats["callback_errors"] == 1


# --- from_crawler ---------------------------------------------------------------------


def test_from_crawler_builds_components_as_scrapy_does(base):
    seen = []

    class Tagger:
        def __init__(self, tag):
            self.tag = tag

        @classmethod
        def from_crawler(cls, crawler):
            assert crawler.settings.getbool("TAGGING_ENABLED")
            assert crawler.settings.concurrency == FAST.concurrency
            made = cls(crawler.settings.get("TAG", "none"))
            crawler.signals.connect(made.opened, signal=netweir.signals.spider_opened)
            crawler.signals.connect(made.scraped, signal="item_scraped")
            made.stats = crawler.stats
            seen.append(crawler.spider)
            return made

        def opened(self, spider):
            seen.append("opened")

        def scraped(self, item):
            self.stats.inc_value("tagger/items")

        def item_scraped(self, item):
            # Never: an object made by from_crawler hears what it connects.
            seen.append("by name")

        def process_item(self, item, spider):
            return {**item, "tag": self.tag}

    class Plain:
        def process_item(self, item, spider):
            seen.append("plain")
            return item

    class Off:
        @classmethod
        def from_crawler(cls, crawler):
            raise netweir.NotConfigured("not today")

    items = []

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]
        custom_settings = {"TAGGING_ENABLED": "True", "TAG": "blue"}
        pipelines = [Tagger, Plain, Off, lambda item: items.append(item) or item]

        def parse(self, page):
            yield {"url": page.url}

    site = Site()
    stats = site.run()
    assert items == [{"url": base + "/x", "tag": "blue"}]
    assert seen == [site, "opened", "plain"]
    assert stats["tagger/items"] == 1


def test_scrapy_s_own_signal_objects_and_exceptions_work(base, monkeypatch):
    scrapy = pytest.importorskip("scrapy")
    from scrapy import signals
    from scrapy.exceptions import DontCloseSpider, NotConfigured

    seen = []

    class Ported:
        @classmethod
        def from_crawler(cls, crawler):
            made = cls()
            crawler.signals.connect(made.idle, signal=signals.spider_idle)
            crawler.signals.connect(made.closed, signal=signals.spider_closed)
            return made

        def idle(self, spider):
            seen.append("idle")
            if len(seen) == 1:
                raise DontCloseSpider

        def closed(self, spider, reason):
            seen.append(reason)

    class Disabled:
        def __init__(self):
            raise NotConfigured

    assert scrapy
    monkeypatch.setattr(netweir._crawl, "_IDLE_WAIT", 0.01)
    Site, _ = spider(base, start="/x", middlewares=[Ported, Disabled])
    Site().run()
    assert seen == ["idle", "idle", "finished"]


def test_spider_error_for_an_errback_and_a_coroutine_callback_through_middlewares(base):
    seen = []

    class Watch:
        def spider_error(self, failure, response):
            seen.append((str(failure.value), response))

    class Upper:
        async def process_spider_output(self, response, result, spider):
            async for out in result:
                yield {k: v.upper() for k, v in out.items()}

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "retries": 0})
        middlewares = [Watch()]
        spider_middlewares = [Upper()]

        def start(self):
            return [
                netweir.Request(base + "/x"),
                netweir.Request("http://127.0.0.1:1/y", errback=self.lost),
            ]

        async def parse(self, page):
            return [{"text": page.text}]

        def lost(self, request, error):
            raise RuntimeError("errback broke")

    items = []
    Site.pipelines = [lambda item: items.append(item) or item]
    stats = Site().run()
    assert items == [{"text": "<P>/X</P>"}]
    assert seen == [("errback broke", None)] and stats["callback_errors"] == 1


def test_close_spider_from_a_signal_handler(base):
    class Enough:
        @classmethod
        def from_crawler(cls, crawler):
            made = cls()
            made.engine = crawler.engine
            crawler.signals.connect(made.scraped, signal=netweir.signals.item_scraped)
            return made

        def scraped(self, item, spider):
            self.engine.close_spider(spider, "one is enough")

    Site, _ = spider(base, middlewares=[Enough])
    site = Site()
    stats = site.run()
    assert site.finish_reason == "one is enough" and stats["items"] == 1


def test_a_rules_spider_s_answered_page_is_read_by_the_rules(base):
    stored = netweir.get(base + "/")  # links to /a and /b
    Handler.served.clear()

    class Store:
        def process_request(self, request, spider):
            return stored

    class Ruled(netweir.Spider):
        settings = FAST
        start_urls = [base + "/start"]
        rules = [netweir.Follow("a")]
        middlewares = [Store()]

    stats = Ruled().run()
    assert stats["callback_errors"] == 0 and stats["pages_served"] == 1
    # The rules found the served page's links, and Rust fetched them.
    assert Handler.served["/start"] == 0
    assert (Handler.served["/a"], Handler.served["/b"]) == (1, 1)


def test_a_from_crawler_that_fails_stops_the_crawl_before_any_fetch(base):
    Handler.served.clear()

    class Broken:
        @classmethod
        def from_crawler(cls, crawler):
            raise ConnectionError("no database")

    Site, _ = spider(base, start="/never-built", pipelines=[Broken])
    with pytest.raises(ConnectionError, match="no database"):
        Site().run()
    assert Handler.served["/never-built"] == 0


# --- second round ------------------------------------------------------------


def test_served_pages_count_toward_max_pages(base):
    served = []

    class Store:
        def process_request(self, request, spider):
            served.append(request.url)
            return netweir.get(base + "/x")

    class Endless(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "max_pages": 1, "max_time": 20})
        start_urls = [base + "/0"]
        middlewares = [Store()]

        def parse(self, page):
            n = int(page.request.url.rsplit("/", 1)[1])
            for i in range(3):
                yield netweir.Request(f"{base}/{n * 3 + i + 1}")

    site = Endless()
    stats = site.run()
    assert site.finish_reason == "max_pages"
    assert stats["pages_served"] == 1 and stats["fetched"] == 0


def test_served_pages_count_toward_max_pages_per_domain(base):
    class Store:
        def process_request(self, request, spider):
            return netweir.get(base + "/x")

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "max_pages_per_domain": 2})
        start_urls = [base + "/1", base + "/2", base + "/3"]
        middlewares = [Store()]

        def parse(self, page):
            yield {"url": page.request.url}

    stats = Site().run()
    assert (stats["items"], stats["pages_served"], stats["skipped_domain_full"]) == (2, 2, 1)


def test_close_spider_from_a_spider_idle_handler(base):
    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]

        def opened(self):
            self.crawler.signals.connect(self.idle, signal=netweir.signals.spider_idle)

        def idle(self, spider):
            self.crawler.engine.close_spider(spider, "nothing more today")

        def parse(self, page):
            pass

    site = Site()
    site.run()
    assert site.finish_reason == "nothing more today"
    assert site.crawler is None  # let go of once the crawl is over


def test_an_exception_no_middleware_takes_is_offered_to_each_once(base):
    asked = []

    def named(name):
        class Middleware:
            async def process_spider_output(self, response, result, spider):
                async for out in result:
                    yield out

            def process_spider_exception(self, response, exception, spider):
                asked.append(name)

        return Middleware()

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/x"]
        spider_middlewares = [named("A"), named("B")]

        def parse(self, page):
            yield {"url": page.url}
            raise ValueError("broke")

    stats = Site().run()
    # Nearest the spider first, and each once: not B, A, A.
    assert asked == ["B", "A"]
    assert stats["items"] == 1 and stats["callback_errors"] == 1


def test_close_spider_in_spider_opened_ends_the_crawl_before_it_starts(base, caplog):
    Handler.served.clear()
    calls = []

    class Gate:
        def spider_opened(self, spider):
            raise netweir.CloseSpider("bandwidth")

        def engine_started(self):
            calls.append("engine_started")

    class Other:
        def spider_opened(self):
            calls.append("other opened")

    caplog.set_level("INFO")
    Site, _ = spider(base, start="/never-opened", middlewares=[Gate(), Other()])
    site = Site()
    site.run()
    assert site.finish_reason == "bandwidth" and calls == ["other opened"]
    assert Handler.served["/never-opened"] == 0
    assert "stopping the crawl: bandwidth" in caplog.text
    assert "bandwidth reached" not in caplog.text


def test_netweir_s_counts_win_over_a_component_s(base):
    class Counts:
        @classmethod
        def from_crawler(cls, crawler):
            crawler.stats.set_value("items", 1000)
            crawler.stats.inc_value("counts/mine")
            return cls()

    Site, _ = spider(base, start="/x", follow=False, middlewares=[Counts])
    stats = Site().run()
    assert stats["items"] == 1 and stats["counts/mine"] == 1


@pytest.mark.parametrize("served", [False, True], ids=["fetched", "served"])
def test_max_pages_ends_a_served_crawl_as_it_ends_a_fetched_one(base, served):
    class Store:
        def process_request(self, request, spider):
            return netweir.get(request.url)

    parsed = []

    class Site(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "max_pages": 2, "max_time": 20})
        start_urls = [base + "/start"]
        middlewares = [Store()] if served else []

        def parse(self, page):
            parsed.append(page.request.url.rsplit("/", 1)[1])
            if page.request.url.endswith("/start"):
                yield netweir.Request(base + "/s1")
                yield netweir.Request(base + "/s2")
            # Still delivered: the limit doesn't cut a callback short.
            yield {"url": page.request.url}

    site = Site()
    stats = site.run()
    assert parsed == ["start", "s1"]
    assert stats["items"] == 2 and site.finish_reason == "max_pages"
    assert stats["pages_served" if served else "fetched"] == 2


def test_max_error_rate_counts_served_pages(base):
    class Store:
        def process_request(self, request, spider):
            return netweir.get(base + "/x")

    class Site(netweir.Spider):
        settings = FAST
        start_urls = [base + "/1", base + "/2"]
        middlewares = [Store()]
        checks = [netweir.Checks(max_error_rate=0.4)]

        def parse(self, page):
            if page.request.url.endswith("/1"):
                raise ValueError("broke")

    site = Site()
    site.run()
    assert site.check_failures == ["error rate 50.0%, more than 40.0%"]
