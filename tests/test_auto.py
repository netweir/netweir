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


def test_job_posting():
    item = page(fixture("job-jsonld"), "https://jobs.example/j/1042").extract(
        netweir.auto.JobPosting
    )
    assert item["job_title"] == "Senior Rust Engineer"
    assert item["base_salary"]["value_min"] == "70000"
    assert item["metadata"]["probability"] > 0.9


def test_auto_article_leaves_netweir_article_alone():
    p = page(fixture("article-jsonld"), "https://paper.example/2026/rivers-rise")
    item = p.extract(netweir.auto.Article)
    assert item["headline"] == "Rivers rise across the north"
    assert item["authors"][0]["name"] == "Ann Lee"
    old = p.article()
    assert isinstance(old, netweir.Article)
    assert old.title == "Rivers rise across the north"
    assert set(vars(old)) == {"title", "byline", "published", "image", "language", "text", "html"}


def test_product_list_and_navigation():
    p = page(fixture("list-itemlist"), "https://shop.example/c/lamps")
    listing = p.extract(netweir.auto.ProductList)
    assert [x["name"] for x in listing["products"]] == ["Arc lamp", "Dome lamp", "Cone lamp"]
    assert listing["pagination_next"] == {"url": "https://shop.example/c/lamps?page=2"}
    nav = p.extract(netweir.auto.ProductNavigation)
    assert nav["items"][0] == {"url": "https://shop.example/p/arc", "name": "Arc lamp"}
    assert nav["next_page"] == {"url": "https://shop.example/c/lamps?page=2"}


def test_extract_command_prints_the_item(capsys):
    from netweir import _cli

    assert _cli.main(["extract", str(FIXTURES / "product-jsonld.html")]) == 0
    item = json.loads(capsys.readouterr().out)
    assert item["name"] == "Arc desk lamp"
    assert item["metadata"]["sources"]["name"] == "json-ld"


def test_extract_command_takes_a_type_and_zyte_names(capsys):
    from netweir import _cli

    target = str(FIXTURES / "job-jsonld.html")
    assert _cli.main(["extract", "--type", "job-posting", "--field-names", "zyte", target]) == 0
    item = json.loads(capsys.readouterr().out)
    assert item["jobTitle"] == "Senior Rust Engineer"
    assert item["baseSalary"]["valueMin"] == "70000"


def test_extract_command_rejects_an_unknown_type(capsys):
    from netweir import _cli

    with pytest.raises(SystemExit):
        _cli.main(["extract", "--type", "recipe", "x.html"])


def test_re_on_a_pinned_field_picks_the_text_to_read():
    html = SALE.replace(
        '<span class="sale">$9.50</span>', '<span class="p">was $20.00 now $9.50</span>'
    )

    class ByText(netweir.auto.Product):
        price = netweir.css("span.p::text", re=r"now (\S+)")

    class ByElement(netweir.auto.Product):
        price = netweir.css("span.p", re=r"now (\S+)")

    assert ByText.extract(page(html))["price"] == "9.50"
    assert ByElement.extract(page(html))["price"] == "9.50"


SHAPES = """<html><body><span class="org">Acme Ltd</span><span class="loc">Leeds, UK</span>
<p class="by"><a href="/a">Ann Lee</a> and <a href="/r">Raj Patel</a></p>
<a class="next" href="/c?page=3">Next page</a><span class="pg">Page 2 of 9</span>
<video class="v" src="/v.mp4"></video></body></html>"""


def test_pinned_structured_fields_keep_the_schemas_shape():
    class Job(netweir.auto.JobPosting):
        hiring_organization = netweir.css("span.org")
        job_location = netweir.css("span.loc")

    class Story(netweir.auto.Article):
        authors = netweir.css("p.by")
        videos = netweir.css("video.v")

    class Listing(netweir.auto.ProductList):
        pagination_next = netweir.css("a.next")
        page_number = netweir.css("span.pg::text")

    class Nav(netweir.auto.ProductNavigation):
        next_page = netweir.css("a.next")

    p = page(SHAPES, "https://shop.example/c")
    job = Job.extract(p)
    assert job["hiring_organization"] == {"name": "Acme Ltd", "name_raw": "Acme Ltd"}
    assert job["job_location"] == {"raw": "Leeds, UK"}
    story = Story.extract(p)
    assert story["authors"] == [
        {"name": "Ann Lee", "name_raw": "Ann Lee", "url": "https://shop.example/a"},
        {"name": "Raj Patel", "name_raw": "Raj Patel", "url": "https://shop.example/r"},
    ]
    assert story["videos"] == [{"url": "https://shop.example/v.mp4"}]
    listing = Listing.extract(p)
    assert listing["pagination_next"] == {
        "url": "https://shop.example/c?page=3",
        "text": "Next page",
    }
    assert listing["page_number"] == 2
    assert Nav.extract(p)["next_page"] == {
        "url": "https://shop.example/c?page=3",
        "name": "Next page",
    }


