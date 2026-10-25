"""Declarative spiders: Items and rules, extracted and followed in Rust."""

import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

PAGES = 3
PER_PAGE = 4


def listing(n):
    books = "".join(
        f'<article class="product_pod"><h3><a href="../book/{n}-{i}">B{n}-{i}</a></h3></article>'
        for i in range(PER_PAGE)
    )
    nxt = f'<li class="next"><a href="/page/{n + 1}">next</a></li>' if n < PAGES else ""
    return f"<html><body>{books}<ul class='pager'>{nxt}</ul></body></html>"


def detail(name):
    n, i = name.split("-")
    stock = "In stock (7 available)" if i != "0" else "Out of stock"
    return (
        f"<html><head><base href='/page/'></head><body><h1>  Book {name}  </h1>"
        f"<p class='price_color'>£{n}.{i}5</p><p class='stock'>{stock}</p>"
        f"<table><tr><th>UPC</th><td>upc-{name}</td></tr></table>"
        f"<a class='tag'>t1</a><a class='tag'>t2</a>"
        f"<a class='back' href='1'>back</a>"
        f"<ul><li class='next'><a href='99'>more</a></li></ul></body></html>"
    )


ODD = (
    "<a class='o' href='/gone'>404</a><a class='o' href='/file.pdf'>pdf</a>"
    "<a class='o' href='/book/1-1'>a</a><a class='o' href='/book/1-1'>again</a>"
    "<a class='o' href='/book/1-1#x'>fragment</a>"
)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    log: list[str] = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        Handler.log.append(self.path)
        p = self.path
        if p == "/robots.txt":
            body, status = "User-agent: *\nDisallow: /book/3-3\n", 200
        elif p.startswith("/page/"):
            body, status = listing(int(p.rsplit("/", 1)[1])), 200
        elif p.startswith("/book/"):
            body, status = detail(p.rsplit("/", 1)[1]), 200
        elif p == "/odd":
            body, status = ODD, 200
        elif p == "/gone":
            body, status = "<h1>Gone</h1><p class='price_color'>£0.00</p>", 404
        elif p == "/file.pdf":
            body, status = "%PDF-1.4 <h1>pdf title</h1>", 200
        else:
            body, status = "missing", 404
        data = body.encode()
        self.send_response(status)
        ctype = "application/pdf" if p.endswith(".pdf") else "text/html; charset=utf-8"
        self.send_header("Content-Type", ctype)
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


FAST = netweir.Settings(throttle=False, start_delay=0, obey_tdmrep=False)


class Book(netweir.Item):
    title = netweir.css("h1::text", strip=True)
    price = netweir.css(".price_color::text", re=r"[\d.]+", into=float)
    stock = netweir.css(".stock::text", re=r"(\d+) available", into=int, default=0)
    upc = netweir.xpath("//th[.='UPC']/following-sibling::td/text()")
    tags = netweir.css(".tag::text", all=True)


def books_spider(base, **attrs):
    attrs.setdefault("settings", FAST)
    attrs.setdefault(
        "rules",
        [
            netweir.Follow("li.next a"),
            netweir.Follow("article.product_pod h3 a", extract=Book),
        ],
    )
    attrs.setdefault("start_urls", [f"{base}/page/1"])
    return type("Books", (netweir.Spider,), attrs)()


def collect(spider):
    items = []
    spider.pipelines = [lambda item: items.append(item) or item]
    stats = spider.run()
    return items, stats


def test_a_declarative_spider_crawls_and_extracts(base):
    items, stats = collect(books_spider(base))
    # 3 pages x 4 books, less the one robots.txt keeps out.
    assert len(items) == PAGES * PER_PAGE - 1
    assert stats["items"] == len(items)
    assert stats["dropped_robots"] == 1
    book = next(i for i in items if i["upc"] == "upc-2-1")
    assert book == {
        "title": "Book 2-1",
        "price": 2.15,
        "stock": 7,
        "upc": "upc-2-1",
        "tags": ["t1", "t2"],
    }
    assert list(book) == ["title", "price", "stock", "upc", "tags"], "fields keep their order"
    out_of_stock = next(i for i in items if i["upc"] == "upc-1-0")
    assert out_of_stock["stock"] == 0, "default for a missing value"


