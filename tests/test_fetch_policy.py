"""Per-call and per-crawl fetch policy: params, logins, redirects, timeouts,
certificates and retried statuses (docs/design/fetch-policy.md)."""

import base64
import collections
import json
import pathlib
import ssl
import threading
import time
from http.server import BaseHTTPRequestHandler
from urllib.parse import parse_qs

import pytest
import worker_spiders
from fake_redis import FakeRedis
from local_server import serve

import netweir

TLS = pathlib.Path(__file__).with_name("tls")
ADA = "Basic " + base64.b64encode(b"ada:secret").decode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    #: Each request's path, Host and Authorization, in order.
    seen: list[tuple[str, str, str | None]] = []
    hits: collections.Counter = collections.Counter()
    lock = threading.Lock()

    def log_message(self, *args):
        pass

    def send(self, status, body=b"", headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        with Handler.lock:
            Handler.seen.append(
                (self.path, self.headers.get("Host"), self.headers.get("Authorization"))
            )
            Handler.hits[self.path] += 1
            hits = Handler.hits[self.path]
        path, _, query = self.path.partition("?")
        args = {k: v[-1] for k, v in parse_qs(query).items()}
        if path == "/echo":
            body = json.dumps({"path": self.path, "headers": list(self.headers.items())})
            self.send(200, body.encode(), [("Content-Type", "application/json")])
        elif path == "/whoami":
            who = self.headers.get("Authorization") or "nobody"
            self.send(200, f"<h1>{who}</h1>".encode(), [("Content-Type", "text/html")])
        elif path == "/redirect":
            # A line break would end the header: only a plain URL.
            to = args["to"]
            if any(c in to for c in "\r\n"):
                self.send(400, b"bad to")
                return
            self.send(302, b"moved", [("Location", to), ("Set-Cookie", "r=1; Path=/")])
        elif path == "/loop":
            self.send(302, b"", [("Location", "/loop")])
        elif path == "/slow":
            time.sleep(float(args.get("for", "1.5")))
            self.send(200, b"late")
        elif path == "/flaky":
            # The given status the first time, then a page.
            status = int(args["status"]) if hits == 1 else 200
            self.send(status, b"<h1>flaky</h1>", [("Content-Type", "text/html")])
        else:
            self.send(404, b"not here")


def start(tls=False):
    Handler.seen, Handler.hits = [], collections.Counter()
    servers = serve(Handler)
    if tls:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        context.load_cert_chain(TLS / "server.pem", TLS / "server.key")
        for s in servers:
            s.socket = context.wrap_socket(s.socket, server_side=True)
    return servers


@pytest.fixture
def base():
    servers = start()
    yield f"http://127.0.0.1:{servers[0].server_port}"
    for s in servers:
        s.shutdown()
        s.server_close()


@pytest.fixture
def secure():
    servers = start(tls=True)
    yield f"https://localhost:{servers[0].server_port}"
    for s in servers:
        s.shutdown()
        s.server_close()


def sent(page):
    return json.loads(page.body)


def header(page, name):
    return dict((k.lower(), v) for k, v in sent(page)["headers"]).get(name.lower())


# netweir.get, request and Client


def test_params_are_added_to_the_query_as_requests_adds_them(base):
    page = netweir.get(
        f"{base}/echo?a=1#top",
        params={"q": "rust lang", "tags": ["x", "y"], "none": None, "n": 2, "é": "ü"},
    )
    assert sent(page)["path"] == "/echo?a=1&q=rust+lang&tags=x&tags=y&n=2&%C3%A9=%C3%BC"
    page = netweir.get(f"{base}/echo", params=[("k", "1"), ("k", "2")])
    assert sent(page)["path"] == "/echo?k=1&k=2"
    page = netweir.get(f"{base}/echo", params={})
    assert sent(page)["path"] == "/echo"
    page = netweir.get(f"{base}/echo", params={"t": ["a", None, "b"], "u": [None]})
    assert sent(page)["path"] == "/echo?t=a&t=b"
    for query in ("x=1&y=a%20b", b"x=1&y=a%20b"):
        page = netweir.get(f"{base}/echo?z=0", params=query)
        assert sent(page)["path"] == "/echo?z=0&x=1&y=a%20b", "a query is added as it is"


def test_auth_is_sent_as_basic_where_chrome_sends_it(base):
    page = netweir.get(f"{base}/echo", auth=("ada", "sécret"))
    assert header(page, "authorization") == "Basic " + base64.b64encode(
        "ada:sécret".encode()
    ).decode("ascii")
    names = [k.lower() for k, _ in sent(page)["headers"]]
    assert names[:3] == ["host", "connection", "authorization"], names


def test_a_login_in_the_url_becomes_auth_and_leaves_the_url(base):
    port = base.rsplit(":", 1)[1]
    page = netweir.get(f"http://ada:se%40cret@127.0.0.1:{port}/echo")
    expected = "Basic " + base64.b64encode(b"ada:se@cret").decode()
    assert header(page, "authorization") == expected
    assert header(page, "host") == f"127.0.0.1:{port}"
    assert "ada" not in page.url
    # An empty login is none, as requests reads it.
    page = netweir.get(f"http://@127.0.0.1:{port}/echo")
    assert header(page, "authorization") is None
    # auth= wins over the URL's.
    page = netweir.get(f"http://bob:x@127.0.0.1:{port}/echo", auth=("ada", "secret"))
    assert header(page, "authorization") == ADA


def test_auth_stays_behind_when_a_redirect_leaves_the_origin(base):
    port = base.rsplit(":", 1)[1]
    there = f"http://localhost:{port}/echo"
    page = netweir.get(f"{base}/redirect?to={there}", auth=("ada", "secret"))
    assert page.url == there
    assert header(page, "authorization") is None
    assert Handler.seen[0][2] == ADA


def test_a_redirect_can_be_read_instead_of_followed(base):
    for kwargs in ({"allow_redirects": False}, {"follow_redirects": False}):
        page = netweir.get(f"{base}/redirect?to=/echo", **kwargs)
        assert page.status == 302
        assert page.headers["location"] == "/echo"
        assert page.body == b"moved"
    assert [p for p, *_ in Handler.seen] == ["/redirect?to=/echo"] * 2
    with pytest.raises(ValueError, match="allow_redirects"):
        netweir.get(f"{base}/echo", allow_redirects=False, follow_redirects=True)


async def test_a_clients_redirect_is_read_and_its_cookie_kept(base):
    client = netweir.Client()
    page = await client.get(f"{base}/redirect?to=/echo", allow_redirects=False)
    assert page.status == 302
    page = await client.get(f"{base}/echo")
    assert header(page, "cookie") == "r=1"


def test_too_many_redirects_is_an_error_at_the_limit_asked_for(base):
    with pytest.raises(netweir.FetchError) as e:
        netweir.get(f"{base}/loop", max_redirects=2)
    assert e.value.kind == "too_many_redirects"
    assert len(Handler.seen) == 3


async def test_a_clients_redirect_limit_and_a_calls_own(base):
    client = netweir.Client(max_redirects=1)
    with pytest.raises(netweir.FetchError) as e:
        await client.get(f"{base}/redirect?to=/redirect%3Fto%3D/echo")
    assert e.value.kind == "too_many_redirects"
    page = await client.get(f"{base}/redirect?to=/redirect%3Fto%3D/echo", max_redirects=2)
    assert page.status == 200


def test_a_calls_timeout_replaces_the_clients(base):
    with pytest.raises(netweir.FetchError) as e:
        netweir.get(f"{base}/slow", timeout=0.3)
    assert e.value.kind == "timeout"


async def test_a_clients_call_can_wait_longer_than_the_client(base):
    client = netweir.Client(timeout=0.3)
    with pytest.raises(netweir.FetchError):
        await client.get(f"{base}/slow?for=0.8")
    page = await client.get(f"{base}/slow?for=0.8", timeout=5)
    assert page.body == b"late"


async def test_a_clients_auth_goes_with_every_call_unless_one_has_its_own(base):
    client = netweir.Client(auth=("ada", "secret"))
    assert header(await client.get(f"{base}/echo"), "authorization") == ADA
    page = await client.request("GET", f"{base}/echo", auth=("bob", "x"), params={"a": 1})
    assert header(page, "authorization") == "Basic " + base64.b64encode(b"bob:x").decode()
    assert sent(page)["path"] == "/echo?a=1"


def test_verify_false_and_a_ca_file_reach_a_self_signed_host(secure):
    with pytest.raises(netweir.FetchError) as e:
        netweir.get(f"{secure}/echo")
    assert e.value.kind == "tls"
    assert netweir.get(f"{secure}/echo", verify=False).status == 200
    assert netweir.get(f"{secure}/echo", verify=str(TLS / "ca.pem")).status == 200
    assert netweir.get(f"{secure}/echo", verify=TLS / "ca.pem").status == 200


async def test_a_client_trusts_its_ca_file(secure):
    client = netweir.Client(verify=TLS / "ca.pem")
    assert (await client.get(f"{secure}/echo")).status == 200


def test_a_ca_file_that_holds_no_certificate_is_refused(tmp_path):
    empty = tmp_path / "empty.pem"
    empty.write_text("nothing here\n")
    with pytest.raises(ValueError, match="CA certificates"):
        netweir.Client(verify=empty)
    with pytest.raises(FileNotFoundError):
        netweir.Client(verify=tmp_path / "missing.pem")


# A crawl's Request and Settings


def settings(**kw):
    kw = {
        "retries": 0,
        "backoff_base": 0.01,
        "backoff_max": 0.01,
        "throttle": False,
        "start_delay": 0,
        "obey_robots": False,
        "obey_tdmrep": False,
        **kw,
    }
    return netweir.Settings(**kw)


def crawl(requests, parse=None, **kw):
    """Runs a crawl of ``requests``; returns what reached the callback, as
    (status, url, body), and the errors that reached the errback, as
    (url, kind)."""
    pages, errors = [], []

    class Policy(netweir.Spider):
        async def start(self):
            for r in requests:
                yield r

        def parse(self, page):
            pages.append((page.status, page.url, page.body))
            if parse:
                yield from parse(page)

        def failed(self, request, error):
            errors.append((request.url, getattr(error, "kind", type(error).__name__)))

    for r in requests:
        r.errback = "failed"
    spider = Policy()
    spider.settings = settings(**kw)
    crawl.stats = spider.run()
    return pages, errors


def test_a_requests_timeout_and_scrapys_download_timeout(base):
    pages, errors = crawl(
        [
            netweir.Request(f"{base}/slow?a", timeout=0.3),
            netweir.Request(f"{base}/slow?b", meta={"download_timeout": 0.3}),
            netweir.Request(f"{base}/slow?for=0.6", meta={"download_timeout": 0.1}, timeout=5),
        ]
    )
    assert sorted(errors) == [(f"{base}/slow?a", "timeout"), (f"{base}/slow?b", "timeout")]
    assert [p[2] for p in pages] == [b"late"], "the field wins over meta"


def test_a_request_can_stop_at_a_redirect_and_read_it(base):
    pages, errors = crawl(
        [
            netweir.Request(f"{base}/redirect?to=/echo&a", allow_redirects=False),
            netweir.Request(f"{base}/redirect?to=/echo&b", meta={"dont_redirect": True}),
        ]
    )
    assert errors == []
    assert sorted(p[:2] for p in pages) == [
        (302, f"{base}/redirect?to=/echo&a"),
        (302, f"{base}/redirect?to=/echo&b"),
    ]
    assert not any(p.startswith("/echo") for p, *_ in Handler.seen)


def test_redirect_limits_per_request_per_meta_and_per_crawl(base):
    loop = f"{base}/loop"
    _, errors = crawl([netweir.Request(loop, max_redirects=1, dont_filter=True)])
    assert errors == [(loop, "too_many_redirects")]
    assert len(Handler.seen) == 2
    Handler.seen.clear()
    _, errors = crawl([netweir.Request(loop, meta={"redirect_ttl": 2})], max_redirects=5)
    assert errors == [(loop, "too_many_redirects")]
    assert len(Handler.seen) == 3
    Handler.seen.clear()
    # redirect_ttl can't raise the crawl's limit, as in Scrapy.
    _, errors = crawl([netweir.Request(loop, meta={"redirect_ttl": 9})], max_redirects=1)
    assert len(Handler.seen) == 2


def test_retry_http_codes_decide_what_is_tried_again(base):
    pages, _ = crawl(
        [netweir.Request(f"{base}/flaky?status=404")], retries=2, retry_http_codes=(404,)
    )
    assert [p[0] for p in pages] == [200]
    pages, _ = crawl([netweir.Request(f"{base}/flaky?status=503")], retries=2, retry_http_codes=())
    assert [p[0] for p in pages] == [503], "503 left out: not retried"
    pages, _ = crawl([netweir.Request(f"{base}/flaky?status=500")], retries=2)
    assert [p[0] for p in pages] == [200], "the default still retries a 500"


def test_the_crawls_login_goes_to_its_domain_and_no_further(base):
    port = base.rsplit(":", 1)[1]
    local = f"http://localhost:{port}"
    away = f"{local}/redirect?to={base}/whoami"
    crawl(
        [netweir.Request(away), netweir.Request(f"{base}/whoami?direct")],
        http_auth=("ada", "secret"),
        http_auth_domain="localhost",
    )
    by_path = {p: a for p, _, a in Handler.seen}
    assert by_path[f"/redirect?to={base}/whoami"] == ADA
    assert by_path["/whoami"] is None, "followed to 127.0.0.1"
    assert by_path["/whoami?direct"] is None


def test_the_crawls_login_reaches_robots_txt(base):
    port = base.rsplit(":", 1)[1]
    crawl(
        [netweir.Request(f"http://localhost:{port}/whoami")],
        obey_robots=True,
        http_auth=("ada", "secret"),
        http_auth_domain="localhost",
    )
    assert dict((p, a) for p, _, a in Handler.seen)["/robots.txt"] == ADA


def test_a_requests_login_goes_to_its_own_origin_and_scrapys_to_a_domain(base):
    port = base.rsplit(":", 1)[1]
    there = f"http://localhost:{port}/whoami"
    pages, _ = crawl(
        [
            netweir.Request(f"{base}/redirect?to={there}", auth=("ada", "secret")),
            netweir.Request(
                f"http://localhost:{port}/redirect?to={there}%3Fmeta",
                meta={"http_user": "ada", "http_pass": "secret", "http_auth_domain": "localhost"},
            ),
        ]
    )
    bodies = sorted(p[2] for p in pages)
    assert bodies == [f"<h1>{ADA}</h1>".encode(), b"<h1>nobody</h1>"]


def test_a_logins_domain_must_be_a_bare_host_that_isnt_a_public_suffix(base):
    login = {"http_auth": ("ada", "secret")}
    for bad in ("127.0.0.1:8080", "http://localhost", "localhost/x", "com", "co.uk"):
        with pytest.raises(ValueError):
            netweir.Settings(http_auth_domain=bad, **login)
    assert netweir.Settings(http_auth_domain=".Intranet.Example.com", **login).http_auth_domain == (
        "intranet.example.com"
    )
    # In meta, the request is skipped and counted, and the crawl goes on.
    pages, _ = crawl(
        [
            netweir.Request(
                f"{base}/whoami?bad",
                meta={"http_user": "ada", "http_pass": "x", "http_auth_domain": "127.0.0.1:1"},
            ),
            netweir.Request(f"{base}/whoami?good"),
        ]
    )
    assert [p[1] for p in pages] == [f"{base}/whoami?good"]
    assert crawl.stats["callback_errors"] == 1


def test_a_meta_timeout_that_cant_work_skips_its_request_not_the_crawl(base):
    bad = [0, -1, float("inf"), float("nan"), "soon"]
    pages, _ = crawl(
        [
            netweir.Request(f"{base}/echo?{n}", meta={"download_timeout": t})
            for n, t in enumerate(bad)
        ]
        + [netweir.Request(f"{base}/echo?fine", meta={"download_timeout": "5"})]
    )
    assert [p[1] for p in pages] == [f"{base}/echo?fine"]
    assert crawl.stats["callback_errors"] == len(bad)


def test_auth_over_a_meta_login_is_said_once(base, caplog):
    meta = {"http_user": "bob", "http_pass": "x", "http_auth_domain": "localhost"}
    with caplog.at_level("WARNING", logger="netweir"):
        pages, _ = crawl(
            [
                netweir.Request(f"{base}/whoami?{n}", auth=("ada", "secret"), meta=meta)
                for n in range(3)
            ]
        )
    assert {p[2] for p in pages} == {f"<h1>{ADA}</h1>".encode()}
    said = [r for r in caplog.records if "wins over meta" in r.getMessage()]
    assert len(said) == 1


def test_the_command_reads_the_login_from_the_environment(base, tmp_path, monkeypatch):
    from netweir import _cli

    port = base.rsplit(":", 1)[1]
    spider = tmp_path / "who.py"
    spider.write_text(
        "import netweir\n"
        "class Who(netweir.Spider):\n"
        f"    start_urls = ['http://localhost:{port}/whoami']\n"
        "    def parse(self, page):\n"
        "        yield {'who': page.css('h1::text').get()}\n"
    )
    out = tmp_path / "who.jsonl"
    monkeypatch.setenv("NETWEIR_HTTP_AUTH", "ada:secret")
    settings = ["obey_robots=false", "obey_tdmrep=false", "throttle=false", "start_delay=0"]
    args = ["crawl", str(spider), "-o", str(out), "-s", "http_auth_domain=localhost"]
    for s in settings:
        args += ["-s", s]
    assert _cli.main(args) == 0
    assert json.loads(out.read_text())["who"] == ADA


def test_a_crawls_login_needs_a_domain():
    with pytest.raises(ValueError, match="http_auth_domain"):
        netweir.Settings(http_auth=("ada", "secret"))
    with pytest.raises(ValueError, match="http_auth"):
        netweir.Settings(http_auth_domain="example.com")


def test_a_crawl_trusts_its_ca_file(secure):
    pages, errors = crawl([netweir.Request(f"{secure}/echo")], verify=str(TLS / "ca.pem"))
    assert [p[0] for p in pages] == [200] and errors == []
    pages, errors = crawl([netweir.Request(f"{secure}/echo")])
    assert errors == [(f"{secure}/echo", "tls")]


def test_a_requests_policy_survives_a_checkpoint(base, tmp_path):
    port = base.rsplit(":", 1)[1]
    requests = [
        netweir.Request(f"{base}/whoami?first"),
        netweir.Request(
            f"http://localhost:{port}/redirect?to={base}/whoami%3Fsecond",
            auth=("ada", "secret"),
            timeout=4.5,
            max_redirects=0,
        ),
    ]
    checkpoint = str(tmp_path / "state")
    # One request is sent, and the crawl stops with the other saved.
    crawl(requests, max_pages=1, concurrency=1, checkpoint=checkpoint)
    assert len(Handler.seen) == 1
    errors = []

    class Resumed(netweir.Spider):
        async def start(self):
            return
            yield

        def parse(self, page):
            pass

        def failed(self, request, error):
            errors.append((request.timeout, request.max_redirects, request.auth, error.kind))

    spider = Resumed()
    spider.settings = settings(checkpoint=checkpoint)
    spider.run()
    assert Handler.seen[-1][2] == ADA, "sent with its login"
    assert errors == [(4.5, 0, ("ada", "secret"), "too_many_redirects")]


def test_a_requests_policy_travels_through_the_shared_queue(base):
    fake = FakeRedis()
    try:
        port = base.rsplit(":", 1)[1]
        seen = []

        def again(page):
            seen.append((page.request.timeout, page.request.auth, page.request.allow_redirects))
            return []

        pages, _ = crawl(
            [
                netweir.Request(
                    f"http://localhost:{port}/redirect?to=/whoami",
                    auth=("ada", "secret"),
                    timeout=7,
                    allow_redirects=False,
                    callback="parse",
                )
            ],
            parse=again,
            queue=fake.url,
            queue_key="policy",
            queue_idle=1.0,
        )
        assert [p[0] for p in pages] == [302]
        assert Handler.seen[0][2] == ADA
        assert seen == [(7.0, ("ada", "secret"), False)]
    finally:
        fake.close()


def test_requests_from_workers_keep_their_policy(base):
    spider = worker_spiders.Logins()
    spider.start_urls = [f"{base}/echo"]
    spider.settings = settings(workers=2)
    items = []
    spider.run(output=items.append)
    assert items == [{"who": ADA, "timeout": 4.0, "max_redirects": 3}]


def test_the_command_line_sets_the_policy_settings():
    import dataclasses

    from netweir._cli import _parse_setting

    fields = {f.name: f for f in dataclasses.fields(netweir.Settings)}
    assert _parse_setting("retry_http_codes=500, 503", fields) == ("retry_http_codes", (500, 503))
    assert _parse_setting("max_redirects=5", fields) == ("max_redirects", 5)
    assert _parse_setting("verify=false", fields) == ("verify", False)
    assert _parse_setting("verify=/etc/ca.pem", fields) == ("verify", "/etc/ca.pem")
    # A password may hold commas and colons: the login splits at the first.
    assert _parse_setting("http_auth=ada:se,c:ret", fields) == ("http_auth", ("ada", "se,c:ret"))
    with pytest.raises(ValueError, match="user:password"):
        _parse_setting("http_auth=ada", fields)
    with pytest.raises(ValueError, match="whole numbers"):
        _parse_setting("retry_http_codes=5xx", fields)


def test_a_requests_password_isnt_in_its_repr():
    r = netweir.Request("https://e.com/", auth=("ada", "secret"))
    assert "secret" not in repr(r)


def test_a_login_in_a_requests_url_becomes_its_auth():
    r = netweir.Request("https://ada:se%40cret@e.com/a")
    assert r.url == "https://e.com/a"
    assert r.auth == ("ada", "se@cret")


def test_policy_values_that_cannot_work_are_refused():
    for bad in (
        {"timeout": 0},
        {"timeout": -1},
        {"timeout": float("inf")},
        {"timeout": "soon"},
        {"max_redirects": -1},
    ):
        with pytest.raises(ValueError):
            netweir.Request("https://e.com/", **bad)
    with pytest.raises(ValueError):
        netweir.Settings(max_redirects=-1)
    with pytest.raises(ValueError):
        netweir.Settings(retry_http_codes=(99,))
