"""Spiders, the crawl loop, pipelines, exporters and the command line."""

import asyncio
import csv
import json
import logging
import signal
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

ROBOTS = "User-agent: *\nDisallow: /private\n"


def listing(n, last):
    nxt = f'<li class="next"><a href="/page/{n + 1}">next</a></li>' if n < last else ""
    books = "".join(
        f'<article><h3><a href="/book/{n}-{i}">Book {n}-{i}</a></h3><p class="price">£{n}.{i}0</p></article>'
        for i in range(3)
    )
    return f"<html><body>{books}<ul>{nxt}</ul><a href='/private/x'>hidden</a></body></html>"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    log = []

    def log_message(self, *args):
        pass

    def send(self, status, body, ctype="text/html; charset=utf-8"):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        Handler.log.append(self.path)
        p = self.path
        if p == "/robots.txt":
            self.send(200, ROBOTS, "text/plain")
        elif p.startswith("/page/"):
            n = int(p.rsplit("/", 1)[1])
            self.send(200, listing(n, 3))
        elif p.startswith("/book/"):
            self.send(200, f"<h1>{p.rsplit('/', 1)[1]}</h1>")
        elif p == "/based":
            self.send(200, '<head><base href="/book/"></head><a href="y">y</a>')
        elif p.startswith("/slow/"):
            # An endless chain of pages, each a moment late.
            time.sleep(0.2)
            n = int(p.rsplit("/", 1)[1])
            self.send(200, f'<h1>{n}</h1><a href="/slow/{n + 1}">next</a>')
        elif p == "/boom":
            self.send(500, "<h1>error</h1>")
        else:
            self.send(404, "<h1>missing</h1>")


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


FAST = netweir.Settings(throttle=False, start_delay=0, backoff_base=0.01, backoff_max=0.02)


class Books(netweir.Spider):
    settings = FAST

    async def parse(self, page):
        for a in page.css("article h3 a"):
            yield {"title": a.text, "price": a.xpath("../../p/text()").get()}
        if nxt := page.css("li.next a::attr(href)").get():
            yield page.follow(nxt)


def test_a_spider_follows_links_and_yields_items(base):
    spider = Books()
    spider.start_urls = [f"{base}/page/1"]
    items = []
    spider.pipelines = [lambda item: items.append(item) or item]
    stats = spider.run()
    assert [i["title"] for i in items][:3] == ["Book 1-0", "Book 1-1", "Book 1-2"]
    assert len(items) == 9
    assert stats["items"] == 9
    assert stats["fetched"] == 3
    assert stats["callback_errors"] == 0


def test_output_files_by_extension(base, tmp_path):
    spider = Books()
    spider.start_urls = [f"{base}/page/1"]
    out = tmp_path / "books.jsonl"
    spider.run(output=str(out))
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert len(rows) == 9 and rows[0] == {"title": "Book 1-0", "price": "£1.00"}
    assert out.read_text(encoding="utf-8").startswith('{"title":'), "keys keep the item's order"

    out = tmp_path / "books.csv"
    spider.run(output=str(out))
    with out.open(encoding="utf-8", newline="") as f:
        rows = list(csv.DictReader(f))
    assert rows[0] == {"title": "Book 1-0", "price": "£1.00"} and len(rows) == 9
    assert out.read_text(encoding="utf-8").startswith("title,price\n")

    with pytest.raises(ValueError, match="txt"):
        spider.run(output=str(tmp_path / "books.txt"))


def test_robots_txt_keeps_the_spider_out(base):
    class Follow(netweir.Spider):
        settings = FAST

        async def parse(self, page):
            yield page.follow("/private/x")
            yield {"url": page.url}

    spider = Follow()
    spider.start_urls = [f"{base}/page/3"]
    Handler.log.clear()
    stats = spider.run()
    assert stats["dropped_robots"] == 1
    assert "/private/x" not in Handler.log


def test_each_page_is_fetched_once(base):
    class Loop(netweir.Spider):
        settings = FAST

        async def parse(self, page):
            yield page.follow("/page/2")
            yield page.follow("/page/2?")  # same page, written differently
            yield {"url": page.url}

    spider = Loop()
    spider.start_urls = [f"{base}/page/2", f"{base}/page/2#top"]
    stats = spider.run()
    assert stats["fetched"] == 1
    assert stats["duplicates"] == 3


