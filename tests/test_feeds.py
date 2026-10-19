"""XMLFeedSpider and CSVFeedSpider, as Scrapy documents them."""

import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_tdmrep=False)

RSS = b"""<?xml version="1.0"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Feed</title>
    <item id="1"><title>First</title><link>https://example.com/1</link>
      <dc:creator>Ann</dc:creator></item>
    <item id="2"><title>Second</title><link>https://example.com/2</link>
      <dc:creator>Bob</dc:creator></item>
  </channel>
</rss>"""

SITEMAP = b"""<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/a</loc></url>
  <url><loc>https://example.com/b</loc></url>
</urlset>"""

ATOM = b"""<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>One</title></entry>
  <entry><title>Two</title></entry>
</feed>"""

CSV = b'id,name,description\n1,Ann,first\n2,Bob,"second, with a comma"\n3,short\n4,Cy,"a line\nbreak"\n'
SEMI = b"'1';'Ann';'first'\n'2';'Bob';'it''s second'\n"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    pages: dict = {}

    def log_message(self, *args):
        pass

    def do_GET(self):
        body, ctype = self.pages.get(self.path, (b"", "text/plain"))
        self.send_response(200 if self.path in self.pages else 404)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    base = f"http://127.0.0.1:{server.server_port}"
    Handler.pages = {
        "/rss": (RSS, "application/rss+xml"),
        "/rss.txt": (RSS, "text/plain"),
        "/sitemap": (SITEMAP, "application/xml"),
        "/atom": (ATOM, "application/atom+xml"),
        "/broken": (b"<rss><item><title>x</title></item><item>", "application/xml"),
        "/feed.csv": (CSV, "text/csv"),
        "/semi.csv": (SEMI, "text/csv; charset=utf-8"),
        "/empty.csv": (b"", "text/csv"),
        "/next": (b"<h1>next</h1>", "text/html"),
    }
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield base
    server.shutdown()
    server.server_close()


def run(spider_cls):
    items = []

    class Collect(spider_cls):
        settings = FAST
        pipelines = [lambda item: items.append(item) or item]

    stats = Collect().run()
    return items, stats


@pytest.mark.parametrize("iterator", ["iternodes", "xml", "html"])
def test_each_item_goes_to_parse_node(base, iterator):
    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/rss"]

        def parse_node(self, page, node):
            return {
                "id": node.xpath("@id").get(),
                "title": node.xpath("title/text()").get(),
                "page": page.url,
            }

    Feed.iterator = iterator
    items, _ = run(Feed)
    assert sorted((i["id"], i["title"]) for i in items) == [("1", "First"), ("2", "Second")]
    assert {i["page"] for i in items} == {f"{base}/rss"}


def test_iternodes_and_xml_keep_link_text_html_loses_it(base):
    links = {}
    for iterator in ["iternodes", "xml", "html"]:

        class Feed(netweir.XMLFeedSpider):
            start_urls = [f"{base}/rss"]

            def parse_node(self, page, node):
                yield {"link": node.xpath("link/text()").get()}

        Feed.iterator = iterator
        items, _ = run(Feed)
        links[iterator] = sorted(str(i["link"]) for i in items)
    assert links["iternodes"] == links["xml"] == ["https://example.com/1", "https://example.com/2"]
    assert links["html"] == ["None", "None"]


@pytest.mark.parametrize("iterator", ["iternodes", "xml"])
def test_namespaces_are_registered_and_usable_in_itertag(base, iterator):
    class Sitemap(netweir.XMLFeedSpider):
        start_urls = [f"{base}/sitemap"]
        namespaces = [("n", "http://www.sitemaps.org/schemas/sitemap/0.9")]
        itertag = "n:url"

        def parse_node(self, page, node):
            return {"loc": node.xpath("n:loc/text()").get()}

    Sitemap.iterator = iterator
    items, _ = run(Sitemap)
    assert sorted(i["loc"] for i in items) == ["https://example.com/a", "https://example.com/b"]


def test_an_unprefixed_itertag_is_in_no_namespace(base):
    class Atom(netweir.XMLFeedSpider):
        start_urls = [f"{base}/atom"]
        itertag = "entry"

        def parse_node(self, page, node):
            return {"title": node.xpath("*/text()").get()}

    items, _ = run(Atom)
    assert items == []

    class Atom2(Atom):
        namespaces = [("atom", "http://www.w3.org/2005/Atom")]
        itertag = "atom:entry"

        def parse_node(self, page, node):
            return {"title": node.xpath("atom:title/text()").get()}

    items, _ = run(Atom2)
    assert sorted(i["title"] for i in items) == ["One", "Two"]


def test_the_iterator_reads_the_body_whatever_its_content_type(base):
    for iterator in ["iternodes", "xml"]:

        class Feed(netweir.XMLFeedSpider):
            start_urls = [f"{base}/rss.txt"]

            def parse_node(self, page, node):
                return {"creator": node.xpath("dc:creator/text()").get()}

        Feed.iterator = iterator
        Feed.namespaces = [("dc", "http://purl.org/dc/elements/1.1/")]
        items, _ = run(Feed)
        assert sorted(i["creator"] for i in items) == ["Ann", "Bob"], iterator


