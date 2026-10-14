"""Tracked selectors: an element followed through a redesign."""

import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import netweir
import netweir._track

PAGES = Path(__file__).parent.parent / "crates/netweir-dom/tests/fixtures/redesigns"


def page(name):
    return (PAGES / name).read_text(encoding="utf-8")


@pytest.fixture(autouse=True)
def tracks_home(tmp_path, monkeypatch):
    """A fresh ~/.netweir for every test."""
    monkeypatch.setenv("NETWEIR_HOME", str(tmp_path / "home"))
    netweir._track._reset()
    yield
    netweir._track._reset()


def test_a_tracked_selector_finds_its_element_after_a_redesign(caplog):
    before = netweir.parse(page("class-renamed.before.html"))
    found = before.css("p.price_color::text", track="price")
    assert (found.get(), found.relocated, found.score) == ("£24.99", False, 1.0)

    after = netweir.parse(page("class-renamed.after.html"))
    assert after.css("p.price_color::text").get() is None, "the plain selector misses"
    with caplog.at_level(logging.WARNING, logger="netweir"):
        found = after.css("p.price_color::text", track="price")
    assert found.get() == "£24.99"
    assert found.relocated and found.score >= 0.75
    assert "price" in caplog.text and "similar" in caplog.text


def test_xpath_can_be_tracked_too():
    before = netweir.parse(page("reordered.before.html"))
    assert before.xpath("//dd[@class='upc']/text()", track="upc").get() == "a897fe39"
    after = netweir.parse(page("reordered.after.html"))
    found = after.xpath("//dd[@class='upc']/text()", track="upc")
    assert found.get() == "a897fe39" and found.relocated
    with pytest.raises(TypeError, match="variables"):
        after.xpath("//dd[@class=$c]", track="upc", c="upc")


def test_an_element_that_is_gone_is_not_made_up(caplog):
    netweir.parse(page("gone.before.html")).css("p.discount::text", track="discount")
    with caplog.at_level(logging.WARNING, logger="netweir"):
        found = netweir.parse(page("gone.after.html")).css("p.discount::text", track="discount")
    assert found.get() is None and not found.relocated
    assert found.score < 0.75, "the score says how close the nearest thing came"
    # A name with nothing saved for it is simply not found.
    assert netweir.parse("<p>").css("i::text", track="never-seen").score is None
    assert "discount" in caplog.text


def test_only_one_selector_can_be_tracked():
    doc = netweir.parse("<p>x</p>")
    with pytest.raises(netweir.SelectorError, match="list"):
        doc.css("p, a", track="x")


SITE = {"version": "before"}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.startswith("/product"):
            body = page(f"class-renamed.{SITE['version']}.html")
        else:
            body = '<a class="p" href="/product/1">one</a>'
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


class Product(netweir.Item):
    title = netweir.css("h1::text")
    price = netweir.css("p.price_color::text", track="price")


def test_tracked_item_fields_survive_a_redesign_between_crawls(base, tmp_path, caplog):
    settings = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)

    def crawl():
        items = []

        class Shop(netweir.Spider):
            start_urls = [f"{base}/"]
            rules = [netweir.Follow("a.p", extract=Product)]
            pipelines = [lambda item: items.append(item) or item]

        Shop.settings = settings
        stats = Shop().run()
        return items, stats

    SITE["version"] = "before"
    items, stats = crawl()
    assert items == [{"title": "Blue Kettle", "price": "£24.99"}]
    assert stats["relocated"] == 0

    SITE["version"] = "after"
    with caplog.at_level(logging.WARNING, logger="netweir"):
        items, stats = crawl()
    assert items == [{"title": "Blue Kettle", "price": "£24.99"}]
    assert stats["relocated"] == 1
    assert "Product.price" in caplog.text


def test_item_extract_tracks_in_callbacks_as_well():
    assert Product.extract(netweir.parse(page("class-renamed.before.html")))["price"] == "£24.99"
    after = Product.extract(netweir.parse(page("class-renamed.after.html")))
    assert after["price"] == "£24.99"


def test_each_crawl_reports_tracking_again(base, caplog):
    settings = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)

    class Shop(netweir.Spider):
        start_urls = [f"{base}/"]
        rules = [netweir.Follow("a.p", extract=Product)]

    Shop.settings = settings
    SITE["version"] = "before"
    Shop().run()
    SITE["version"] = "after"
    for _ in range(2):
        caplog.clear()
        with caplog.at_level(logging.WARNING, logger="netweir"):
            Shop().run()
        assert "Product.price" in caplog.text