@pytest.mark.parametrize("style", ["async_gen", "gen", "coroutine", "plain", "none"])
def test_every_kind_of_callback(base, style):
    def items(page):
        return [{"url": page.url}]

    async def async_gen(self, page):
        for i in items(page):
            yield i

    def gen(self, page):
        yield from items(page)

    async def coroutine(self, page):
        await asyncio.sleep(0)
        return items(page)

    def plain(self, page):
        return items(page)

    def none(self, page):
        return None

    callbacks = {
        "async_gen": async_gen,
        "gen": gen,
        "coroutine": coroutine,
        "plain": plain,
        "none": none,
    }
    spider_cls = type("S", (netweir.Spider,), {"settings": FAST, "parse": callbacks[style]})
    spider = spider_cls()
    spider.start_urls = [f"{base}/book/x"]
    assert spider.run()["items"] == (0 if style == "none" else 1)


def test_pipelines_transform_and_drop(base, tmp_path):
    seen = []

    def only_cheap(item):
        return item if item["price"] < "£2" else None

    async def shout(item):
        await asyncio.sleep(0)
        return {**item, "title": item["title"].upper()}

    spider = Books()
    spider.start_urls = [f"{base}/page/1"]
    spider.pipelines = [only_cheap, shout, lambda i: seen.append(i) or i]
    stats = spider.run()
    assert [i["title"] for i in seen] == ["BOOK 1-0", "BOOK 1-1", "BOOK 1-2"]
    assert stats["items"] == 3
    assert stats["items_dropped"] == 6


def test_failures_reach_the_errback(base):
    errors = []

    class Failing(netweir.Spider):
        settings = netweir.Settings(
            throttle=False,
            start_delay=0,
            obey_robots=False,
            obey_tdmrep=False,
            backoff_base=0.01,
            backoff_max=0.02,
        )

        async def start(self):
            yield netweir.Request("http://127.0.0.1:9/nothing", errback=self.lost)
            yield netweir.Request(f"{base}/boom", callback=self.status)

        async def lost(self, request, error):
            errors.append((request.url, error.kind))
            yield {"lost": request.url}

        async def status(self, page):
            yield {"status": page.status}

    stats = Failing().run()
    assert errors == [("http://127.0.0.1:9/nothing", "connect")]
    assert stats["failed"] == 1
    assert stats["items"] == 2, "a 500 is a page; the errback's item counts too"


def test_callback_errors_are_logged_and_counted(base, caplog):
    class Broken(netweir.Spider):
        settings = FAST

        async def parse(self, page):
            raise RuntimeError("bad selector logic")
            yield

    spider = Broken()
    spider.start_urls = [f"{base}/book/a", f"{base}/book/b"]
    with caplog.at_level(logging.ERROR, logger="netweir"):
        stats = spider.run()
    assert stats["callback_errors"] == 2
    assert "bad selector logic" in caplog.text
    assert f"{base}/book/a" in caplog.text

    spider.settings = netweir.Settings(throttle=False, start_delay=0, fail_fast=True)
    with pytest.raises(RuntimeError, match="bad selector logic"):
        spider.run()


def test_meta_named_callbacks_and_priorities(base):
    seen = []

    class Meta(netweir.Spider):
        settings = netweir.Settings(throttle=False, start_delay=0, per_domain=1)

        async def start(self):
            yield netweir.Request(f"{base}/book/low", callback="detail", meta={"n": 1})
            yield netweir.Request(f"{base}/book/high", callback="detail", meta={"n": 2}, priority=5)

        async def detail(self, page):
            seen.append((page.css("h1::text").get(), page.meta["n"], page.request.priority))
            yield {"n": page.meta["n"]}

    Meta().run()
    assert sorted(seen) == [("high", 2, 5), ("low", 1, 0)]


def test_follow_all_and_relative_urls(base):
    class All(netweir.Spider):
        settings = FAST

        async def parse(self, page):
            if "/page/" in page.url:
                for request in page.follow_all(
                    page.css("article h3 a::attr(href)"), callback=self.book
                ):
                    yield request

        async def book(self, page):
            yield {"book": page.css("h1::text").get()}

    spider = All()
    spider.start_urls = [f"{base}/page/1"]
    assert spider.run()["items"] == 3


def test_settings_are_checked():
    with pytest.raises(ValueError):
        netweir.Settings(concurrency=0)
    with pytest.raises(TypeError):
        netweir.Settings(concurency=4)  # a typo is an error, not ignored


