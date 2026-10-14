import asyncio
import gzip
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

PAGE = b"<html><head><title>Shop</title></head><body><h1>Books</h1><a href='/b'>b</a></body></html>"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send(self, status, body, headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path
        if path == "/page":
            self.send(200, PAGE, [("Content-Type", "text/html; charset=utf-8")])
        elif path == "/latin1":
            self.send(
                200,
                "<p>café</p>".encode("latin-1"),
                [("Content-Type", "text/html; charset=ISO-8859-1")],
            )
        elif path == "/redirect":
            self.send(302, b"", [("Location", "/page")])
        elif path == "/headers":
            body = json.dumps(list(self.headers.items())).encode()
            self.send(200, body, [("Content-Type", "application/json")])
        elif path == "/gzip":
            self.send(
                200,
                gzip.compress(PAGE),
                [("Content-Type", "text/html"), ("Content-Encoding", "gzip")],
            )
        elif path == "/set-cookie":
            self.send(200, b"ok", [("Set-Cookie", "session=abc; Path=/")])
        elif path == "/twice":
            self.send(200, b"ok", [("X-Seen", "a"), ("X-Seen", "b")])
        elif path == "/two-cookies":
            self.send(200, b"ok", [("Set-Cookie", "a=1; Path=/"), ("Set-Cookie", "b=2; Path=/")])
        elif path == "/login":
            # A cookie set on a redirect must reach the page it redirects to.
            self.send(302, b"", [("Set-Cookie", "auth=yes; Path=/"), ("Location", "/headers")])
        elif path == "/slow":
            time.sleep(2)
            self.send(200, b"late")
        else:
            self.send(404, b"<h1>not here</h1>", [("Content-Type", "text/html")])


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def test_get_returns_a_parsed_page(base):
    page = netweir.get(f"{base}/page")
    assert page.status == 200
    assert page.url == f"{base}/page"
    assert page.version == "HTTP/1.1"
    assert page.headers["content-type"] == "text/html; charset=utf-8"
    assert page.body == PAGE
    assert page.css("h1::text").get() == "Books"
    assert page.css("a::attr(href)").getall() == ["/b"]


def test_redirects_are_followed(base):
    page = netweir.get(f"{base}/redirect")
    assert page.status == 200
    assert page.url == f"{base}/page"


def test_error_statuses_are_pages_not_exceptions(base):
    page = netweir.get(f"{base}/missing")
    assert page.status == 404
    assert page.css("h1::text").get() == "not here"


def test_text_uses_the_declared_charset(base):
    page = netweir.get(f"{base}/latin1")
    assert page.text == "<p>café</p>"
    assert page.css("p::text").get() == "café"


def test_compressed_bodies_arrive_decompressed(base):
    assert netweir.get(f"{base}/gzip").body == PAGE


def test_repeated_headers_are_joined(base):
    assert netweir.get(f"{base}/twice").headers["x-seen"] == "a, b"


def test_raw_headers_keep_every_set_cookie(base):
    page = netweir.get(f"{base}/two-cookies")
    cookies = [v for k, v in page.raw_headers if k == "set-cookie"]
    assert cookies == ["a=1; Path=/", "b=2; Path=/"]
    assert page.headers["set-cookie"] == "b=2; Path=/"


def test_profile_headers_go_out_in_chrome_http1_order(base):
    sent = json.loads(netweir.get(f"{base}/headers").body)
    names = [k for k, _ in sent]
    assert names[:3] == ["Host", "Connection", "sec-ch-ua"]
    assert "User-Agent" in names
    # HTTP/2-only headers stay off plain-HTTP requests, as in Chrome.
    assert "priority" not in [n.lower() for n in names]


def test_caller_headers_override_and_extend(base):
    sent = dict(
        json.loads(
            netweir.get(
                f"{base}/headers", headers={"Accept-Language": "de-DE", "X-Trace": "1"}
            ).body
        )
    )
    assert sent["Accept-Language"] == "de-DE"
    # New headers keep the caller's capitalisation over HTTP/1.1.
    assert sent["X-Trace"] == "1"


async def test_client_keeps_cookies_between_requests(base):
    async with netweir.Client() as client:
        await client.get(f"{base}/set-cookie")
        sent = dict(json.loads((await client.get(f"{base}/headers")).body))
    assert sent["Cookie"] == "session=abc"


async def test_cookies_set_on_a_redirect_reach_the_next_page(base):
    async with netweir.Client() as client:
        page = await client.get(f"{base}/login")
    assert page.url == f"{base}/headers"
    assert dict(json.loads(page.body))["Cookie"] == "auth=yes"


async def test_get_many_is_concurrent_and_ordered(base):
    client = netweir.Client()
    start = time.perf_counter()
    pages = await client.get_many([f"{base}/slow", f"{base}/page", f"{base}/slow"])
    elapsed = time.perf_counter() - start
    assert [p.body for p in pages] == [b"late", PAGE, b"late"]
    assert elapsed < 3.5, f"took {elapsed:.1f}s; requests ran one after another"


async def test_get_many_can_return_errors_in_place(base):
    client = netweir.Client()
    pages = await client.get_many([f"{base}/page", "not a url"], return_exceptions=True)
    assert pages[0].status == 200
    assert isinstance(pages[1], netweir.FetchError)
    assert pages[1].kind == "invalid"


def test_timeouts_raise_fetch_error(base):
    with pytest.raises(netweir.FetchError) as err:
        netweir.get(f"{base}/slow", timeout=0.3)
    assert err.value.kind == "timeout"


def test_refused_connections_raise_fetch_error():
    import socket

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    with pytest.raises(netweir.FetchError) as err:
        netweir.get(f"http://127.0.0.1:{port}/")
    assert err.value.kind == "connect"


def test_bad_arguments_raise_value_error():
    with pytest.raises(ValueError, match="unknown profile"):
        netweir.Client(profile="netscape")
    with pytest.raises(ValueError):
        netweir.Client(timeout=0)
    with pytest.raises(ValueError):
        netweir.Client(proxy="not a proxy")
    with pytest.raises(netweir.FetchError) as err:
        netweir.get("ftp://example.com/")
    assert err.value.kind == "invalid"


def test_blocking_get_releases_the_gil(base):
    results = []
    threads = [
        threading.Thread(target=lambda: results.append(netweir.get(f"{base}/slow")))
        for _ in range(4)
    ]
    start = time.perf_counter()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(results) == 4
    assert time.perf_counter() - start < 3.5


def test_asyncio_loop_stays_responsive(base):
    async def main():
        client = netweir.Client()
        ticks = 0

        async def ticker():
            nonlocal ticks
            while True:
                await asyncio.sleep(0.05)
                ticks += 1

        task = asyncio.create_task(ticker())
        await client.get(f"{base}/slow")
        task.cancel()
        return ticks

    assert asyncio.run(main()) >= 20


def test_profiles_by_browser_or_version(base):
    for profile in (
        "chrome",
        "firefox",
        "safari",
        "chrome-155-macos",
        "chrome-154-macos",
        "firefox-158-macos",
        "firefox-156-macos",
        "safari-27-macos",
    ):
        assert netweir.get(f"{base}/page", profile=profile).status == 200, profile
    with pytest.raises(ValueError, match="firefox"):
        netweir.get(f"{base}/page", profile="netscape")


def test_a_redirect_can_be_left_unfollowed(base):
    from netweir._native import Fetcher

    async def go():
        response = await Fetcher().send(f"{base}/redirect", follow=False)
        return response.status, dict(response.headers)["location"]

    assert asyncio.run(go()) == (302, "/page")


def test_a_fetcher_without_cookies_keeps_none(base):
    from netweir._native import Fetcher

    async def go(cookies):
        fetcher = Fetcher(cookies=cookies)
        await fetcher.get(f"{base}/set-cookie")
        sent = json.loads((await fetcher.get(f"{base}/headers")).body)
        return [v for k, v in sent if k.lower() == "cookie"]

    assert asyncio.run(go(True)) == ["session=abc"]
    assert asyncio.run(go(False)) == []
