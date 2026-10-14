import logging
from http.server import BaseHTTPRequestHandler

import pytest
from local_server import serve

import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    port = 0

    def log_message(self, *args):
        pass

    def do_GET(self):
        p = Handler.port
        if self.path == "/":
            links = [
                "/shop/a",
                "/shop/b?sort=price",
                "/blog/1",
                "/logout",
                f"http://localhost:{p}/elsewhere",
                f"http://localhost:{p}/elsewhere-too",
            ]
            body = "".join(f'<a href="{h}">x</a>' for h in links)
        else:
            body = f"<h1>{self.path}</h1>"
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    servers = serve(Handler)
    Handler.port = servers[0].server_port
    yield f"http://127.0.0.1:{servers[0].server_port}"
    for server in servers:
        server.shutdown()


def test_allowed_domains_keeps_a_spider_on_its_sites(base, caplog):
    seen = []

    class Home(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/"]
        allowed_domains = ["127.0.0.1"]

        def parse(self, page):
            seen.append(page.url)
            yield from page.follow_all(page.css("a::attr(href)"))

    with caplog.at_level(logging.WARNING, logger="netweir"):
        stats = Home().run()
    assert all(u.startswith(base) for u in seen)
    assert len(seen) == 5  # the home page and its four links on the site
    assert stats["offsite"] == 2
    # Said once for the site, not once per request.
    assert caplog.text.count("localhost") == 1


def test_dont_filter_goes_offsite_anyway(base):
    seen = []

    class Home(netweir.Spider):
        settings = FAST
        allowed_domains = ["127.0.0.1"]

        def start(self):
            yield netweir.Request(f"http://localhost:{Handler.port}/x", dont_filter=True)

        def parse(self, page):
            seen.append(page.url)

    Home().run()
    assert len(seen) == 1


def test_a_domain_allows_its_subdomains_and_a_port_only_that_port():
    from netweir._native import _offsite

    allowed = ["example.com", "shop.test:8080"]
    assert not _offsite("https://example.com/", allowed)
    assert not _offsite("https://www.example.com/", allowed)
    assert not _offsite("https://WWW.Example.COM/", allowed)
    assert _offsite("https://notexample.com/", allowed)
    assert _offsite("https://example.com.evil.org/", allowed)
    assert not _offsite("http://shop.test:8080/", allowed)
    assert _offsite("http://shop.test:9090/", allowed)


def test_a_url_in_allowed_domains_is_an_error():
    class Bad(netweir.Spider):
        settings = FAST
        allowed_domains = ["https://example.com/"]

    with pytest.raises(ValueError, match="domain"):
        Bad().run()


def test_rules_take_allow_and_deny_patterns_and_domains(base):
    pages = []

    class Rules(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/"]
        rules = [
            netweir.Follow(
                "a",
                allow=[r"/shop/", r"/blog/"],
                deny=[r"sort="],
                deny_domains=["localhost"],
                callback="page",
            )
        ]

        def page(self, page):
            pages.append(page.url.removeprefix(base))

        def parse(self, page):
            pass

    Rules().run()
    assert sorted(pages) == ["/blog/1", "/shop/a"]


def test_rules_stay_within_allowed_domains_too(base):
    pages = []

    class Rules(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/"]
        allowed_domains = ["127.0.0.1"]
        rules = [netweir.Follow("a", callback="page")]

        def page(self, page):
            pages.append(page.url)

        def parse(self, page):
            pass

    stats = Rules().run()
    assert all(u.startswith(base) for u in pages)
    assert stats["offsite"] == 2


def test_a_bad_pattern_is_an_error_up_front():
    with pytest.raises(ValueError, match="allow"):
        netweir.Follow("a", allow=["("])