def test_a_spider_without_parse_says_so(base):
    class Lazy(netweir.Spider):
        settings = FAST

    spider = Lazy()
    spider.start_urls = [f"{base}/book/x"]
    assert spider.run()["callback_errors"] == 1


def test_command_line(base, tmp_path):
    script = tmp_path / "books.py"
    script.write_text(
        "import netweir\n"
        "class Books(netweir.Spider):\n"
        f"    start_urls = ['{base}/page/1']\n"
        "    async def parse(self, page):\n"
        "        for a in page.css('article h3 a::text'):\n"
        "            yield {'title': a}\n"
    )
    out = tmp_path / "out.jsonl"
    done = subprocess.run(
        [
            sys.executable,
            "-m",
            "netweir",
            "crawl",
            str(script),
            "-o",
            str(out),
            "-s",
            "throttle=false",
            "-s",
            "start_delay=0",
            "-s",
            "concurrency=4",
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 0, done.stderr
    assert len(out.read_text(encoding="utf-8").splitlines()) == 3
    assert "fetched" in done.stderr

    bad = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-s", "nonsense=1"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert bad.returncode == 2
    assert "nonsense" in bad.stderr


def test_parquet_output(base, tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    spider = Books()
    spider.start_urls = [f"{base}/page/1"]
    out = tmp_path / "books.parquet"
    spider.run(output=str(out))
    table = pq.read_table(out)
    assert table.num_rows == 9
    assert table.column_names == ["title", "price"]
    assert table.to_pylist()[0] == {"title": "Book 1-0", "price": "£1.00"}


def test_parquet_column_types(tmp_path, caplog):
    pq = pytest.importorskip("pyarrow.parquet")
    pa = pytest.importorskip("pyarrow")
    path = str(tmp_path / "t.parquet")
    exporter = netweir.export.parquet(path)
    exporter({"n": 1, "x": 1, "ok": True, "tags": ["a"], "none": None, "s": "a"})
    exporter({"n": 2, "x": 2.5, "ok": False, "tags": [], "s": 3})
    for i in range(20_000):  # past the sample and over a row group boundary
        exporter({"n": i, "x": i, "ok": True, "s": "z"})
    exporter({"n": "not a number", "late": 1})
    with caplog.at_level(logging.WARNING, logger="netweir"):
        exporter.close()

    f = pq.ParquetFile(path)
    assert f.metadata.num_row_groups == 3
    schema = f.schema_arrow
    assert schema.field("n").type == pa.int64()
    assert schema.field("x").type == pa.float64(), "an int and a float make a double"
    assert schema.field("ok").type == pa.bool_()
    assert schema.field("tags").type == pa.string(), "nested values are JSON text"
    assert schema.field("none").type == pa.string(), "a column of nothing is text"
    assert schema.field("s").type == pa.string()
    rows = f.read().to_pylist()
    assert rows[0] == {"n": 1, "x": 1.0, "ok": True, "tags": '["a"]', "none": None, "s": "a"}
    assert rows[1]["s"] == "3" and rows[1]["tags"] == "[]"
    assert rows[2]["x"] == 0.0, "ints after the sample widen into a double column"
    assert rows[-1]["n"] is None
    assert len(rows) == 20_003
    assert "'n'" in caplog.text and "late" in caplog.text


def test_parquet_with_no_items(tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    path = str(tmp_path / "empty.parquet")
    netweir.export.parquet(path).close()
    assert pq.read_table(path).num_rows == 0


def test_ignoring_robots_txt_is_said_once(base, caplog):
    spider = Books()
    spider.settings = netweir.Settings(throttle=False, start_delay=0, obey_robots=False)
    spider.start_urls = [f"{base}/page/1"]
    with caplog.at_level(logging.WARNING, logger="netweir"):
        spider.run()
    assert [r.message for r in caplog.records].count(
        "obey_robots is off: robots.txt is not being checked"
    ) == 1


def test_urljoin_resolves_against_the_page(base):
    seen = []

    class Join(netweir.Spider):
        settings = FAST

        async def parse(self, page):
            seen.append((page.urljoin("../book/x"), page.urljoin("https://e.com/a")))
            return None

    spider = Join()
    spider.start_urls = [f"{base}/page/1"]
    spider.run()
    assert seen == [(f"{base}/book/x", "https://e.com/a")]

    seen.clear()
    spider.start_urls = [f"{base}/based"]
    spider.run()
    assert seen == [(f"{base}/book/x", "https://e.com/a")], "<base href> counts"

    class Follow(netweir.Spider):
        settings = FAST

        async def parse(self, page):
            if page.url.endswith("/based"):
                yield page.follow("y")
            else:
                seen.append(page.url)

    seen.clear()
    spider = Follow()
    spider.start_urls = [f"{base}/based"]
    spider.run()
    assert seen == [f"{base}/book/y"]


def test_exporters_on_the_class_survive_a_second_run(base, tmp_path):
    out = tmp_path / "books.jsonl"

    class Twice(Books):
        pipelines = [netweir.export.jsonl(str(out))]

    Twice.start_urls = [f"{base}/page/3"]
    Twice().run()
    stats = Twice().run()
    assert stats["callback_errors"] == 0 and stats["items"] == 3
    assert len(out.read_text(encoding="utf-8").splitlines()) == 3, (
        "the second run starts the file afresh"
    )


def test_odd_callback_results(base, caplog):
    class Odd(netweir.Spider):
        settings = FAST

        def parse(self, page):
            return b"abc"

    spider = Odd()
    spider.start_urls = [f"{base}/book/x"]
    with caplog.at_level(logging.ERROR, logger="netweir"):
        stats = spider.run()
    assert stats["items"] == 0
    assert stats["callback_errors"] == 1
    assert "bytes" in caplog.text


def test_an_unknown_callback_name_is_counted_not_raised(base):
    class Missing(netweir.Spider):
        settings = FAST

        async def start(self):
            yield netweir.Request(f"{base}/book/x", callback="nope")

    assert Missing().run()["callback_errors"] == 1


def test_start_can_be_a_plain_generator(base):
    class Plain(netweir.Spider):
        settings = FAST

        def start(self):
            yield netweir.Request(f"{base}/book/x")

        def parse(self, page):
            yield {"url": page.url}

    assert Plain().run()["items"] == 1


def test_command_line_spiders_can_use_dataclasses(base, tmp_path):
    script = tmp_path / "netweir.py"  # a name that could shadow the package
    script.write_text(
        "from __future__ import annotations\n"
        "import dataclasses\n"
        "import netweir\n"
        "@dataclasses.dataclass\n"
        "class Book:\n"
        "    url: str\n"
        "class One(netweir.Spider):\n"
        f"    start_urls = ['{base}/book/x']\n"
        "    def parse(self, page):\n"
        "        yield Book(page.url)\n"
    )
    out = tmp_path / "out.jsonl"
    done = subprocess.run(
        [
            sys.executable,
            "-m",
            "netweir",
            "crawl",
            str(script),
            "-o",
            str(out),
            "-s",
            "throttle=false",
            "-s",
            "start_delay=0",
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 0, done.stderr
    assert json.loads(out.read_text(encoding="utf-8")) == {"url": f"{base}/book/x"}


def test_settings_on_the_command_line_get_their_types():
    import dataclasses

    from netweir._cli import _parse_setting

    fields = {f.name: f for f in dataclasses.fields(netweir.Settings)}
    assert _parse_setting("max_depth=3", fields) == ("max_depth", 3)
    assert _parse_setting("max_depth=none", fields) == ("max_depth", None)
    assert _parse_setting("max_delay=2.5", fields) == ("max_delay", 2.5)
    assert _parse_setting("timeout=10", fields) == ("timeout", 10.0)
    assert _parse_setting("proxy=http://p:8080", fields) == ("proxy", "http://p:8080")
    assert _parse_setting("proxies=http://a:1,http://b:2", fields) == (
        "proxies",
        ("http://a:1", "http://b:2"),
    )


@pytest.mark.parametrize(
    "args, message",
    [
        (["-s", "profile=netscape"], "netscape"),
        (["-o", "/nonexistent/dir/x.jsonl"], "nonexistent"),
        (["-o", "out.txt"], "txt"),
        (["-s", "max_depth=deep"], "max_depth takes a whole number"),
        (["-s", "timeout=soon"], "timeout takes a number"),
    ],
)
def test_command_line_errors_are_one_line(base, tmp_path, args, message):
    script = tmp_path / "s.py"
    script.write_text("import netweir\nclass S(netweir.Spider):\n    start_urls = []\n")
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), *args],
        capture_output=True,
        text=True,
        timeout=60,
        cwd=tmp_path,
    )
    assert done.returncode == 2, done.stderr
    assert "Traceback" not in done.stderr
    assert message in done.stderr


def test_command_line_missing_file(tmp_path):
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(tmp_path / "nope.py")],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 2
    assert "no such file" in done.stderr and "Traceback" not in done.stderr


def test_max_depth_stops_following(base):
    depths = []

    class Deep(netweir.Spider):
        settings = netweir.Settings(throttle=False, start_delay=0, max_depth=1)

        async def parse(self, page):
            depths.append((page.url.rsplit("/", 1)[1], page.depth))
            if nxt := page.css("li.next a::attr(href)").get():
                yield page.follow(nxt)

    spider = Deep()
    spider.start_urls = [f"{base}/page/1"]
    stats = spider.run()
    assert depths == [("1", 0), ("2", 1)]
    assert stats["skipped_depth"] == 1


def test_max_pages_per_domain(base):
    spider = Books()
    spider.settings = netweir.Settings(throttle=False, start_delay=0, max_pages_per_domain=2)
    spider.start_urls = [f"{base}/page/1"]
    stats = spider.run()
    assert stats["fetched"] == 2 and stats["skipped_domain_full"] == 1


def test_command_line_spider_arguments(base, tmp_path):
    script = tmp_path / "args.py"
    script.write_text(
        "import netweir\n"
        "class Args(netweir.Spider):\n"
        "    colour = 'red'\n"
        f"    start_urls = ['{base}/book/x']\n"
        "    def parse(self, page):\n"
        "        yield {'colour': self.colour, 'size': self.size}\n"
    )
    out = tmp_path / "out.jsonl"
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-o", str(out),
         "-a", "colour=blue", "-a", "size=a=b", "-s", "throttle=false", "-s", "start_delay=0"],
        capture_output=True, text=True, timeout=60,
    )  # fmt: skip
    assert done.returncode == 0, done.stderr
    assert json.loads(out.read_text(encoding="utf-8")) == {"colour": "blue", "size": "a=b"}


