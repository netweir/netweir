"""Proxy pools: the three rotations, benching proxies that fail, a
request's own proxy, and what each proxy did.

Each proxy here answers for the sites itself, as a forward proxy is sent
the whole URL (``GET http://shop.test/1``), and logs what it carried. One
crawl request goes at a time, so which proxy gets which request follows
from the rotation alone.
"""

import logging
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

import pytest

import netweir

BLOCK = Path(__file__).parent.parent / "crates/netweir-core/tests/fixtures/blocks"


def block_page():
    raw = (BLOCK / "cloudflare-block.http").read_bytes()
    head, body = raw.split(b"\r\n\r\n", 1)
    lines = head.decode().split("\r\n")
    headers = [tuple(line.split(": ", 1)) for line in lines[1:]]
    return int(lines[0].split(" ")[1]), headers, body


class Proxy:
    """A forward proxy on a local port. ``answer`` is "page" (every URL is
    a small page, and /login sets a cookie), "login" (a 407 for everything) or "block" (Cloudflare's
    block page for everything). ``carried`` lists the URLs it was sent,
    with the Cookie header each had."""

    def __init__(self, answer="page"):
        self.carried: list[tuple[str, str]] = []
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                proxy.carried.append((self.path, self.headers.get("Cookie", "")))
                if answer == "login":
                    self.reply(407, [("Proxy-Authenticate", 'Basic realm="pool"')], b"")
                elif answer == "block":
                    self.reply(*block_page())
                else:
                    body = f"<html><title>{self.path}</title></html>".encode()
                    headers = [("Content-Type", "text/html")]
                    if self.path.endswith("/login"):
                        headers.append(("Set-Cookie", "sid=1; Path=/"))
                    self.reply(200, headers, body)

            def reply(self, status, headers, body):
                self.send_response(status)
                for k, v in headers:
                    if k.lower() != "content-length":
                        self.send_header(k, v)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def urls(self):
        return [url for url, _ in self.carried]

    def hosts(self):
        return {urlsplit(url).hostname for url in self.urls()}


@pytest.fixture
def proxies():
    made: list[Proxy] = []

    def make(answer="page"):
        made.append(Proxy(answer))
        return made[-1]

    yield make
    for p in made:
        p.server.shutdown()
        p.server.server_close()


