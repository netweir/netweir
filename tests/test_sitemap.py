import gzip
import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_tdmrep=False)


def urlset(*locs, alternates=()):
    rows = []
    for loc in locs:
        alt = "".join(f'<xhtml:link rel="alternate" hreflang="de" href="{a}"/>' for a in alternates)
        rows.append(f"<url><loc>{loc}</loc><lastmod>2026-10-0{len(rows) + 1}</lastmod>{alt}</url>")
    return (
        '<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"'
        ' xmlns:xhtml="http://www.w3.org/1999/xhtml">' + "".join(rows) + "</urlset>"
    ).encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    pages: dict[str, tuple[bytes, str]] = {}

    def log_message(self, *args):
        pass

    def do_GET(self):
        body, ctype = self.pages.get(self.path, (b"<h1>not here</h1>", "text/html"))
        status = 200 if self.path in self.pages else 404
        if self.path.startswith(("/product/", "/blog/", "/de/")):
            body, ctype, status = f"<h1>{self.path}</h1>".encode(), "text/html", 200
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    base = f"http://127.0.0.1:{server.server_port}"
    Handler.pages = {
        "/robots.txt": (
            f"User-agent: *\nAllow: /\nSitemap: {base}/sitemap.xml\n".encode(),
            "text/plain",
        ),
        "/sitemap.xml": (
            (
                '<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">'
                f"<sitemap><loc>{base}/products.xml.gz</loc></sitemap>"
                f"<sitemap><loc>{base}/blog.xml</loc></sitemap>"
                "</sitemapindex>"
            ).encode(),
            "application/xml",
        ),
        "/products.xml.gz": (
            gzip.compress(urlset(f"{base}/product/1", f"{base}/product/2")),
            "application/gzip",
        ),
        "/blog.xml": (
            urlset(f"{base}/blog/hello", alternates=[f"{base}/de/hallo"]),
            "application/xml",
        ),
        "/urls.txt": (f"{base}/product/9\n{base}/blog/text\n".encode(), "text/plain"),
        "/broken.xml": (b"<urlset><url><loc>", "application/xml"),
    }
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield base
    server.shutdown()
    server.server_close()


def crawl(spider_cls):
    seen = []

    class S(spider_cls):
        settings = FAST

        def product(self, page):
            seen.append(("product", page.css("h1::text").get()))

        def post(self, page):
            seen.append(("post", page.css("h1::text").get()))

        def parse(self, page):
            seen.append(("parse", page.css("h1::text").get()))

    stats = S().run()
    return sorted(seen), stats


def test_a_spider_crawls_a_site_from_its_robots_txt_sitemaps(base):
    class Shop(netweir.Spider):
        sitemap_urls = [f"{base}/robots.txt"]
        sitemap_rules = [("/product/", "product"), ("/blog/", "post")]

    seen, _ = crawl(Shop)
    assert seen == [
        ("post", "/blog/hello"),
        ("product", "/product/1"),
        ("product", "/product/2"),
    ]


def test_only_the_sitemaps_follow_names_are_read(base):
    class Shop(netweir.Spider):
        sitemap_urls = [f"{base}/sitemap.xml"]
        sitemap_follow = ["/products"]
        sitemap_rules = [("/product/", "product"), ("/blog/", "post")]

    seen, _ = crawl(Shop)
    assert [kind for kind, _ in seen] == ["product", "product"]


def test_pages_no_rule_matches_are_skipped(base):
    class Shop(netweir.Spider):
        sitemap_urls = [f"{base}/sitemap.xml"]
        sitemap_rules = [("/blog/", "post")]

    seen, _ = crawl(Shop)
    assert seen == [("post", "/blog/hello")]


def test_alternate_links_come_along_when_asked(base):
    class Blog(netweir.Spider):
        sitemap_urls = [f"{base}/blog.xml"]
        sitemap_alternate_links = True

    seen, _ = crawl(Blog)
    assert seen == [("parse", "/blog/hello"), ("parse", "/de/hallo")]


def test_a_filter_chooses_entries_by_what_the_sitemap_says(base):
    class Recent(netweir.Spider):
        sitemap_urls = [f"{base}/products.xml.gz"]

        def sitemap_filter(self, entries):
            for entry in entries:
                if entry["lastmod"] >= "2026-10-02":
                    yield entry

    seen, _ = crawl(Recent)
    assert seen == [("parse", "/product/2")]


def test_a_text_sitemap_and_a_function_callback(base):
    found = []

    class Text(netweir.Spider):
        settings = FAST
        sitemap_urls = [f"{base}/urls.txt"]
        sitemap_rules = [("/product/", lambda page: found.append(page.url))]

        def parse(self, page):
            pass

    Text().run()
    assert found == [f"{base}/product/9"]


def test_a_broken_sitemap_is_logged_and_skipped(base, caplog):
    class Broken(netweir.Spider):
        sitemap_urls = [f"{base}/broken.xml"]

    with caplog.at_level(logging.WARNING, logger="netweir"):
        seen, stats = crawl(Broken)
    assert seen == []
    assert "broken.xml" in caplog.text and "sitemap" in caplog.text
    assert stats["callback_errors"] == 0