@pytest.mark.parametrize(
    "arg, message",
    [("colour", "NAME=VALUE"), ("not-a-name=1", "not-a-name"), ("settings=x", "use -s")],
)
def test_command_line_spider_argument_mistakes(tmp_path, arg, message):
    script = tmp_path / "s.py"
    script.write_text("import netweir\nclass S(netweir.Spider):\n    start_urls = []\n")
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-a", arg],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 2, done.stderr
    assert message in done.stderr and "Traceback" not in done.stderr


def test_command_line_stats_file(base, tmp_path):
    script = tmp_path / "books.py"
    script.write_text(
        "import netweir\n"
        "class Books(netweir.Spider):\n"
        f"    start_urls = ['{base}/page/1']\n"
        "    def parse(self, page):\n"
        "        yield {'url': page.url}\n"
    )
    stats = tmp_path / "stats.json"
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "--stats", str(stats),
         "-s", "throttle=false", "-s", "start_delay=0"],
        capture_output=True, text=True, timeout=60,
    )  # fmt: skip
    assert done.returncode == 0, done.stderr
    saved = json.loads(stats.read_text(encoding="utf-8"))
    assert saved["finish_reason"] == "finished"
    assert saved["items"] == 1 and saved["fetched"] == 1


def test_command_line_stats_file_with_a_component_s_own_stats(base, tmp_path):
    # Scrapy's components keep datetimes in the stats (start_time).
    script = tmp_path / "books.py"
    script.write_text(
        "import datetime\n"
        "import netweir\n"
        "class Clock:\n"
        "    @classmethod\n"
        "    def from_crawler(cls, crawler):\n"
        "        crawler.stats.set_value('start_time', datetime.datetime(2026, 1, 2))\n"
        "        return cls()\n"
        "class Books(netweir.Spider):\n"
        f"    start_urls = ['{base}/page/1']\n"
        "    middlewares = [Clock]\n"
        "    def parse(self, page):\n"
        "        yield {'url': page.url}\n"
    )
    stats = tmp_path / "stats.json"
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "--stats", str(stats),
         "-s", "throttle=false", "-s", "start_delay=0"],
        capture_output=True, text=True, timeout=60,
    )  # fmt: skip
    assert done.returncode == 0, done.stderr
    saved = json.loads(stats.read_text(encoding="utf-8"))
    assert saved["start_time"] == "2026-01-02 00:00:00"
    assert saved["items"] == 1


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX signals")
def test_sigterm_stops_a_crawl_as_ctrl_c_does(base, tmp_path):
    script = tmp_path / "slow.py"
    script.write_text(
        "import netweir\n"
        "class Slow(netweir.Spider):\n"
        f"    start_urls = ['{base}/slow/1']\n"
        "    def parse(self, page):\n"
        "        yield {'n': page.css('h1::text').get()}\n"
        "        yield page.follow(page.css('a::attr(href)').get())\n"
    )
    out, stats = tmp_path / "out.jsonl", tmp_path / "stats.json"
    proc = subprocess.Popen(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-o", str(out),
         "--stats", str(stats), "-s", "throttle=false", "-s", "start_delay=0"],
        stderr=subprocess.PIPE, text=True,
    )  # fmt: skip
    time.sleep(2)  # pages come every 0.2 s
    proc.send_signal(signal.SIGTERM)
    _, err = proc.communicate(timeout=30)
    assert proc.returncode == 130, err
    assert "netweir: interrupted" in err and "Traceback" not in err
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert len(rows) >= 3
    saved = json.loads(stats.read_text(encoding="utf-8"))
    assert saved["finish_reason"] == "interrupted"
    assert saved["items"] == len(rows)


