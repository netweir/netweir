"""deltafetch: a page that gave items in an earlier run isn't fetched again."""

import collections
import contextlib
import csv
import dataclasses
import json
import logging
import socket
import sqlite3
import subprocess
import sys
import threading
import xml.etree.ElementTree as ET
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import worker_spiders

import netweir


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    #: The catalogue's products, by number; the listing links to each.
    products: list[int] = []
    asked: collections.Counter = collections.Counter()
    # The server answers in threads, and += on a Counter isn't atomic
    # without the GIL.
    lock = threading.Lock()

    def log_message(self, *args):
        pass

    def do_GET(self):
        with Handler.lock:
            Handler.asked[self.path] += 1
        if self.path == "/old/1":
            # Moved: the request's own URL is what the record keys. A fixed
            # target: nothing from the request reaches the header.
            self.send_response(301)
            self.send_header("Location", "/item/1")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path.split("?")[0] == "/":
            links = "".join(
                f'<a class="item" data-sku="sku-{n}" href="/item/{n}">p</a>'
                for n in Handler.products
            )
            body = f"<html><body>{links}<a class='about' href='/about'>a</a></body></html>"
        elif self.path.startswith("/item/"):
            number = self.path.rsplit("/", 1)[1].split("?")[0]
            body = f"<html><body><h1>{number}</h1></body></html>"
        else:
            body = "<html><body><p>nothing to sell</p></body></html>"
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"])).decode()
        Handler.asked[f"POST {self.path} {body}"] += 1
        data = f"<html><body><h1>{body}</h1></body></html>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def server():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


@pytest.fixture
def base(server):
    Handler.asked.clear()
    Handler.products = [1, 2, 3]
    return server


def settings(**kw):
    return netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, **kw
    )


class Shop(netweir.Spider):
    """Follows the listing's product links; each product page gives an item."""

    def parse(self, page):
        for link in page.css("a.item"):
            yield page.follow(link.attr("href"), callback=self.product)
        yield page.follow(page.css("a.about::attr(href)").get(), callback=self.about)

    def product(self, page):
        yield {"name": page.css("h1::text").get()}

    def about(self, page):
        # A page that gives no items: it's fetched every run.
        return None


def run(spider, base, output=None, **kw):
    """Runs `spider` from the listing and returns its items and stats."""
    items = []
    spider.start_urls = spider.start_urls or [base + "/"]
    spider.settings = settings(**kw)
    # Passes each item on: one that comes back None counts as dropped.
    stats = spider.run(output=output or (lambda item: items.append(item) or item))
    return items, stats


def names(items):
    return sorted(i["name"] for i in items)


def test_a_second_run_fetches_only_what_is_new(base, tmp_path):
    out = tmp_path / "day1.jsonl"
    delta = str(tmp_path / "delta")
    _, stats = run(Shop(), base, output=str(out), deltafetch=delta)
    lines = out.read_text(encoding="utf-8").splitlines()
    assert sorted(json.loads(line)["name"] for line in lines) == ["1", "2", "3"]
    assert stats["skipped_delta"] == 0
    assert (tmp_path / "delta" / "deltafetch.sqlite3").exists()

    Handler.asked.clear()
    Handler.products = [1, 2, 3, 4]
    items, stats = run(Shop(), base, deltafetch=delta)
    assert names(items) == ["4"]
    assert stats["skipped_delta"] == 3
    # The listing and the page with no items are fetched again; the
    # products already written aren't asked for at all.
    assert Handler.asked == {"/": 1, "/about": 1, "/item/4": 1}


def test_without_deltafetch_everything_is_fetched_every_run(base):
    run(Shop(), base)
    items, stats = run(Shop(), base)
    assert names(items) == ["1", "2", "3"]
    assert stats["skipped_delta"] == 0
    assert Handler.asked["/item/1"] == 2


def test_a_deltafetch_key_stands_for_the_page(base, tmp_path):
    class Keyed(Shop):
        #: Where the links point: the second run reaches the same
        #: products through other URLs.
        suffix = ""

        def parse(self, page):
            for link in page.css("a.item"):
                yield page.follow(
                    link.attr("href") + self.suffix,
                    callback=self.product,
                    meta={"deltafetch_key": link.attr("data-sku")},
                )

    delta = str(tmp_path / "delta")
    run(Keyed(), base, deltafetch=delta)
    Handler.asked.clear()
    again = Keyed()
    again.suffix = "?from=home"
    items, stats = run(again, base, deltafetch=delta)
    assert items == []
    assert stats["skipped_delta"] == 3
    assert Handler.asked == {"/": 1}


def test_a_deltafetch_key_must_be_a_str_or_bytes(base, tmp_path, caplog):
    class Numbered(Shop):
        def parse(self, page):
            yield page.follow("/item/1", callback=self.product, meta={"deltafetch_key": 1})
            # The callback goes on: only that request is dropped.
            yield page.follow("/item/2", callback=self.product)

    items, stats = run(Numbered(), base, deltafetch=str(tmp_path / "delta"))
    assert stats["callback_errors"] == 1
    assert names(items) == ["2"]
    assert Handler.asked["/item/1"] == 0
    assert 'meta["deltafetch_key"] must be a str or bytes, not int' in caplog.text


def test_a_bad_key_from_start_is_dropped_as_from_a_callback(base, tmp_path):
    class Starts(Shop):
        def start(self):
            yield netweir.Request(base + "/item/1", meta={"deltafetch_key": 1.5})
            yield netweir.Request(base + "/item/2", callback=self.product)

    items, stats = run(Starts(), base, deltafetch=str(tmp_path / "delta"))
    assert stats["callback_errors"] == 1
    assert names(items) == ["2"]