def refused():
    """A proxy address nothing listens at."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    return f"http://127.0.0.1:{port}"


def settings(**kw):
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        concurrency=1,
        per_domain=1,
        backoff_base=0.01,
        backoff_max=0.05,
        **kw,
    )


def crawl(urls_or_requests, **kw):
    seen: list[tuple[str, int]] = []

    class Spider(netweir.Spider):
        settings = netweir.Settings()

        async def start(self):
            for r in urls_or_requests:
                yield r if isinstance(r, netweir.Request) else netweir.Request(r, dont_filter=True)

        def parse(self, page):
            seen.append((page.url, page.status))

    spider = Spider()
    spider.settings = settings(**kw)
    stats = spider.run()
    return spider, stats, seen


def test_per_request_spreads_the_crawl_over_the_pool(proxies):
    a, b, c = proxies(), proxies(), proxies()
    urls = [f"http://shop.test/{i}" for i in range(6)]
    spider, stats, seen = crawl(urls, proxies=[a.url, b.url, c.url], proxy_rotation="per_request")
    assert len(seen) == 6
    assert (len(a.urls()), len(b.urls()), len(c.urls())) == (2, 2, 2)
    assert set(a.urls() + b.urls() + c.urls()) == set(urls)
    assert spider.proxy_stats == {
        p.url: {"requests": 2, "failures": 0, "benched": 0} for p in (a, b, c)
    }
    assert stats["proxies_benched"] == 0


def test_per_request_cookies_are_the_crawls_whichever_proxy(proxies):
    a, b = proxies(), proxies()

    class Spider(netweir.Spider):
        start_urls = ["http://shop.test/login"]

        def parse(self, page):
            if page.url.endswith("/login"):
                yield netweir.Request("http://shop.test/next")

    spider = Spider()
    spider.settings = settings(proxies=[a.url, b.url], proxy_rotation="per_request")
    spider.run()
    assert a.carried == [("http://shop.test/login", "")]
    assert b.carried == [("http://shop.test/next", "sid=1")]


def test_per_host_keeps_each_site_on_one_proxy(proxies):
    a, b = proxies(), proxies()
    urls = [f"http://{host}/{i}" for i in range(3) for host in ("one.test", "two.test")]
    spider, _, seen = crawl(urls, proxies=[a.url, b.url], proxy_rotation="per_host")
    assert len(seen) == 6
    assert len(a.hosts()) == len(b.hosts()) == 1
    assert a.hosts() | b.hosts() == {"one.test", "two.test"}
    assert len(a.urls()) == len(b.urls()) == 3


def test_per_host_moves_a_blocked_site_to_the_next_proxy(proxies):
    blocker, good = proxies("block"), proxies()
    spider, stats, seen = crawl(
        ["http://shop.test/1", "http://shop.test/2"],
        proxies=[blocker.url, good.url],
        proxy_rotation="per_host",
    )
    assert sorted(seen) == [("http://shop.test/1", 200), ("http://shop.test/2", 200)]
    assert len(blocker.urls()) == 1
    assert len(good.urls()) == 2
    assert stats["blocked"] == 1
    assert spider.proxy_stats[blocker.url] == {"requests": 1, "failures": 1, "benched": 0}


def test_on_block_takes_the_next_proxy_only_after_a_block(proxies):
    front, good = proxies("block"), proxies()
    spider, stats, seen = crawl(["http://shop.test/1"], proxy=front.url, proxies=[good.url])
    assert seen == [("http://shop.test/1", 200)]
    assert front.urls() == good.urls() == ["http://shop.test/1"]
    assert stats["sessions_replaced"] == 1
    # The crawl's own proxy isn't one of the pool.
    assert spider.proxy_stats == {good.url: {"requests": 1, "failures": 0, "benched": 0}}


def test_proxies_that_fail_are_benched_and_their_logins_stay_out_of_the_logs(proxies, caplog):
    login, good = proxies("login"), proxies()
    dead = refused()
    secret = login.url.replace("http://", "http://ann:hunter2@")
    urls = [f"http://shop.test/{i}" for i in range(4)]
    with caplog.at_level(logging.WARNING, logger="netweir"):
        spider, stats, seen = crawl(
            urls,
            proxies=[dead, secret, good.url],
            proxy_rotation="per_request",
            proxy_failures=1,
        )
    assert len(seen) == 4, "every page got through the one good proxy"
    assert stats["proxies_benched"] == 2
    assert spider.proxy_stats[dead] == {"requests": 1, "failures": 1, "benched": 1}
    assert spider.proxy_stats[secret] == {"requests": 1, "failures": 1, "benched": 1}
    assert spider.proxy_stats[good.url]["requests"] == 4
    assert "hunter2" not in caplog.text
    port = login.url.rsplit(":", 1)[1]
    assert f"proxy http://***@127.0.0.1:{port} failed once; benched" in caplog.text


def test_with_every_proxy_benched_the_crawl_carries_on_and_says_so_once(proxies, caplog):
    login = proxies("login")
    with caplog.at_level(logging.WARNING, logger="netweir"):
        spider, _, _ = crawl(
            ["http://shop.test/1", "http://shop.test/2"],
            proxies=[login.url],
            proxy_rotation="per_request",
            proxy_failures=1,
            retries=1,
        )
    # Two tries each, all through the one proxy there is.
    assert len(login.urls()) == 4
    assert spider.proxy_stats[login.url]["benched"] == 1
    assert caplog.text.count("every proxy is benched") == 1


def test_a_request_can_name_its_proxy_as_scrapy_does_in_meta(proxies):
    pool, own, meta = proxies(), proxies(), proxies()
    requests = [
        netweir.Request("http://shop.test/pool"),
        netweir.Request("http://shop.test/own", proxy=own.url),
        netweir.Request("http://shop.test/meta", meta={"proxy": meta.url}),
    ]
    spider, _, seen = crawl(requests, proxies=[pool.url], proxy_rotation="per_request")
    assert len(seen) == 3
    assert pool.urls() == ["http://shop.test/pool"]
    assert own.urls() == ["http://shop.test/own"]
    assert meta.urls() == ["http://shop.test/meta"]
    assert list(spider.proxy_stats) == [pool.url, own.url, meta.url]


def test_a_request_whose_proxy_cant_be_used_is_skipped(caplog):
    with caplog.at_level(logging.WARNING, logger="netweir"):
        _, stats, seen = crawl([netweir.Request("http://shop.test/1", proxy="ftp://a:21")])
    assert seen == []
    assert stats["invalid_urls"] == 1
    assert "its proxy can't be used" in caplog.text


@pytest.mark.parametrize("none", [None, ""])
def test_meta_proxy_none_sends_the_request_direct_as_in_scrapy(proxies, none):
    pool, site = proxies(), proxies()
    direct = f"{site.url}/direct"
    spider, _, seen = crawl(
        [netweir.Request(direct, meta={"proxy": none})],
        proxies=[pool.url],
        proxy_rotation="per_request",
    )
    assert seen == [(direct, 200)]
    assert site.urls() == ["/direct"], "straight to the site"
    assert pool.urls() == []
    assert spider.proxy_stats[pool.url]["requests"] == 0


def test_going_direct_survives_a_checkpoint(proxies, tmp_path):
    pool, site, other = proxies(), proxies(), proxies()
    requests = [
        netweir.Request("http://shop.test/first", priority=1, proxy=other.url),
        netweir.Request(f"{site.url}/later", meta={"proxy": None}),
    ]
    crawl(requests, checkpoint=str(tmp_path), max_pages=1)
    assert site.urls() == []
    crawl([], checkpoint=str(tmp_path), proxies=[pool.url], proxy_rotation="per_request")
    assert site.urls() == ["/later"]
    assert pool.urls() == []


def test_a_password_that_isnt_percent_encoded_is_refused_without_showing_it(caplog):
    with pytest.raises(ValueError, match="percent-encode") as caught:
        netweir.Settings(proxies=["http://ann:hun/ter2@proxy:8080"])._engine()
    assert "ter2" not in str(caught.value)
    with caplog.at_level(logging.WARNING, logger="netweir"):
        _, stats, _ = crawl([netweir.Request("http://shop.test/1", proxy="http://a:b#c@p:1")])
    assert stats["invalid_urls"] == 1
    assert "b#c" not in caplog.text


def test_a_failure_through_a_proxy_with_a_login_doesnt_show_the_login(caplog):
    dead = refused().replace("http://", "http://ann:hunter2@")
    with caplog.at_level(logging.DEBUG, logger="netweir"):
        spider, _, seen = crawl(
            ["http://shop.test/1"], proxies=[dead], proxy_rotation="per_request", retries=0
        )
    assert seen == []
    assert "hunter2" not in caplog.text
    # proxy_stats is keyed by the proxy as you gave it, so you can look it up.
    assert spider.proxy_stats[dead]["failures"] == 1


def test_a_requests_proxy_survives_a_checkpoint(proxies, tmp_path):
    own, other = proxies(), proxies()
    requests = [
        netweir.Request("http://shop.test/first", priority=1, proxy=other.url),
        netweir.Request("http://shop.test/later", proxy=own.url),
    ]
    # The first run stops after one request; the second has the other.
    crawl(requests, checkpoint=str(tmp_path), max_pages=1)
    assert (other.urls(), own.urls()) == (["http://shop.test/first"], [])
    # Nothing may go out through the crawl's own proxy, which is dead.
    crawl([], checkpoint=str(tmp_path), proxy=refused())
    assert own.urls() == ["http://shop.test/later"]


@pytest.mark.parametrize(
    "kw, message",
    [
        ({"proxy_rotation": "per_request"}, "give some"),
        ({"proxy_rotation": "per_host", "proxies": ["http://a:1"], "proxy": "http://b:2"}, "win"),
        ({"proxy_rotation": "sometimes", "proxies": ["http://a:1"]}, "proxy_rotation must be"),
        ({"proxy_failures": 0}, "proxy_failures"),
        ({"proxy_cooldown": 0}, "proxy_cooldown"),
    ],
)
def test_proxy_settings_are_checked(kw, message):
    with pytest.raises(ValueError, match=message):
        netweir.Settings(**kw)


def test_a_pool_proxy_the_client_cant_use_is_refused_up_front():
    with pytest.raises(ValueError, match="socks5") as caught:
        netweir.Settings(proxies=["ftp://ann:hunter2@proxy:21"])._engine()
    assert "hunter2" not in str(caught.value)


def test_a_requests_proxy_login_stays_out_of_its_repr():
    request = netweir.Request("http://shop.test/1", proxy="http://ann:hunter2@10.0.0.1:8080")
    assert "hunter2" not in repr(request)
    assert request.proxy == "http://ann:hunter2@10.0.0.1:8080"