# Stops the crawl at the third page, just after the future a task waits on
# is done and before the task is told: a KeyboardInterrupt raised there
# leaves the task waiting for good.
INTERRUPTED_AS_A_TASK_WAKES = """
import asyncio, signal, sys
from netweir import _cli

call_soon = asyncio.BaseEventLoop.call_soon
armed = False

def interrupting(loop, callback, *args, **kwargs):
    global armed
    if armed and isinstance(getattr(callback, "__self__", None), asyncio.Task):
        armed = False
        signal.raise_signal(SIGNAL)
    return call_soon(loop, callback, *args, **kwargs)

asyncio.BaseEventLoop.call_soon = interrupting
if IGNORE_SIGINT:
    signal.signal(signal.SIGINT, signal.SIG_IGN)
sys.exit(_cli.main(sys.argv[1:]))
"""


@pytest.mark.parametrize(
    "stop, ignore_sigint",
    [("SIGINT", False), ("SIGTERM", False), ("SIGTERM", True)],
    ids=["ctrl-c", "sigterm", "sigterm-with-sigint-ignored"],
)
def test_an_interrupt_as_a_task_wakes_still_ends_the_crawl(base, tmp_path, stop, ignore_sigint):
    script = tmp_path / "slow.py"
    script.write_text(
        "import netweir\n"
        "class Slow(netweir.Spider):\n"
        f"    start_urls = ['{base}/slow/1']\n"
        "    def parse(self, page):\n"
        "        import __main__\n"
        "        __main__.armed = page.url.endswith('/3')\n"
        "        yield {'n': page.css('h1::text').get()}\n"
        "        yield page.follow(page.css('a::attr(href)').get())\n"
    )
    out, stats = tmp_path / "out.jsonl", tmp_path / "stats.json"
    runner = INTERRUPTED_AS_A_TASK_WAKES.replace("SIGNAL", f"signal.{stop}").replace(
        "IGNORE_SIGINT", str(ignore_sigint)
    )
    done = subprocess.run(
        [sys.executable, "-c", runner, "crawl", str(script), "-o", str(out),
         "--stats", str(stats), "-s", "throttle=false", "-s", "start_delay=0"],
        capture_output=True, text=True, timeout=30,
    )  # fmt: skip
    assert done.returncode == 130, done.stderr
    assert "netweir: interrupted" in done.stderr and "Traceback" not in done.stderr
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert len(rows) >= 3
    assert json.loads(stats.read_text(encoding="utf-8"))["finish_reason"] == "interrupted"