def test_without_deltafetch_a_bad_key_is_ignored(base):
    class Numbered(Shop):
        def parse(self, page):
            yield page.follow("/item/1", callback=self.product, meta={"deltafetch_key": 1})

    items, stats = run(Numbered(), base)
    assert (names(items), stats["callback_errors"]) == (["1"], 0)


def test_dont_filter_fetches_a_page_whatever_it_gave(base, tmp_path):
    class Forced(Shop):
        def parse(self, page):
            for link in page.css("a.item"):
                yield page.follow(link.attr("href"), callback=self.product, dont_filter=True)

    delta = str(tmp_path / "delta")
    run(Forced(), base, deltafetch=delta)
    items, stats = run(Forced(), base, deltafetch=delta)
    assert names(items) == ["1", "2", "3"]
    assert stats["skipped_delta"] == 0


def test_reset_fetches_everything_again(base, tmp_path):
    delta = str(tmp_path / "delta")
    run(Shop(), base, deltafetch=delta)
    items, stats = run(Shop(), base, deltafetch=delta, deltafetch_reset=True)
    assert names(items) == ["1", "2", "3"]
    assert stats["skipped_delta"] == 0
    # The reset run recorded afresh.
    items, stats = run(Shop(), base, deltafetch=delta)
    assert (items, stats["skipped_delta"]) == ([], 3)


def test_start_requests_are_skipped_too(base, tmp_path):
    class Direct(Shop):
        start_urls = []

        def parse(self, page):
            yield {"name": page.css("h1::text").get()}

    delta = str(tmp_path / "delta")
    product = Direct()
    product.start_urls = [base + "/item/1"]
    run(product, base, deltafetch=delta)
    again = Direct()
    again.start_urls = [base + "/item/1"]
    items, stats = run(again, base, deltafetch=delta)
    assert (items, stats["skipped_delta"]) == ([], 1)
    assert Handler.asked["/item/1"] == 1


def test_rule_items_record_their_pages(base, tmp_path):
    class Product(netweir.Item):
        name = netweir.css("h1::text")

    class Ruled(netweir.Spider):
        rules = [netweir.Follow("a.item", extract=Product)]

    delta = str(tmp_path / "delta")
    first, _ = run(Ruled(), base, deltafetch=delta)
    assert names(first) == ["1", "2", "3"]
    Handler.asked.clear()
    Handler.products = [1, 2, 3, 4]
    items, stats = run(Ruled(), base, deltafetch=delta)
    assert names(items) == ["4"]
    assert stats["skipped_delta"] == 3
    assert Handler.asked == {"/": 1, "/item/4": 1}


def test_rule_callbacks_record_their_pages(base, tmp_path):
    class Ruled(netweir.Spider):
        rules = [netweir.Follow("a.item", callback="product")]

        def product(self, page):
            yield {"name": page.css("h1::text").get()}

    delta = str(tmp_path / "delta")
    run(Ruled(), base, deltafetch=delta)
    items, stats = run(Ruled(), base, deltafetch=delta)
    assert (items, stats["skipped_delta"]) == ([], 3)


def test_items_from_worker_processes_record_their_pages(base, tmp_path):
    delta = str(tmp_path / "delta")
    first, _ = run(worker_spiders.Shop(), base, workers=2, deltafetch=delta)
    assert len(first) == 6, "two items a product"
    Handler.asked.clear()
    items, stats = run(worker_spiders.Shop(), base, workers=2, deltafetch=delta)
    assert items == []
    assert stats["skipped_delta"] == 3
    assert Handler.asked == {"/": 1}


def test_an_item_a_pipeline_drops_records_nothing(base, tmp_path):
    def no_two(item):
        return None if item["name"] == "2" else item

    delta = str(tmp_path / "delta")
    spider = Shop()
    spider.pipelines = [no_two]
    run(spider, base, deltafetch=delta)
    items, stats = run(Shop(), base, deltafetch=delta)
    assert names(items) == ["2"]
    assert stats["skipped_delta"] == 2


def test_errback_and_on_block_items_record_nothing(base, tmp_path):
    class Lost(netweir.Spider):
        def start(self):
            yield netweir.Request("http://127.0.0.1:9/gone", errback=self.lost)

        def lost(self, request, error):
            yield {"name": "lost"}

    delta = str(tmp_path / "delta")
    for _ in range(2):
        items, stats = run(Lost(), base, deltafetch=delta, retries=0)
        assert names(items) == ["lost"]
        assert (stats["failed"], stats["skipped_delta"]) == (1, 0)


def test_skips_are_logged_at_debug(base, tmp_path, caplog):
    delta = str(tmp_path / "delta")
    run(Shop(), base, deltafetch=delta)
    with caplog.at_level(logging.DEBUG, logger="netweir"):
        run(Shop(), base, deltafetch=delta)
    assert f"gave items in an earlier run, skipped: {base}/item/1" in caplog.text


def test_with_parquet_pages_are_recorded_once_the_file_is_written(base, tmp_path):
    pytest.importorskip("pyarrow")
    delta = str(tmp_path / "delta")
    run(Shop(), base, output=str(tmp_path / "items.parquet"), deltafetch=delta)
    items, stats = run(Shop(), base, deltafetch=delta)
    assert (items, stats["skipped_delta"]) == ([], 3)


