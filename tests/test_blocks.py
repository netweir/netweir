"""Block pages: what a response means, and what netweir.get does about it."""

import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import netweir

FIXTURES = Path(__file__).parent.parent / "crates/netweir-core/tests/fixtures/blocks"


def fixture(name):
    raw = (FIXTURES / name).read_bytes()
    head, body = raw.split(b"\r\n\r\n", 1)
    lines = head.decode().split("\r\n")
    status = int(lines[0].split(" ")[1])
    headers = [tuple(line.split(": ", 1)) for line in lines[1:]]
    return status, headers, body


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        name = self.path.lstrip("/")
        status, headers, body = fixture(name)
        self.send_response(status)
        for k, v in headers:
            if k.lower() != "content-length":
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


def test_get_raises_on_a_block(base):
    with pytest.raises(netweir.Blocked) as caught:
        netweir.get(f"{base}/cloudflare-block.http")
    assert caught.value.vendor == "cloudflare"
    assert caught.value.kind == "block"
    assert caught.value.page.status == 403
    assert "cloudflare" in str(caught.value)


def test_a_block_can_be_a_page(base):
    page = netweir.get(f"{base}/datadome-captcha.http", raise_on_block=False)
    assert page.outcome == "blocked"
    assert page.blocked == "datadome"


@pytest.mark.parametrize(
    "name, outcome",
    [
        ("ok-behind-cloudflare.http", "ok"),
        ("ok-mentions-vendors.http", "ok"),
        ("not-found.http", "http_error"),
        ("throttled.http", "throttled"),
        ("payment.http", "payment_required"),
    ],
)
def test_outcomes(base, name, outcome):
    page = netweir.get(f"{base}/{name}")
    assert page.outcome == outcome
    assert page.blocked is None


async def test_client_raises_too_and_get_many_can_collect(base):
    async with netweir.Client() as client:
        with pytest.raises(netweir.Blocked):
            await client.get(f"{base}/human-captcha.http")
        pages = await client.get_many(
            [f"{base}/ok-behind-cloudflare.http", f"{base}/akamai-block.http"],
            return_exceptions=True,
        )
    assert pages[0].outcome == "ok"
    assert isinstance(pages[1], netweir.Blocked) and pages[1].vendor == "akamai"


FAST = netweir.Settings(
    throttle=False,
    start_delay=0,
    obey_robots=False,
    obey_tdmrep=False,
    retries=1,
    backoff_base=0.01,
    backoff_max=0.02,
    max_delay=0.05,
)


def test_a_spider_hears_about_blocks_it_could_not_get_past(base):
    seen = []

    class Guarded(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/datadome-captcha.http", f"{base}/ok-behind-cloudflare.http"]

        def parse(self, page):
            yield {"url": page.url}

        def on_block(self, request, page):
            seen.append((request.url, page.blocked, page.status))
            yield {"blocked": request.url}

    stats = Guarded().run()
    assert seen == [(f"{base}/datadome-captcha.http", "datadome", 403)]
    assert stats["items"] == 2, "on_block can yield items too"
    assert stats["blocked"] == 2 and stats["retries"] == 1
    assert stats["sessions_replaced"] == 1, "a new session only for the one retry"


def test_blocks_are_logged_by_default(base, caplog):
    class Quiet(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/akamai-block.http"]

    with caplog.at_level("WARNING", logger="netweir"):
        Quiet().run()
    assert "blocked by akamai" in caplog.text


def test_payment_required_is_reported_with_the_price(base, caplog):
    class Paying(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/payment.http"]

        def parse(self, page):
            return None

    with caplog.at_level("WARNING", logger="netweir"):
        stats = Paying().run()
    assert "USD 0.01" in caplog.text
    assert stats["retries"] == 0


def test_settings_check_the_ladder():
    with pytest.raises(ValueError):
        netweir.Settings(breaker_ratio=1.5)
    with pytest.raises(ValueError):
        netweir.Settings(retries=-1)
    assert netweir.Settings(proxies=["http://a:1", "http://b:2"]).proxies == (
        "http://a:1",
        "http://b:2",
    )