CTRL_C_WITH_ANOTHER_TASK = """
import asyncio, signal
from netweir._crawl import _run

async def side():
    try:
        await asyncio.sleep(60)
    finally:
        await asyncio.sleep(0.1)
        print("side wound up", flush=True)

async def main():
    side_task = asyncio.ensure_future(side())
    await asyncio.sleep(0.1)
    signal.raise_signal(signal.SIGINT)
    try:
        await asyncio.sleep(60)
    finally:
        side_task.cancel()
        await asyncio.sleep(0.1)
        print("main wound up", flush=True)

try:
    _run(main())
except KeyboardInterrupt:
    print("interrupted", flush=True)
"""


def test_ctrl_c_lets_every_task_wind_up_before_it_is_raised():
    """As Python 3.11's asyncio.run does: KeyboardInterrupt raised inside
    the main task would end the loop with the other tasks' cleanup not
    run, and asyncio complaining of a pending task destroyed."""
    done = subprocess.run(
        [sys.executable, "-c", CTRL_C_WITH_ANOTHER_TASK], capture_output=True, text=True, timeout=30
    )
    assert done.returncode == 0, done.stderr
    assert done.stdout.split("\n") == ["main wound up", "side wound up", "interrupted", ""]
    assert "never retrieved" not in done.stderr and "destroyed" not in done.stderr