def test_with_a_checkpoint_too(base, tmp_path):
    delta = str(tmp_path / "delta")
    out = tmp_path / "items.jsonl"
    run(Shop(), base, output=str(out), deltafetch=delta, checkpoint=str(tmp_path / "cp1"))
    assert len(out.read_text(encoding="utf-8").splitlines()) == 3
    Handler.products = [1, 2, 3, 4]
    items, stats = run(Shop(), base, deltafetch=delta, checkpoint=str(tmp_path / "cp2"))
    assert names(items) == ["4"]
    assert stats["skipped_delta"] == 3


def test_settings_are_checked():
    with pytest.raises(ValueError, match="deltafetch_reset"):
        netweir.Settings(deltafetch_reset=True)
    with pytest.raises(ValueError, match="deltafetch"):
        netweir.Settings(deltafetch="")
    assert netweir.Settings(deltafetch="d", deltafetch_reset=True).deltafetch == "d"


def test_a_pipeline_that_fails_to_close_records_nothing_at_the_end(base, tmp_path):
    class Fails:
        def __call__(self, item):
            return item

        def close(self):
            raise OSError("the upload failed")

    pytest.importorskip("pyarrow")
    delta = str(tmp_path / "delta")
    spider = Shop()
    spider.pipelines = [Fails()]
    # The Parquet file holds every page back to the end.
    out = tmp_path / "items.parquet"
    with pytest.raises(OSError, match="upload failed"):
        run(spider, base, output=str(out), deltafetch=delta)
    # Its pages aren't on record, so its part isn't named either: the next
    # run would read its items along with the ones it fetches again.
    assert not out.exists()
    _, stats = run(Shop(), base, output=str(out), deltafetch=delta)
    assert stats["skipped_delta"] == 0
    pq = pytest.importorskip("pyarrow.parquet")
    found = [n for p in tmp_path.glob("items*.parquet") for n in pq.read_table(p).column("name")]
    assert sorted(str(n) for n in found) == ["1", "2", "3"]
    assert not list(tmp_path.glob("*.partial"))


class Keyed(Shop):
    """Requests the products in ``metas``, each with its meta."""

    metas: dict[int, dict] = {}

    def parse(self, page):
        for n, meta in self.metas.items():
            yield page.follow(f"/item/{n}", callback=self.product, meta=meta)


def keyed(metas):
    spider = Keyed()
    spider.metas = metas
    return spider


def test_an_empty_key_is_no_key(base, tmp_path):
    delta = str(tmp_path / "delta")
    run(keyed({1: {"deltafetch_key": ""}}), base, deltafetch=delta)
    # Were "" a key, product 2 would share product 1's and be skipped.
    items, stats = run(keyed({1: {}, 2: {"deltafetch_key": ""}}), base, deltafetch=delta)
    assert names(items) == ["2"]
    assert stats["skipped_delta"] == 1


def test_a_bytes_key_is_the_same_key_as_its_text(base, tmp_path):
    delta = str(tmp_path / "delta")
    run(keyed({1: {"deltafetch_key": b"sku-1"}}), base, deltafetch=delta)
    items, stats = run(keyed({2: {"deltafetch_key": "sku-1"}}), base, deltafetch=delta)
    assert (items, stats["skipped_delta"]) == ([], 1)


def test_deltafetch_enabled_false_neither_skips_nor_records(base, tmp_path):
    delta = str(tmp_path / "delta")
    off = {"deltafetch_enabled": False}
    run(keyed({1: off, 2: {}}), base, deltafetch=delta)
    items, stats = run(keyed({1: {}, 2: off}), base, deltafetch=delta)
    # 1 wasn't recorded; 2 was, but this time it doesn't ask.
    assert names(items) == ["1", "2"]
    assert stats["skipped_delta"] == 0


def test_a_callback_that_raises_after_an_item_is_fetched_again(base, tmp_path):
    class Breaks(Shop):
        def product(self, page):
            yield {"name": page.css("h1::text").get()}
            if page.url.endswith("/2"):
                raise ValueError("the rest of the page didn't parse")

    delta = str(tmp_path / "delta")
    _, stats = run(Breaks(), base, deltafetch=delta)
    assert stats["callback_errors"] == 1
    Handler.asked.clear()
    items, stats = run(Shop(), base, deltafetch=delta)
    assert names(items) == ["2"]
    assert Handler.asked["/item/2"] == 1


def test_a_callback_that_raises_in_a_worker_is_fetched_again(base, tmp_path):
    delta = str(tmp_path / "delta")
    for _ in range(2):
        items, stats = run(worker_spiders.Breaks(), base, workers=2, deltafetch=delta)
        assert items == [{"before": base + "/"}]
        assert (stats["callback_errors"], stats["skipped_delta"]) == (1, 0)


def test_an_item_a_validate_stage_drops_records_nothing(base, tmp_path):
    @dataclasses.dataclass
    class Product:
        name: str

    class Unnamed(Shop):
        def product(self, page):
            name = page.css("h1::text").get()
            yield {"title": name} if name == "2" else {"name": name}

    delta = str(tmp_path / "delta")
    spider = Unnamed()
    spider.pipelines = [netweir.validate(Product)]
    _, stats = run(spider, base, deltafetch=delta)
    assert stats["items_invalid"] == 1
    items, stats = run(Shop(), base, deltafetch=delta)
    assert names(items) == ["2"]