def test_parse_node_can_give_many_requests_or_nothing(base):
    seen = []

    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/rss"]

        async def parse_node(self, page, node):
            if node.xpath("@id").get() == "1":
                yield {"title": "one"}
                yield page.follow("/next", callback="nxt")

        def nxt(self, page):
            seen.append(page.css("h1::text").get())

    items, stats = run(Feed)
    assert items == [{"title": "one"}]
    assert seen == ["next"]


def test_adapt_response_and_process_results(base):
    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/rss"]

        def adapt_response(self, page):
            return page.replace(body=page.body.replace(b"First", b"Adapted"))

        def parse_node(self, page, node):
            return [{"title": node.xpath("title/text()").get()}, {"title": "extra"}]

        async def process_results(self, page, results):
            assert isinstance(results, list) and len(results) == 2
            return [dict(r, kept=True) for r in results if r["title"] != "extra"]

    items, _ = run(Feed)
    assert sorted(i["title"] for i in items) == ["Adapted", "Second"]
    assert all(i["kept"] for i in items)


def test_adapt_response_must_give_a_page(base, caplog):
    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/rss"]

        def adapt_response(self, page):
            return page.body

        def parse_node(self, page, node):
            return {"x": 1}

    with caplog.at_level(logging.ERROR, logger="netweir"):
        items, stats = run(Feed)
    assert items == []
    assert stats["callback_errors"] == 1
    assert "adapt_response must return a Page" in caplog.text


def test_without_parse_node_the_spider_says_so(base, caplog):
    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/rss"]

    with caplog.at_level(logging.ERROR, logger="netweir"):
        _, stats = run(Feed)
    assert stats["callback_errors"] == 1
    assert "needs a parse_node(self, page, node) method" in caplog.text


def test_an_unknown_iterator_is_an_error(base, caplog):
    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/rss"]
        iterator = "regex"

        def parse_node(self, page, node):
            return {"x": 1}

    with caplog.at_level(logging.ERROR, logger="netweir"):
        _, stats = run(Feed)
    assert stats["callback_errors"] == 1
    assert "unknown iterator 'regex'" in caplog.text


def test_a_broken_feed_gives_what_came_before_the_break(base, caplog):
    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/broken"]

        def parse_node(self, page, node):
            return {"title": node.xpath("title/text()").get()}

    with caplog.at_level(logging.ERROR, logger="netweir"):
        items, stats = run(Feed)
    assert items == [{"title": "x"}]
    assert stats["callback_errors"] == 1
    assert "not well-formed XML" in caplog.text


# --- CSV ---------------------------------------------------------------------------


def test_rows_are_dicts_keyed_by_the_first_row(base, caplog):
    class Rows(netweir.CSVFeedSpider):
        start_urls = [f"{base}/feed.csv"]

        def parse_row(self, page, row):
            return row

    with caplog.at_level(logging.WARNING, logger="netweir"):
        items, _ = run(Rows)
    assert items == [
        {"id": "1", "name": "Ann", "description": "first"},
        {"id": "2", "name": "Bob", "description": "second, with a comma"},
        {"id": "4", "name": "Cy", "description": "a line\nbreak"},
    ]
    assert "ignoring row 4 (length: 2, should be: 3)" in caplog.text


def test_delimiter_quotechar_and_headers(base):
    class Rows(netweir.CSVFeedSpider):
        start_urls = [f"{base}/semi.csv"]
        delimiter = ";"
        quotechar = "'"
        headers = ["id", "name", "description"]

        def parse_row(self, page, row):
            yield {"name": row["name"], "description": row["description"]}

    items, _ = run(Rows)
    assert items == [
        {"name": "Ann", "description": "first"},
        {"name": "Bob", "description": "it's second"},
    ]


def test_csv_adapt_response_and_process_results(base):
    class Rows(netweir.CSVFeedSpider):
        start_urls = [f"{base}/semi.csv"]
        delimiter = ";"
        quotechar = "'"
        headers = ["id", "name", "description"]

        async def adapt_response(self, page):
            return page.replace(body=page.text.replace("Ann", "Anne"))

        def parse_row(self, page, row):
            return {"name": row["name"]}

        def process_results(self, page, results):
            return [r for r in results if r["name"] != "Bob"]

    items, _ = run(Rows)
    assert items == [{"name": "Anne"}]


def test_an_empty_csv_gives_nothing(base):
    class Rows(netweir.CSVFeedSpider):
        start_urls = [f"{base}/empty.csv"]

        def parse_row(self, page, row):
            return row

    items, stats = run(Rows)
    assert items == [] and stats["callback_errors"] == 0


def test_without_parse_row_the_spider_says_so(base, caplog):
    class Rows(netweir.CSVFeedSpider):
        start_urls = [f"{base}/feed.csv"]

    with caplog.at_level(logging.ERROR, logger="netweir"):
        _, stats = run(Rows)
    assert stats["callback_errors"] == 1
    assert "needs a parse_row(self, page, row) method" in caplog.text


def test_a_feed_spiders_other_callbacks_work_as_a_spiders(base):

    class Feed(netweir.XMLFeedSpider):
        start_urls = [f"{base}/rss"]

        def parse_node(self, page, node):
            return page.follow("/next", callback="nxt")

        def nxt(self, page):
            return {"h1": page.css("h1::text").get()}

    items, stats = run(Feed)
    # Both items' requests go to one URL, crawled once.
    assert items == [{"h1": "next"}]