def test_no_python_runs_per_page(base, monkeypatch):
    def no_pages(*args, **kwargs):
        raise AssertionError("a Page was built for a declarative spider")

    monkeypatch.setattr(netweir._crawl, "Page", no_pages)
    items, stats = collect(books_spider(base))
    assert stats["callback_errors"] == 0
    assert len(items) == PAGES * PER_PAGE - 1


def test_a_rule_that_extracts_does_not_follow_by_default(base):
    Handler.log.clear()
    collect(books_spider(base))
    # Book pages have a "next" link to /page/99 (through <base href>); the
    # rules are not applied on book pages, so it is never fetched.
    assert Handler.log.count("/page/1") == 1
    assert "/page/99" not in Handler.log


def test_follow_true_on_an_extracting_rule_follows_its_links(base):
    spider = books_spider(
        base,
        start_urls=[f"{base}/book/2-1"],
        rules=[
            netweir.Follow("a.back", extract=Book, follow=True),
            netweir.Follow("li.next a"),
        ],
    )
    Handler.log.clear()
    items, _ = collect(spider)
    # The start page is not reached through a rule, so it gives no item; its
    # back link (resolved against <base href='/page/'>) is /page/1.
    assert "/page/1" in Handler.log
    assert items and all("title" in i for i in items)


def test_conversion_failures_are_warned_once_per_field(base, caplog):
    class Strict(netweir.Item):
        price = netweir.css(".price_color::text", into=float)

    spider = books_spider(base, rules=[netweir.Follow("article.product_pod h3 a", extract=Strict)])
    with caplog.at_level(logging.WARNING, logger="netweir"):
        items, _ = collect(spider)
    assert items and all(i["price"] is None for i in items)
    warnings = [r.message for r in caplog.records if "Strict.price" in r.message]
    assert len(warnings) == 1
    assert "£" in warnings[0], "the text that would not convert is shown"


def test_python_converters_run_on_the_finished_item(base):
    class Money(netweir.Item):
        price = netweir.css(".price_color::text", into=lambda s: s.replace("£", "GBP "))

    spider = books_spider(base, rules=[netweir.Follow("article.product_pod h3 a", extract=Money)])
    items, _ = collect(spider)
    assert all(i["price"].startswith("GBP ") for i in items)


def test_a_rule_can_hand_pages_to_a_callback(base):
    seen = []

    async def book(self, page):
        seen.append(page.url)
        yield {"from_callback": page.css("h1::text").get().strip(), **Book.extract(page)}

    spider = books_spider(
        base,
        rules=[
            netweir.Follow("li.next a"),
            netweir.Follow("article.product_pod h3 a", callback="book"),
        ],
        book=book,
    )
    items, _ = collect(spider)
    assert len(seen) == len(items) == PAGES * PER_PAGE - 1
    assert all(i["from_callback"] == i["title"] for i in items)


def test_rules_and_parse_combine(base):
    parsed = []

    async def parse(self, page):
        parsed.append(page.url)
        yield {"listing": page.url}

    spider = books_spider(base, parse=parse)
    items, _ = collect(spider)
    # parse runs on the start page only; the rules do the rest.
    assert parsed == [f"{base}/page/1"]
    assert sum("listing" in i for i in items) == 1
    assert sum("upc" in i for i in items) == PAGES * PER_PAGE - 1


def test_item_extract_works_on_any_node():
    doc = netweir.parse(detail("5-2"))
    assert Book.extract(doc) == {
        "title": "Book 5-2",
        "price": 5.25,
        "stock": 7,
        "upc": "upc-5-2",
        "tags": ["t1", "t2"],
    }


def test_bad_rules_and_fields_fail_up_front():
    with pytest.raises(netweir.SelectorError):

        class Bad(netweir.Item):
            x = netweir.css("p[")

        Bad.extract(netweir.parse("<p>"))
    with pytest.raises(ValueError, match="regular expression"):
        netweir.css("p::text", re="(")
    with pytest.raises(TypeError):
        netweir.Follow()
    with pytest.raises(TypeError):
        netweir.Follow("a", xpath="//a")