def test_jsonl_and_csv_keep_every_run_s_items(base, tmp_path):
    delta = str(tmp_path / "delta")
    jsonl, table = tmp_path / "items.jsonl", tmp_path / "items.csv"
    for products in ([1, 2, 3], [1, 2, 3, 4], [1, 2, 3, 4, 5]):
        Handler.products = products
        spider = Shop()
        spider.pipelines = [netweir.export.csv(str(table))]
        run(spider, base, output=str(jsonl), deltafetch=delta)
    lines = jsonl.read_text(encoding="utf-8").splitlines()
    assert sorted(json.loads(line)["name"] for line in lines) == ["1", "2", "3", "4", "5"]
    with open(table, newline="", encoding="utf-8") as f:
        rows = list(csv.reader(f))
    assert rows[0] == ["name"], "one header"
    assert sorted(r[0] for r in rows[1:]) == ["1", "2", "3", "4", "5"]


def test_json_and_xml_stay_valid_across_runs(base, tmp_path):
    delta = str(tmp_path / "delta")
    out, doc = tmp_path / "items.json", tmp_path / "items.xml"
    for products in ([1, 2], [1, 2, 3], [1, 2, 3]):
        Handler.products = products
        spider = Shop()
        spider.pipelines = [netweir.export.xml(str(doc))]
        run(spider, base, output=str(out), deltafetch=delta)
    assert names(json.loads(out.read_text(encoding="utf-8"))) == ["1", "2", "3"]
    found = [e.text for e in ET.parse(doc).getroot().iter("name")]
    assert sorted(found) == ["1", "2", "3"]