# Stops the crawl just as the third item has been written to the output
# file, before the crawl counts it: once, or twice, as a second Ctrl-C
# that raises at once.
INTERRUPTED_AS_AN_ITEM_IS_WRITTEN = """
import signal, sys
from netweir import _cli, export

write = export._Exporter._write

def writing(self, item):
    result = write(self, item)
    if item.get("n") == "3":
        for _ in range(TIMES):
            signal.raise_signal(SIGNAL)
    return result

export._Exporter._write = writing
sys.exit(_cli.main(sys.argv[1:]))
"""


@pytest.mark.parametrize(
    "stop",
    [
        "SIGINT",
        "SIGTERM",
        pytest.param(
            "SIGBREAK",
            marks=pytest.mark.skipif(sys.platform != "win32", reason="Windows' Ctrl-Break"),
        ),
    ],
)
@pytest.mark.parametrize("workers", ["1", "2"], ids=["alone", "workers"])
@pytest.mark.parametrize("times", [1, 2], ids=["once", "twice"])
def test_an_interrupt_as_an_item_is_written_still_counts_it(base, tmp_path, stop, workers, times):
    script = tmp_path / "slow.py"
    script.write_text(
        "import netweir\n"
        "class Slow(netweir.Spider):\n"
        f"    start_urls = ['{base}/slow/1']\n"
        "    def parse(self, page):\n"
        "        yield {'n': page.css('h1::text').get()}\n"
        "        yield page.follow(page.css('a::attr(href)').get())\n"
    )
    out, stats = tmp_path / "out.jsonl", tmp_path / "stats.json"
    runner = INTERRUPTED_AS_AN_ITEM_IS_WRITTEN.replace("SIGNAL", f"signal.{stop}")
    done = subprocess.run(
        [sys.executable, "-c", runner.replace("TIMES", str(times)),
         "crawl", str(script), "-o", str(out), "--stats", str(stats),
         "-s", "throttle=false", "-s", "start_delay=0", "-s", f"workers={workers}"],
        capture_output=True, text=True, timeout=30,
    )  # fmt: skip
    assert done.returncode == 130, done.stderr
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert [row["n"] for row in rows] == ["1", "2", "3"]
    saved = json.loads(stats.read_text(encoding="utf-8"))
    assert saved["finish_reason"] == "interrupted"
    assert saved["items"] == len(rows)


def test_stats_are_kept_when_a_crawl_raises(base):
    class Raises(netweir.Spider):
        settings = netweir.Settings(throttle=False, start_delay=0, fail_fast=True)
        start_urls = [f"{base}/book/x"]

        def parse(self, page):
            raise RuntimeError("no")

    spider = Raises()
    with pytest.raises(RuntimeError):
        spider.run()
    assert spider.stats["fetched"] == 1


def test_the_command_runs_in_a_thread_and_leaves_sigterm_alone(base, tmp_path):
    from netweir import _cli

    script = tmp_path / "one.py"
    script.write_text(
        "import netweir\n"
        "class One(netweir.Spider):\n"
        f"    start_urls = ['{base}/book/x']\n"
        "    def parse(self, page):\n"
        "        yield {'url': page.url}\n"
    )
    argv = ["crawl", str(script), "-q", "-s", "throttle=false", "-s", "start_delay=0"]
    before = signal.getsignal(signal.SIGTERM)
    results = []
    thread = threading.Thread(target=lambda: results.append(_cli.main(argv)))
    thread.start()
    thread.join(60)
    assert results == [0]
    assert _cli.main(argv) == 0
    assert signal.getsignal(signal.SIGTERM) is before