def test_fields_that_no_single_element_holds_cannot_be_pinned():
    with pytest.raises(ValueError, match="products"):

        class Bad(netweir.auto.ProductList):
            products = netweir.css("li.product")

    class Fine(netweir.auto.ProductList):
        products = netweir.css("li.product::text", all=True, into=str)


def test_unreadable_values_in_a_pinned_list_are_noted():
    html = SALE.replace(
        '<span class="sale">$9.50</span>', '<i class="p">free</i><i class="p">$2</i>'
    )

    class Many(netweir.auto.Product):
        price = netweir.css("i.p", all=True)

    item = Many.extract(page(html))
    assert item["price"] == [None, "2"]
    assert item["metadata"]["validation_messages"]["price"] == [
        "css:i.p found 'free', which doesn't read as price"
    ]


def test_a_long_unreadable_text_is_shortened_in_the_note():
    html = SALE.replace('<span class="sale">$9.50</span>', f'<div class="p">{"word " * 200}</div>')

    class Big(netweir.auto.Product):
        price = netweir.css("div.p")

    [note] = Big.extract(page(html))["metadata"]["validation_messages"]["price"]
    assert len(note) < 200 and "…" in note


def test_csv_columns_follow_zyte_field_names(tmp_path):
    out = tmp_path / "items.csv"
    write = netweir.export.csv(str(out), fields=["name", "regular_price"], field_names="zyte")
    write({"name": "Lamp", "regular_price": "59.99"})
    write.close()
    assert out.read_text().splitlines() == ["name,regularPrice", "Lamp,59.99"]


def test_a_page_without_structured_data_is_read_from_its_layout():
    item = page(fixture("product-dom"), url="https://lumo.example/p/arc-lamp").extract(
        netweir.auto.Product
    )
    assert (item["name"], item["price"], item["currency"]) == ("Arc desk lamp", "49.99", "GBP")
    assert item["sku"] == "ARC-01"
    sources = item["metadata"]["sources"]
    assert sources["price"] == "dom:span"
    assert 0.6 < item["metadata"]["probability"] < 0.8
    # A layout field pins like any other, and its siblings stay automatic.

    class Lumo(netweir.auto.Product):
        sku = netweir.css("dd:nth-of-type(2)")

    pinned = page(fixture("product-dom"), url="https://lumo.example/p/arc-lamp").extract(Lumo)
    assert pinned["sku"] == "A100"
    assert pinned["price"] == "49.99"


def test_page_type_tells_what_a_page_holds():
    for name, kind in [
        ("product-jsonld", "product"),
        ("product-dom", "product"),
        ("list-dom", "product-list"),
        ("list-itemlist", "product-list"),
        ("job-jsonld", "job-posting"),
        ("article-jsonld", "article"),
        ("no-data", "other"),
    ]:
        assert page(fixture(name)).page_type() == kind, name
    root = netweir.parse(fixture("job-dom"))
    assert root.page_type(url="https://jobs.example/jobs/4411") == "job-posting"


def test_extract_command_reads_the_type_the_page_has(capsys):
    from netweir import _cli

    assert _cli.main(["extract", str(FIXTURES / "job-jsonld.html")]) == 0
    assert json.loads(capsys.readouterr().out)["job_title"] == "Senior Rust Engineer"
    assert _cli.main(["extract", str(FIXTURES / "no-data.html")]) == 0
    out = capsys.readouterr()
    assert "doesn't look like" in out.err
    assert "name" in json.loads(out.out)