def test_parquet_adds_a_part_each_run(base, tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    delta = str(tmp_path / "delta")
    out = tmp_path / "items.parquet"
    run(Shop(), base, output=str(out), deltafetch=delta)
    Handler.products = [1, 2, 3, 4]
    run(Shop(), base, output=str(out), deltafetch=delta)
    first, second = pq.read_table(out), pq.read_table(tmp_path / "items.1.parquet")
    assert sorted(first.column("name").to_pylist()) == ["1", "2", "3"]
    assert second.column("name").to_pylist() == ["4"]
    # Nothing new: no empty part.
    run(Shop(), base, output=str(out), deltafetch=delta)
    assert sorted(p.name for p in tmp_path.glob("items*")) == ["items.1.parquet", "items.parquet"]


def test_a_new_record_starts_the_output_afresh(base, tmp_path):
    out = tmp_path / "items.jsonl"
    out.write_text('{"name": "from before"}\n', encoding="utf-8")
    run(Shop(), base, output=str(out), deltafetch=str(tmp_path / "delta"))
    assert len(out.read_text(encoding="utf-8").splitlines()) == 3
    # And so does a reset: its pages are all fetched again.
    run(Shop(), base, output=str(out), deltafetch=str(tmp_path / "delta"), deltafetch_reset=True)
    assert len(out.read_text(encoding="utf-8").splitlines()) == 3


def test_batches_go_beside_the_last_run_s_never_over_them(base, tmp_path):
    delta = str(tmp_path / "delta")

    def batches():
        return netweir.export.jsonl(str(tmp_path / "p{batch}.jsonl"), batch=2)

    def files():
        return sorted(p.name for p in tmp_path.glob("p*.jsonl"))

    run(Shop(), base, output=batches(), deltafetch=delta)
    assert files() == ["p1.jsonl", "p2.jsonl"]
    Handler.products = [1, 2, 3, 4]
    run(Shop(), base, output=batches(), deltafetch=delta)
    assert files() == ["p1.jsonl", "p2.jsonl", "p3.jsonl"]
    # Nothing new: no empty batch, over the last run's or anywhere.
    run(Shop(), base, output=batches(), deltafetch=delta)
    assert files() == ["p1.jsonl", "p2.jsonl", "p3.jsonl"]
    found = []
    for name in files():
        found += [json.loads(line)["name"] for line in (tmp_path / name).open(encoding="utf-8")]
    assert sorted(found) == ["1", "2", "3", "4"]


def test_a_single_cloud_file_is_refused(base, tmp_path, monkeypatch):
    stage = netweir.export.jsonl(str(tmp_path / "items.jsonl"))
    monkeypatch.setattr(stage, "remote", True)
    with pytest.raises(ValueError, match="replaced each run"):
        run(Shop(), base, output=stage, deltafetch=str(tmp_path / "delta"))


def test_items_already_written_on_resume_record_their_page(base, tmp_path):
    delta, cp = str(tmp_path / "delta"), str(tmp_path / "cp")
    out = str(tmp_path / "items.jsonl")
    # Stopped part way: the page whose item hit the limit is left to the
    # resumed run, which finds its item already written.
    run(Shop(), base, output=out, deltafetch=delta, checkpoint=cp, max_items=1)
    _, stats = run(Shop(), base, output=out, deltafetch=delta, checkpoint=cp)
    assert stats["items_already_written"] >= 1
    items, stats = run(Shop(), base, deltafetch=delta)
    assert (items, stats["skipped_delta"]) == ([], 3)


def test_a_page_cut_short_by_max_items_is_fetched_again(base, tmp_path):
    delta = str(tmp_path / "delta")
    out = tmp_path / "items.jsonl"
    run(Shop(), base, output=str(out), deltafetch=delta, max_items=2)
    run(Shop(), base, output=str(out), deltafetch=delta)
    found = [json.loads(line)["name"] for line in out.read_text(encoding="utf-8").splitlines()]
    # Every item is there; the page whose callback the limit stopped gave
    # its item twice, as it may have had more to give.
    assert sorted(set(found)) == ["1", "2", "3"]
    assert len(found) == 4


KILLED = """
import os, sys
import netweir

class Shop(netweir.Spider):
    start_urls = [sys.argv[1] + "/"]
    settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False,
        deltafetch=sys.argv[2], concurrency=1,
    )

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback=self.product)

    def product(self, page):
        yield {"name": page.css("h1::text").get()}

seen = []

def die_at_the_second(item):
    seen.append(item)
    if len(seen) == 2:
        os._exit(1)
    return item

spider = Shop()
spider.pipelines = [die_at_the_second]
spider.run(output=sys.argv[3])
"""


def test_a_killed_run_loses_no_item_and_writes_none_twice(base, tmp_path):
    delta, out = str(tmp_path / "delta"), str(tmp_path / "items.jsonl")
    run(Shop(), base, output=out, deltafetch=delta)
    Handler.products = [1, 2, 3, 4, 5, 6]
    script = tmp_path / "killed.py"
    script.write_text(KILLED, encoding="utf-8")
    done = subprocess.run(
        [sys.executable, str(script), base, delta, out], capture_output=True, timeout=60
    )
    assert done.returncode == 1, done.stderr
    run(Shop(), base, output=out, deltafetch=delta)
    with open(out, encoding="utf-8") as f:
        found = sorted(json.loads(line)["name"] for line in f)
    assert found == ["1", "2", "3", "4", "5", "6"]


def test_redirects_and_post_bodies_are_known_by_the_request_sent(base, tmp_path):
    class Mixed(Shop):
        terms = ["a", "b"]

        def parse(self, page):
            yield page.follow("/old/1", callback=self.product)
            for term in self.terms:
                yield netweir.Request(base + "/search", body=term, callback=self.product)

    delta = str(tmp_path / "delta")
    run(Mixed(), base, deltafetch=delta)
    Handler.asked.clear()
    again = Mixed()
    again.terms = ["a", "b", "c"]
    items, stats = run(again, base, deltafetch=delta)
    assert names(items) == ["c"]
    assert stats["skipped_delta"] == 3
    assert Handler.asked == {"/": 1, "POST /search c": 1}


def test_an_output_changed_since_it_was_recorded_is_refused(base, tmp_path):
    delta, out = str(tmp_path / "delta"), tmp_path / "items.jsonl"
    run(Shop(), base, output=str(out), deltafetch=delta)
    recorded = len(out.read_bytes())
    # Another run, without deltafetch, writes the same file afresh, past
    # the length on record.
    Handler.products = [10, 11, 12, 13]
    run(Shop(), base, output=str(out))
    before = out.read_bytes()
    assert len(before) > recorded
    with pytest.raises(ValueError, match="isn't the file this crawl wrote"):
        run(Shop(), base, output=str(out), deltafetch=delta)
    assert out.read_bytes() == before, "left as it was"
    # Replaced by a file just as long: refused too.
    out.write_bytes(b"x" * len(before))
    with pytest.raises(ValueError, match="isn't the file this crawl wrote"):
        run(Shop(), base, output=str(out), deltafetch=delta)


def test_an_output_the_record_does_not_know_is_refused_if_it_is_there(base, tmp_path):
    delta = str(tmp_path / "delta")
    run(Shop(), base, output=str(tmp_path / "items.jsonl"), deltafetch=delta)
    other = tmp_path / "other.jsonl"
    other.write_text('{"name": "mine"}\n', encoding="utf-8")
    with pytest.raises(ValueError, match="doesn't know it"):
        run(Shop(), base, output=str(other), deltafetch=delta)
    assert other.read_text(encoding="utf-8") == '{"name": "mine"}\n'


def test_a_moved_output_starts_again_with_only_what_is_new(base, tmp_path):
    delta, out = str(tmp_path / "delta"), tmp_path / "items.jsonl"
    run(Shop(), base, output=str(out), deltafetch=delta)
    out.rename(tmp_path / "archived.jsonl")
    Handler.products = [1, 2, 3, 4]
    run(Shop(), base, output=str(out), deltafetch=delta)
    assert [json.loads(line)["name"] for line in out.open(encoding="utf-8")] == ["4"]
    # A path the record never had, with no file there, is fine too.
    Handler.products = [1, 2, 3, 4, 5]
    run(Shop(), base, output=str(tmp_path / "day3.jsonl"), deltafetch=delta)
    lines = (tmp_path / "day3.jsonl").read_text(encoding="utf-8").splitlines()
    assert [json.loads(line)["name"] for line in lines] == ["5"]


#: A crawl run in its own process, which a kill can end at a chosen point:
#: argv is the site, the directory, the output's format, the kill and
#: flags: "cp=NAME" for a checkpoint, "start=QUERY" to start from the
#: listing under another URL, "nodelta" for no deltafetch.
HARNESS = """
import os, sys
import netweir
from netweir import _crawl, export

base, d, fmt, kill, flags = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5:]
what, _, n = kill.partition(":")
count = {"items": 0, "records": 0}

class Kill:
    def process_item(self, item, spider):
        count["items"] += 1
        if what == "item" and count["items"] == int(n):
            os._exit(3)
        return item

record_pages = _crawl._Run.record_pages

def recording(self, closed=False):
    pages = bool(self.produced)
    recorded = record_pages(self, closed)
    count["records"] += pages
    if what == "recorded" and pages and count["records"] == int(n):
        os._exit(3)
    if what == "closed" and closed:
        os._exit(3)
    return recorded

_crawl._Run.record_pages = recording
if what == "publish":
    export.parquet.publish = lambda self: os._exit(3)
if what == "finish":
    # The exporters are closed, and nothing about them recorded yet.
    _crawl._Run.finish = lambda self, closed=True: os._exit(3)
if what == "opened":
    # The outputs are open (a JSON file has lost its "]"), no item yet.
    started = export._Exporter.start

    def start(self, append=False):
        started(self, append)
        os._exit(3)

    export._Exporter.start = start

out = os.path.join(d, "out." + fmt)
settings = dict(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False,
                concurrency=1, deltafetch=os.path.join(d, "delta"))
if "nodelta" in flags:
    del settings["deltafetch"]
start = "/"
for flag in flags:
    name, _, value = flag.partition("=")
    if name == "cp":
        settings["checkpoint"] = os.path.join(d, value)
    if name == "start":
        start = "/?" + value
    if name == "out":
        out = os.path.join(d, value + "." + fmt)

class Shop(netweir.Spider):
    start_urls = [base + start]

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback=self.product)

    def product(self, page):
        yield {"u": page.url.rsplit("/", 1)[1]}

Shop.settings = netweir.Settings(**settings)
Shop.pipelines = [Kill(), getattr(export, fmt)(out)]
Shop().run()
"""


def written(d, fmt):
    """The items in the harness's output, by number, whatever its format."""
    path = d / f"out.{fmt}"
    if fmt == "jsonl":
        return sorted(int(json.loads(line)["u"]) for line in path.open(encoding="utf-8"))
    if fmt == "json":
        return sorted(int(i["u"]) for i in json.loads(path.read_text(encoding="utf-8")))
    if fmt == "csv":
        with path.open(newline="", encoding="utf-8") as f:
            rows = list(csv.reader(f))
        assert rows[0] == ["u"] and ["u"] not in rows[1:], "one header"
        return sorted(int(r[0]) for r in rows[1:])
    if fmt == "xml":
        return sorted(int(e.text) for e in ET.parse(path).getroot().iter("u"))
    pq = pytest.importorskip("pyarrow.parquet")
    assert not list(d.glob("*.partial"))
    return sorted(int(u) for p in d.glob("out*.parquet") for u in pq.read_table(p).column("u"))


#: Each of a sequence's four runs' flags: no checkpoint; a checkpoint
#: for the run that's killed and the one that resumes it; or one
#: checkpoint for all four, taken up again once finished, from a new
#: start URL each time.
CHECKPOINTS = {
    "none": [[], [], [], []],
    "cp": [["cp=a"], ["cp=b"], ["cp=b"], ["cp=c"]],
    "reused": [["cp=a", f"start={n}"] for n in range(4)],
}


@pytest.mark.parametrize(
    "fmt, kill, checkpoints, delta",
    [
        ("jsonl", "item:3", "none", True),
        ("csv", "item:3", "none", True),
        ("json", "item:3", "none", True),
        ("xml", "item:3", "none", True),
        ("parquet", "item:3", "none", True),
        ("parquet", "publish", "none", True),
        ("parquet", "finish", "none", True),
        ("jsonl", "finish", "none", True),
        # Killed with a JSON or XML file's closing tag just taken off.
        ("json", "opened", "none", True),
        ("xml", "opened", "none", True),
        ("json", "opened", "reused", False),
        # The checkpoint's commits lag behind; the record mustn't get ahead.
        ("jsonl", "recorded:2", "cp", True),
        ("json", "item:3", "cp", True),
        ("parquet", "closed", "cp", True),
        ("jsonl", "item:3", "reused", True),
        # A JSON file taken up again went on past its "]", which a crash
        # then left in the middle of it.
        ("json", "item:3", "reused", False),
        ("xml", "item:3", "reused", False),
    ],
)
def test_a_killed_run_loses_no_item(base, tmp_path, fmt, kill, checkpoints, delta):
    if fmt == "parquet":
        pytest.importorskip("pyarrow")
    script = tmp_path / "harness.py"
    script.write_text(HARNESS, encoding="utf-8")
    flags = CHECKPOINTS[checkpoints]
    if not delta:
        flags = [[*f, "nodelta"] for f in flags]

    def crawl(flags, kill="none"):
        args = [sys.executable, str(script), base, str(tmp_path), fmt, kill, *flags]
        done = subprocess.run(args, capture_output=True, text=True, timeout=60)
        assert done.returncode == (0 if kill == "none" else 3), done.stderr

    Handler.products = [1, 2, 3, 4, 5, 6]
    crawl(flags[0])
    Handler.products = list(range(1, 13))
    crawl(flags[1], kill)
    crawl(flags[2])
    Handler.products = list(range(1, 16))
    crawl(flags[3])
    assert written(tmp_path, fmt) == list(range(1, 16))


#: Two listings, the second slow, so their products settle in batches of
#: their own; with "kill", the run dies just after deltafetch records the
#: page of product 4, while the checkpoint's commit of its items may still
#: be on its way.
ORDERING = """
import os, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import netweir
from netweir import _crawl
from netweir._native import Crawler

d, kill, port = sys.argv[1], sys.argv[2] == "kill", int(sys.argv[3])

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        links = lambda ns: "".join(f'<a class="item" href="/item/{n}">p</a>' for n in ns)
        if self.path == "/a":
            body = '<a class="more" href="/b">b</a>' + links((1, 2, 3))
        elif self.path == "/b":
            time.sleep(0.4)
            body = links((4, 5, 6))
        else:
            body = f"<h1>{self.path}</h1>"
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

# The same port each run, so the requests a resumed run reads back
# from the checkpoint still reach the site.
server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{port}"
fourth = Crawler.delta_key("GET", "http://x/", None, b"/item/4")
record_pages = _crawl._Run.record_pages

def recording(self, closed=False):
    was = fourth in self.produced
    recorded = record_pages(self, closed)
    if kill and was:
        os._exit(3)
    return recorded

_crawl._Run.record_pages = recording

class Shop(netweir.Spider):
    settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False,
        concurrency=1, deltafetch=d, checkpoint=os.path.join(d, "cp"),
    )
    start_urls = [base + "/a"]

    def parse(self, page):
        for href in page.css("a.more::attr(href)").getall():
            yield page.follow(href, callback=self.parse, meta={"deltafetch_key": href})
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback=self.product, meta={"deltafetch_key": href})

    def product(self, page):
        yield {"u": page.url.rsplit("/", 1)[1]}

Shop().run(os.path.join(d, "out.jsonl"))
"""


def test_the_record_is_never_ahead_of_the_checkpoint(tmp_path):
    script = tmp_path / "ordering.py"
    script.write_text(ORDERING, encoding="utf-8")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = str(s.getsockname()[1])
    for kill, code in (("kill", 3), ("again", 0)):
        args = [sys.executable, str(script), str(tmp_path), kill, port]
        done = subprocess.run(args, capture_output=True, text=True, timeout=60)
        assert done.returncode == code, done.stderr
    lines = (tmp_path / "out.jsonl").read_text(encoding="utf-8").splitlines()
    assert sorted(int(json.loads(line)["u"]) for line in lines) == [1, 2, 3, 4, 5, 6]


@pytest.mark.parametrize("delta", [True, False])
def test_a_line_added_after_a_clean_finish_is_refused(base, tmp_path, delta):
    out = tmp_path / "items.jsonl"
    keep = {"deltafetch": str(tmp_path / "delta")} if delta else {}
    keep["checkpoint"] = str(tmp_path / "cp")
    run(Shop(), base, output=str(out), **keep)
    with out.open("a", encoding="utf-8") as f:
        f.write('{"name": "added by hand"}\n')
    before = out.read_bytes()
    with pytest.raises(ValueError, match="has grown since the crawl that wrote it finished"):
        run(Shop(), base, output=str(out), **keep)
    assert out.read_bytes() == before, "left as it was"


def test_a_tail_torn_by_a_crash_is_cut_and_the_run_carries_on(base, tmp_path):
    script = tmp_path / "harness.py"
    script.write_text(HARNESS, encoding="utf-8")

    def crawl(kill="none"):
        args = [sys.executable, str(script), base, str(tmp_path), "jsonl", kill]
        done = subprocess.run(args, capture_output=True, text=True, timeout=60)
        assert done.returncode == (0 if kill == "none" else 3), done.stderr

    crawl()
    Handler.products = [1, 2, 3, 4, 5, 6]
    crawl("item:2")
    # Half an item, as a write cut short leaves it.
    with (tmp_path / "out.jsonl").open("a", encoding="utf-8") as f:
        f.write('{"u": "5')
    crawl()
    assert written(tmp_path, "jsonl") == [1, 2, 3, 4, 5, 6]


def test_a_csv_kept_across_runs_keeps_its_columns(base, tmp_path, caplog):
    class Wider(Shop):
        def product(self, page):
            name = page.css("h1::text").get()
            yield {"extra": "E" + name, "name": name}

    delta, out = str(tmp_path / "delta"), tmp_path / "items.csv"
    run(Shop(), base, output=str(out), deltafetch=delta)
    Handler.products = [1, 2, 3, 4]
    run(Wider(), base, output=str(out), deltafetch=delta)
    with out.open(newline="", encoding="utf-8") as f:
        rows = list(csv.reader(f))
    # Under the file's own header, not this run's first item's keys.
    assert rows[0] == ["name"]
    assert sorted(rows[1:]) == [["1"], ["2"], ["3"], ["4"]]
    assert "no column for extra" in caplog.text
    Handler.products = [1, 2, 3, 4, 5]
    other = netweir.export.csv(str(out), fields=["name", "extra"])
    with pytest.raises(ValueError, match="its columns are"):
        run(Wider(), base, output=other, deltafetch=delta)


def test_a_failed_record_keeps_its_pages_for_the_next(base, tmp_path, monkeypatch):
    real_init = netweir._crawl._Run.__init__

    def init(self, *args, **kwargs):
        real_init(self, *args, **kwargs)
        engine, failed = self.engine, []

        class Failing:
            def __getattr__(self, name):
                return getattr(engine, name)

            def produced(self, keys, files):
                if keys and not failed:
                    failed.append(keys)
                    raise OSError("disk busy")
                return engine.produced(keys, files)

        self.engine = Failing()

    delta, out = str(tmp_path / "delta"), tmp_path / "items.jsonl"
    with monkeypatch.context() as m:
        m.setattr(netweir._crawl._Run, "__init__", init)
        run(Shop(), base, output=str(out), deltafetch=delta)
    Handler.products = [1, 2, 3, 4]
    _, stats = run(Shop(), base, output=str(out), deltafetch=delta)
    assert stats["skipped_delta"] == 3
    found = sorted(json.loads(line)["name"] for line in out.open(encoding="utf-8"))
    assert found == ["1", "2", "3", "4"]


def test_a_new_output_killed_before_its_first_item_is_taken_up(base, tmp_path):
    script = tmp_path / "harness.py"
    script.write_text(HARNESS, encoding="utf-8")

    def crawl(kill="none", *flags):
        args = [sys.executable, str(script), base, str(tmp_path), "jsonl", kill, *flags]
        done = subprocess.run(args, capture_output=True, text=True, timeout=60)
        assert done.returncode == (0 if kill == "none" else 3), done.stderr

    crawl()
    # A path the record has never seen, killed as soon as it's opened.
    crawl("opened", "out=new")
    assert (tmp_path / "new.jsonl").stat().st_size == 0
    Handler.products = [1, 2, 3, 4]
    crawl("none", "out=new")
    lines = (tmp_path / "new.jsonl").read_text(encoding="utf-8").splitlines()
    assert [json.loads(line)["u"] for line in lines] == ["4"]


def parquet_names(tmp_path):
    pq = pytest.importorskip("pyarrow.parquet")
    assert not list(tmp_path.glob("*.partial"))
    return sorted(
        str(n) for p in tmp_path.glob("items*.parquet") for n in pq.read_table(p).column("name")
    )


def test_a_part_not_named_is_not_on_the_checkpoint_either(base, tmp_path):
    pytest.importorskip("pyarrow")

    class Fails:
        def __call__(self, item):
            return item

        def close(self):
            raise OSError("the upload failed")

    delta, out = str(tmp_path / "delta"), str(tmp_path / "items.parquet")
    cp = {n: str(tmp_path / f"cp{n}") for n in (1, 2, 3)}
    run(Shop(), base, output=out, deltafetch=delta, checkpoint=cp[1])
    Handler.products = [1, 2, 3, 4, 5]
    failing = Shop()
    failing.pipelines = [Fails()]
    with pytest.raises(OSError, match="upload failed"):
        run(failing, base, output=out, deltafetch=delta, checkpoint=cp[2])
    # Taking that checkpoint up again writes the part's items again...
    run(Shop(), base, output=out, deltafetch=delta, checkpoint=cp[2])
    assert parquet_names(tmp_path) == ["1", "2", "3", "4", "5"]
    # ...and a run with a checkpoint of its own skips their pages.
    _, stats = run(Shop(), base, output=out, deltafetch=delta, checkpoint=cp[3])
    assert stats["skipped_delta"] == 5
    assert parquet_names(tmp_path) == ["1", "2", "3", "4", "5"]
    # A finished checkpoint taken up again, from another start URL.
    Handler.products = [1, 2, 3, 4, 5, 6]
    again = Shop()
    again.start_urls = [base + "/?again"]
    run(again, base, output=out, deltafetch=delta, checkpoint=cp[2])
    assert parquet_names(tmp_path) == ["1", "2", "3", "4", "5", "6"]


def test_pages_are_recorded_after_a_failed_checkpoint_commit(base, tmp_path, monkeypatch):
    real_init = netweir._crawl._Run.__init__

    def init(self, *args, **kwargs):
        real_init(self, *args, **kwargs)
        engine, failed = self.engine, []

        class FailsOnce:
            def __getattr__(self, name):
                return getattr(engine, name)

            def flush(self):
                if self_run.produced and not failed:
                    failed.append(1)
                    raise OSError("checkpoint write failed: disk busy")
                return engine.flush()

        self_run = self
        self.engine = FailsOnce()

    delta, out = str(tmp_path / "delta"), tmp_path / "items.jsonl"
    with monkeypatch.context() as m:
        m.setattr(netweir._crawl._Run, "__init__", init)
        run(Shop(), base, output=str(out), deltafetch=delta, checkpoint=str(tmp_path / "cp"))
    Handler.products = [1, 2, 3, 4]
    _, stats = run(Shop(), base, output=str(out), deltafetch=delta)
    assert stats["skipped_delta"] == 3
    found = sorted(json.loads(line)["name"] for line in out.open(encoding="utf-8"))
    assert found == ["1", "2", "3", "4"]


def test_a_refused_start_changes_nothing(base, tmp_path):
    delta = str(tmp_path / "delta")
    table, doc = tmp_path / "items.csv", tmp_path / "items.json"
    spider = Shop()
    spider.pipelines = [netweir.export.csv(str(table))]
    run(spider, base, output=str(doc), deltafetch=delta)
    record = tmp_path / "delta" / "deltafetch.sqlite3"

    def files():
        with contextlib.closing(sqlite3.connect(record)) as db:
            return sorted(db.execute("SELECT path, size, tail, clean FROM files"))

    before, json_before = files(), doc.read_bytes()
    other = Shop()
    other.pipelines = [netweir.export.csv(str(table), fields=["name", "price"])]
    with pytest.raises(ValueError, match="its columns are"):
        run(other, base, output=str(doc), deltafetch=delta)
    assert files() == before, "the record is as the clean run left it"
    assert doc.read_bytes() == json_before, "the JSON file keeps its closing ]"


def test_an_item_with_no_fields_does_not_start_a_csv(base, tmp_path, caplog):
    class Empty(Shop):
        def parse(self, page):
            yield {}
            yield from super().parse(page)

    out = tmp_path / "items.csv"
    run(Empty(), base, output=str(out))
    with out.open(newline="", encoding="utf-8") as f:
        rows = list(csv.reader(f))
    assert rows[0] == ["name"]
    assert sorted(rows[1:]) == [["1"], ["2"], ["3"]]
    assert "an item with no fields was left out" in caplog.text
