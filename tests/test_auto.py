"""Auto-extraction from Python: typed items, pinned fields and crawls."""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import netweir
import netweir._track
from netweir._native import _response

FIXTURES = Path(__file__).parent.parent / "crates/netweir-dom/tests/fixtures/auto"


def fixture(name):
    return (FIXTURES / f"{name}.html").read_text(encoding="utf-8")


def page(html, url="https://shop.example/p/arc-lamp"):
    return netweir.Page(
        _response(url, 200, "HTTP/1.1", [("content-type", "text/html")], html.encode())
    )


@pytest.fixture(autouse=True)
def tracks_home(tmp_path, monkeypatch):
    monkeypatch.setenv("NETWEIR_HOME", str(tmp_path / "home"))
    netweir._track._reset()
    yield
    netweir._track._reset()


def test_product_from_json_ld():
    item = page(fixture("product-jsonld")).extract(netweir.auto.Product)
    assert item["name"] == "Arc desk lamp"
    assert item["price"] == "49.99"
    assert item["currency"] == "GBP"
    assert item["brand"] == {"name": "Lumo"}
    assert item["url"] == "https://shop.example/p/arc-lamp"
    assert list(item)[:3] == ["url", "canonical_url", "name"], "the schema's order"
    assert list(item)[-1] == "metadata"
    meta = item["metadata"]
    assert meta["probability"] > 0.9
    assert meta["sources"]["name"] == "json-ld"
    assert meta["validation_messages"] == {}
    assert meta["date_downloaded"].endswith("Z") and meta["date_downloaded"][:2] == "20"
    json.dumps(item)  # plain JSON, nothing to convert


SALE = """<html><head><script type="application/ld+json">
{"@type": "Product", "name": "Desk lamp", "offers": {"price": "12.00", "priceCurrency": "USD"}}
</script></head><body><h1>Desk lamp</h1><p><span class="sale">$9.50</span> <s>$12.00</s></p>
</body></html>"""


class Lamps(netweir.auto.Product):
    price = netweir.css("span.sale")


def test_override_one_field_keeps_the_rest():
    item = Lamps.extract(page(SALE))
    assert item["price"] == "9.50"
    assert item["name"] == "Desk lamp", "still automatic"
    assert item["currency"] == "USD", "pinning price pins price alone"
    assert item["metadata"]["sources"]["price"] == "css:span.sale"
    assert item["metadata"]["sources"]["name"] == "json-ld"


def test_a_pinned_field_reads_text_too():
    class ByText(netweir.auto.Product):
        price = netweir.css("span.sale::text")
        regular_price = netweir.css("s::text")

    item = ByText.extract(page(SALE))
    assert (item["price"], item["regular_price"]) == ("9.50", "12.00")


def test_pinned_field_that_misses_is_none_with_a_note():
    class Wrong(netweir.auto.Product):
        price = netweir.css("span.gone")

    item = Wrong.extract(page(SALE))
    assert item["price"] is None, "never silently automatic"
    assert item["metadata"]["validation_messages"]["price"] == ["css:span.gone matched nothing"]


def test_extra_field_is_kept_after_the_schema():
    class WithWarranty(netweir.auto.Product):
        warranty = netweir.css("p.warranty::text", default="none")

    item = WithWarranty.extract(page(SALE))
    keys = list(item)
    assert keys[-2:] == ["warranty", "metadata"]
    assert item["warranty"] == "none"


def test_into_on_a_pinned_field_replaces_the_reader():
    class Upper(netweir.auto.Product):
        name = netweir.css("h1::text", into=str.upper)

    assert Upper.extract(page(SALE))["name"] == "DESK LAMP"


def test_a_field_name_the_schema_has_cannot_be_a_typo():
    with pytest.raises(TypeError, match="metadata"):

        class Bad(netweir.auto.Product):
            metadata = netweir.css("p")


def test_extract_on_a_node_takes_the_url():
    item = netweir.auto.Product.extract(
        netweir.parse(fixture("product-microdata")), url="https://shop.example/p/apron"
    )
    assert item["main_image"] == {"url": "https://shop.example/p/apron.jpg"}
    assert item["metadata"]["sources"]["name"] == "microdata:h1"


def test_zyte_field_names_on_export(tmp_path):
    item = page(fixture("product-jsonld")).extract(netweir.auto.Product)
    out = tmp_path / "items.jsonl"
    write = netweir.export.jsonl(str(out), field_names="zyte")
    write(item)
    write.close()
    row = json.loads(out.read_text())
    assert row["regularPrice"] == "59.99"
    assert row["mainImage"] == {"url": "https://shop.example/img/arc-1.jpg"}
    assert row["aggregateRating"] == {"ratingValue": 4.6, "bestRating": 5.0, "reviewCount": 128}
    assert "dateDownloaded" in row["metadata"] and "validationMessages" in row["metadata"]
    assert "regular_price" not in row


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    version = "before"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/":
            body = '<a class="p" href="/p/lamp">lamp</a>'
        elif self.path == "/p/lamp":
            sale = (
                '<span class="sale">$9.50</span>'
                if Handler.version == "before"
                else '<span class="offer-now">$9.50</span>'
            )
            body = SALE.replace('<span class="sale">$9.50</span>', sale)
        else:
            body = "missing"
        data = body.encode()
        self.send_response(200 if body != "missing" else 404)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


class TrackedLamps(netweir.auto.Product):
    price = netweir.css("span.sale", track="price")


def crawl(base, item):
    items = []

    class Shop(netweir.Spider):
        start_urls = [f"{base}/"]
        rules = [netweir.Follow("a.p", extract=item)]
        pipelines = [lambda i: items.append(i) or i]
        settings = netweir.Settings(
            throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False
        )

    Shop().run()
    return items


def test_auto_item_in_a_declarative_crawl(base, monkeypatch):
    monkeypatch.setattr(Handler, "version", "before")
    [item] = crawl(base, Lamps)
    assert item["url"] == f"{base}/p/lamp"
    assert (item["name"], item["price"], item["currency"]) == ("Desk lamp", "9.50", "USD")
    assert item["metadata"]["sources"]["price"] == "css:span.sale"


def test_tracked_override_survives_a_redesign(base, monkeypatch):
    monkeypatch.setattr(Handler, "version", "before")
    assert crawl(base, TrackedLamps)[0]["price"] == "9.50"
    monkeypatch.setattr(Handler, "version", "after")
    [item] = crawl(base, TrackedLamps)
    assert item["price"] == "9.50", "self-healing found the renamed element"


def test_field_names_are_checked():
    with pytest.raises(ValueError, match="snake"):
        netweir.export.jsonl("x.jsonl", field_names="camel")
