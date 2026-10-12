"""netweir.scrapy.DownloadHandler: Scrapy fetching with netweir. Each test
runs a real Scrapy crawl in a process of its own (Scrapy's reactor can't
start twice in one process)."""

import base64
import gzip
import json
import subprocess
import sys
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

pytest.importorskip("scrapy")


#: Where each /redirect-to/ path sends the client.
REDIRECTS = {
    "/redirect-to/%2Flanded%3Fa%3D1": "/landed?a=1",
    "/redirect-to/%2Fwith%20space": "/with space",
    "/redirect-to/%2Fsecond": "/second",
}


class Site(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    seen: list = []

    def log_message(self, *args):
        pass

    def reply(self, status, body=b"", headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def echo(self, extra=None):
        Site.seen.append((self.command, self.path))
        body = json.dumps(
            {
                "path": self.path,
                "method": self.command,
                "headers": [[k.lower(), v] for k, v in self.headers.items()],
                **(extra or {}),
            }
        ).encode()
        self.reply(200, body, [("Content-Type", "application/json")])

    def do_GET(self):
        if self.path in REDIRECTS:
            Site.seen.append((self.command, self.path))
            # Fixed targets: nothing from the request reaches the header.
            self.reply(302, headers=[("Location", REDIRECTS[self.path])])
        elif self.path == "/redirect":
            Site.seen.append((self.command, self.path))
            self.reply(302, headers=[("Location", "/landed")])
        elif self.path == "/set-cookie":
            Site.seen.append((self.command, self.path))
            self.reply(
                200,
                b"{}",
                [("Set-Cookie", "session=abc; Path=/"), ("Content-Type", "application/json")],
            )
        elif self.path == "/slow":
            time.sleep(3)
            self.reply(200, b"{}")
        elif self.path == "/big":
            self.reply(200, b"x" * 200_000, [("Content-Type", "text/plain")])
        elif self.path == "/two-cookies":
            self.reply(
                200,
                b"{}",
                [
                    ("Set-Cookie", "a=1"),
                    ("Set-Cookie", "b=2"),
                    ("Content-Type", "application/json"),
                ],
            )
        elif self.path == "/gzip":
            body = gzip.compress(b'{"compressed": true}')
            self.reply(
                200, body, [("Content-Type", "application/json"), ("Content-Encoding", "gzip")]
            )
        else:
            self.echo()

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length).decode()
        if self.path == "/login":
            self.reply(303, headers=[("Location", "/landed")])
            return
        self.echo({"body": body})

    def do_HEAD(self):
        Site.seen.append((self.command, self.path))
        self.send_response(200)
        self.send_header("X-Head", "yes")
        self.send_header("Content-Length", "0")
        self.end_headers()


class Proxy(BaseHTTPRequestHandler):
    """A forward proxy that answers for any URL and records its login."""

    protocol_version = "HTTP/1.1"
    logins: list = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        Proxy.logins.append(self.headers.get("Proxy-Authorization"))
        body = json.dumps({"via": "proxy", "url": self.path}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def serve(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_port}"


@pytest.fixture(scope="module")
def base():
    server, url = serve(Site)
    yield url
    server.shutdown()


def crawl(tmp_path, spider_body, settings=None, env=None):
    """Runs a Scrapy spider whose parse() yields dicts, with netweir's
    handler, and returns its items."""
    out = tmp_path / "items.json"
    script = tmp_path / "run.py"
    settings = {
        "DOWNLOAD_HANDLERS": {
            "http": "netweir.scrapy.DownloadHandler",
            "https": "netweir.scrapy.DownloadHandler",
        },
        "LOG_LEVEL": "ERROR",
        "ROBOTSTXT_OBEY": False,
        "TELNETCONSOLE_ENABLED": False,
        "FEEDS": {str(out): {"format": "json"}},
        **(settings or {}),
    }
    script.write_text(
        "import json, scrapy\n"
        "from scrapy.crawler import CrawlerProcess\n"
        "class Spider(scrapy.Spider):\n"
        "    name = 'test'\n"
        + textwrap.indent(textwrap.dedent(spider_body), "    ")
        + f"\nprocess = CrawlerProcess({settings!r})\n"
        "process.crawl(Spider)\n"
        "process.start()\n"
    )
    import os

    run = subprocess.run(
        [sys.executable, str(script)],
        capture_output=True,
        text=True,
        timeout=120,
        env={**os.environ, **(env or {})},
    )
    assert run.returncode == 0, run.stderr
    return json.loads(out.read_text()) if out.exists() else [], run.stderr


def headers_of(item):
    return {k: v for k, v in item["headers"]}


def test_requests_go_out_as_the_browser_sends_them(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/page"!r}]
        def parse(self, response):
            yield {{**response.json(), "protocol": response.protocol}}
        """,
    )
    [item] = items
    sent = headers_of(item)
    assert "Chrome/" in sent["user-agent"] and "Scrapy" not in sent["user-agent"]
    assert sent["sec-fetch-mode"] == "navigate"
    assert item["protocol"] == "HTTP/1.1"


def test_firefox_profile_from_the_settings(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/page"!r}]
        def parse(self, response):
            yield response.json()
        """,
        {"NETWEIR_PROFILE": "firefox"},
    )
    assert "Firefox/" in headers_of(items[0])["user-agent"]


def test_scrapy_follows_redirects_and_keeps_cookies(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/set-cookie"!r}]
        def parse(self, response):
            if response.url.endswith("/set-cookie"):
                yield scrapy.Request({base + "/redirect"!r})
                return
            yield {{**response.json(), "redirects": response.meta.get("redirect_times")}}
        """,
    )
    [item] = items
    assert item["path"] == "/landed" and item["redirects"] == 1
    # The cookie the first page set, sent once, by Scrapy's middleware.
    cookies = [v for k, v in item["headers"] if k == "cookie"]
    assert cookies == ["session=abc"]


def test_a_compressed_body_is_decompressed_once(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/gzip"!r}]
        def parse(self, response):
            yield response.json()
        """,
    )
    assert items == [{"compressed": True}]


def test_forms_post_as_a_browser_posts_them(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/page"!r}]
        def parse(self, response):
            if response.request.method == "GET":
                yield scrapy.FormRequest({base + "/search"!r}, formdata={{"q": "maps"}})
                return
            yield response.json()
        """,
    )
    [item] = items
    assert (item["method"], item["body"]) == ("POST", "q=maps")
    assert headers_of(item)["sec-fetch-mode"] == "navigate"
    assert headers_of(item)["origin"].startswith("http://127.0.0.1")


def test_meta_proxy_with_a_login(base, tmp_path):
    Proxy.logins.clear()
    server, proxy = serve(Proxy)
    try:
        login = proxy.replace("http://", "http://ann:p%40ss@")
        items, _ = crawl(
            tmp_path,
            f"""
            async def start(self):
                yield scrapy.Request("http://shop.test/a", meta={{"proxy": {login!r}}})
            def parse(self, response):
                yield response.json()
            """,
        )
    finally:
        server.shutdown()
    assert items == [{"via": "proxy", "url": "http://shop.test/a"}]
    assert Proxy.logins == ["Basic " + base64.b64encode(b"ann:p@ss").decode()]


def test_failures_go_to_scrapy_s_retries_and_errbacks(tmp_path):
    items, _ = crawl(
        tmp_path,
        """
        async def start(self):
            yield scrapy.Request("http://127.0.0.1:1/", errback=self.failed)
        def parse(self, response):
            yield {"ok": True}
        def failed(self, failure):
            yield {"error": type(failure.value).__name__, "retries": failure.request.meta.get("retry_times")}
        """,
        {"RETRY_TIMES": 1},
    )
    assert items == [{"error": "DownloadConnectionRefusedError", "retries": 1}]


# --- after the first review ----------------------------------------------------


def test_a_redirected_start_url_still_looks_typed(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/redirect"!r}]
        def parse(self, response):
            yield response.json()
        """,
    )
    sent = headers_of(items[0])
    assert items[0]["path"] == "/landed"
    # Typed into the address bar and redirected: no Referer, and still a
    # request from nowhere, not a link from the page that redirected.
    assert "referer" not in sent and sent["sec-fetch-site"] == "none"


def test_a_form_that_redirects_lands_with_a_get(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/page"!r}]
        def parse(self, response):
            if response.url.endswith("/page"):
                yield scrapy.FormRequest({base + "/login"!r}, formdata={{"user": "ann"}})
                return
            yield response.json()
        """,
    )
    [item] = items
    sent = headers_of(item)
    assert (item["path"], item["method"]) == ("/landed", "GET")
    assert "content-type" not in sent and sent["sec-fetch-mode"] == "navigate"


def test_a_json_request_asks_as_a_script_does(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        async def start(self):
            from scrapy.http import JsonRequest
            yield JsonRequest({base + "/api"!r})
            yield scrapy.Request({base + "/own"!r}, headers={{"Accept": "text/csv"}})
        def parse(self, response):
            yield response.json()
        """,
    )
    by_path = {i["path"]: headers_of(i) for i in items}
    assert by_path["/api"]["sec-fetch-mode"] == "cors"
    assert "application/json" in by_path["/api"]["accept"]
    # A spider's own Accept is sent; Scrapy's default one isn't.
    assert by_path["/own"]["accept"] == "text/csv"


def test_no_proxy_means_none_not_the_system_s(base, tmp_path):
    Proxy.logins.clear()
    server, proxy = serve(Proxy)
    try:
        items, _ = crawl(
            tmp_path,
            f"""
            async def start(self):
                yield scrapy.Request({base + "/direct"!r}, meta={{"proxy": None}})
            def parse(self, response):
                yield response.json()
            """,
            env={"http_proxy": proxy, "HTTP_PROXY": proxy, "no_proxy": "", "NO_PROXY": ""},
        )
    finally:
        server.shutdown()
    assert items[0]["path"] == "/direct" and Proxy.logins == []


def test_a_proxy_login_that_isn_t_ascii(base, tmp_path):
    Proxy.logins.clear()
    server, proxy = serve(Proxy)
    try:
        login = proxy.replace("http://", "http://ann:p%C3%A4ss@")
        items, _ = crawl(
            tmp_path,
            f"""
            async def start(self):
                yield scrapy.Request("http://shop.test/a", meta={{"proxy": {login!r}}})
            def parse(self, response):
                yield response.json()
            """,
        )
    finally:
        server.shutdown()
    assert items == [{"via": "proxy", "url": "http://shop.test/a"}]
    # Not dropped: sent, as UTF-8, which is how netweir sends a login.
    assert Proxy.logins == ["Basic " + base64.b64encode("ann:päss".encode()).decode()]


def test_failures_that_are_and_aren_t_retried(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        custom_settings = {{"DOWNLOAD_MAXSIZE": 100_000, "RETRY_TIMES": 2}}
        async def start(self):
            for url, meta in [
                ({base + "/slow"!r}, {{"download_timeout": 0.5}}),
                ("http://nowhere.invalid/", {{}}),
                ({base + "/big"!r}, {{}}),
                ({base + "/page"!r}, {{"proxy": "http://127.0.0.1:9", "_auth": True}}),
            ]:
                yield scrapy.Request(
                    url,
                    meta=meta,
                    headers={{"Proxy-Authorization": "Bearer x"}} if meta.get("_auth") else None,
                    errback=self.failed,
                    dont_filter=True,
                )
        def parse(self, response):
            yield {{"ok": response.url}}
        def failed(self, failure):
            yield {{
                "url": failure.request.url,
                "error": type(failure.value).__name__,
                "retries": failure.request.meta.get("retry_times"),
            }}
        """,
    )
    got = {i["url"]: (i["error"], i["retries"]) for i in items}
    assert got[base + "/slow"] == ("DownloadTimeoutError", 2)
    assert got["http://nowhere.invalid/"] == ("CannotResolveHostError", 2)
    # Too big, or impossible to send: not worth trying again.
    assert got[base + "/big"] == ("DownloadCancelledError", None)
    assert got[base + "/page"] == ("NotSupported", None)


def test_responses_keep_every_header_and_report_latency(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        async def start(self):
            yield scrapy.Request({base + "/two-cookies"!r})
            yield scrapy.Request({base + "/head"!r}, method="HEAD")
        def parse(self, response):
            yield {{
                "url": response.url,
                "cookies": [c.decode() for c in response.headers.getlist("Set-Cookie")],
                "head": (response.headers.get("X-Head") or b"").decode(),
                "latency": response.meta["download_latency"],
            }}
        """,
    )
    by_url = {i["url"]: i for i in items}
    assert by_url[base + "/two-cookies"]["cookies"] == ["a=1", "b=2"]
    assert by_url[base + "/head"]["head"] == "yes"
    assert all(i["latency"] > 0 for i in items)


# --- after the second review ---------------------------------------------------


def test_redirects_from_odd_urls_stay_in_the_chain(base, tmp_path):
    # A start URL with a fragment, and a Location with a space: the hop is
    # still the redirect's, not a link from the page that redirected.
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [
            {base + "/redirect-to/%2Flanded%3Fa%3D1#frag"!r},
            {base + "/redirect-to/%2Fwith%20space"!r},
        ]
        def parse(self, response):
            yield {{**response.json(), "url": response.url}}
        """,
    )
    assert len(items) == 2
    for item in items:
        sent = headers_of(item)
        assert "referer" not in sent and sent["sec-fetch-site"] == "none", item
        assert "#" not in item["url"]


def test_a_request_that_copied_meta_starts_its_own_chain(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/redirect"!r}]
        def parse(self, response):
            if response.url.endswith("/landed"):
                # A new request with the old one's meta, redirect history and
                # all: its own redirect is a hop of its own chain only.
                yield response.follow({base + "/redirect-to/%2Fsecond"!r}, meta=response.meta, callback=self.second)
        def second(self, response):
            yield {{**response.json(), "redirects": response.meta.get("redirect_times")}}
        """,
    )
    [item] = items
    sent = headers_of(item)
    assert item["path"] == "/second"
    # Followed from /landed, then redirected: the page it came from is
    # /landed, as the browser would have it.
    assert sent["referer"].endswith("/landed") and sent["sec-fetch-site"] == "same-origin"


def test_a_project_s_own_default_headers_are_kept(base, tmp_path):
    items, _ = crawl(
        tmp_path,
        f"""
        start_urls = [{base + "/api"!r}]
        def parse(self, response):
            yield response.json()
        """,
        {"DEFAULT_REQUEST_HEADERS": {"Accept": "application/json", "Accept-Language": "de-DE"}},
    )
    sent = headers_of(items[0])
    assert sent["accept"] == "application/json" and sent["accept-language"] == "de-DE"