def test_error_pages_and_files_give_no_items(base, caplog):
    spider = books_spider(
        base,
        start_urls=[f"{base}/odd"],
        rules=[netweir.Follow("a.o", extract=Book)],
    )
    with caplog.at_level(logging.INFO, logger="netweir"):
        items, stats = collect(spider)
    assert [i["upc"] for i in items] == ["upc-1-1"]
    assert "404" in caplog.text and "application/pdf" in caplog.text
    # The same link three ways is one request, not two duplicates.
    assert stats["duplicates"] == 0


def test_warnings_name_the_item_class_fully(caplog):
    # Module and qualified name, so two Items called I in different places
    # don't share (and silence) one warning.
    def make():
        class I(netweir.Item):  # noqa: E742
            v = netweir.css(".price_color::text", into=int)

        return I

    with caplog.at_level(logging.WARNING, logger="netweir"):
        make().extract(netweir.parse(detail("1-1")))
    assert "test_rules.test_warnings_name_the_item_class_fully.<locals>.make.<locals>.I.v" in (
        caplog.text
    )


def test_a_query_that_fails_says_so(caplog):
    class Counted(netweir.Item):
        n = netweir.xpath("count(1)")

    with caplog.at_level(logging.WARNING, logger="netweir"):
        assert Counted.extract(netweir.parse("<p>")) == {"n": None}
    assert "query failed" in caplog.text and "couldn't convert" not in caplog.text


def test_mistakes_in_items_and_rules_fail_where_written():
    with pytest.raises(TypeError, match="into"):
        netweir.css("p", into=object())
    with pytest.raises(TypeError, match="priority"):
        netweir.Follow("a", priority="high")
    with pytest.raises(TypeError, match="extract"):

        class Clash(netweir.Item):
            extract = netweir.css("p")


def test_a_pattern_group_that_did_not_take_part_is_empty_like_parsel():
    class G(netweir.Item):
        g = netweir.css("p::text", re=r"x(y)?")

    assert G.extract(netweir.parse("<p>x</p>")) == {"g": ""}


def test_rules_respect_max_depth(base):
    spider = books_spider(
        base,
        settings=netweir.Settings(throttle=False, start_delay=0, obey_tdmrep=False, max_depth=1),
    )
    items, stats = collect(spider)
    # Page 1 (depth 0) and its books and page 2 (depth 1); page 2's links
    # would be depth 2.
    assert len(items) == PER_PAGE
    assert stats["skipped_depth"] > 0


def test_markdown_field_in_an_item():
    class Doc(netweir.Item):
        body = netweir.markdown("article")

    root = netweir.parse('<article><h2>Hi</h2><p><a href="/x">x</a></p></article>')
    assert Doc.extract(root, url="https://a.example/p") == {
        "body": "## Hi\n\n[x](https://a.example/x)"
    }


def test_clean_text_field_reads_the_whole_page():
    class Doc(netweir.Item):
        text = netweir.clean_text()

    root = netweir.parse("<h1>T</h1><p>a <b>b</b></p><script>x()</script>")
    assert Doc.extract(root) == {"text": "T\n\na b"}


def test_main_only_without_main_content_gives_the_default():
    class Doc(netweir.Item):
        body = netweir.markdown(main_only=True, default="none")

    assert Doc.extract(netweir.parse("<p>hi</p>")) == {"body": "none"}


def test_markdown_fields_need_elements():
    with pytest.raises(ValueError, match="element"):
        netweir.markdown("a::attr(href)")


class BackLink(netweir.Item):
    back = netweir.markdown("a.back")


def test_markdown_field_in_a_declarative_crawl(base):
    spider = books_spider(
        base,
        rules=[netweir.Follow("article.product_pod h3 a", extract=BackLink)],
        start_urls=[f"{base}/page/1"],
    )
    items, _ = collect(spider)
    # The page's <base href='/page/'> resolves the link, as a browser would.
    assert items and all(i == {"back": f"[back]({base}/page/1)"} for i in items)
