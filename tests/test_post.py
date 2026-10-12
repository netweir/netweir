import asyncio
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl

import pytest

import netweir

FORM_PAGE = """<!doctype html><html><body>
<form id="search" action="/echo" method="post">
  <input type="hidden" name="token" value="t0k">
  <input name="q" value="books">
  <input type="checkbox" name="new" value="yes" checked>
  <input type="checkbox" name="used" value="yes">
  <input type="radio" name="sort" value="price">
  <input type="radio" name="sort" value="date" checked>
  <select name="size"><option value="s">S</option><option value="m" selected>M</option></select>
  <select name="colour"><option value="red">Red</option><option value="blue">Blue</option></select>
  <select name="tags" multiple><option selected>a</option><option>b</option><option selected>c</option></select>
  <textarea name="note">hello</textarea>
  <input name="off" value="x" disabled>
  <input value="no name">
  <input type="submit" name="go" value="Search">
  <button type="submit" name="alt" value="Other">Other</button>
</form>
<form name="lookup" action="/echo">
  <input name="q" value="first page">
</form>
<form action="/echo" method="post" enctype="multipart/form-data">
  <input name="title" value="A book">
  <input name="body" value="text">
</form>
<form id="upload" action="/echo" method="post" enctype="multipart/form-data">
  <input name="title" value="A book">
  <input type="file" name="cover">
  <input name="after" value="x">
</form>
<form id="plain" action="/echo" method="post">
  <input type="file" name="doc">
  <input name="q" value="y">
</form>
</body></html>"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    broken = 0

    def log_message(self, *args):
        pass

    def send(self, status, body, headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def echo(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode("latin-1")
        reply = {
            "method": self.command,
            "path": self.path,
            "headers": list(self.headers.items()),
            "body": body,
        }
        self.send(200, json.dumps(reply).encode(), [("Content-Type", "application/json")])

    def do_GET(self):
        if self.path == "/big":
            self.send(200, b"x" * 50_000, [("Content-Type", "text/plain")])
            return
        if self.path == "/form":
            self.send(200, FORM_PAGE.encode(), [("Content-Type", "text/html; charset=utf-8")])
        else:
            self.echo()

    def do_POST(self):
        if self.path == "/broken":
            length = int(self.headers.get("Content-Length") or 0)
            self.rfile.read(length)
            Handler.broken += 1
            self.send(500, b"oops")
            return
        if self.path == "/signin":
            length = int(self.headers.get("Content-Length") or 0)
            self.rfile.read(length)
            self.send(302, b"", [("Location", "/home"), ("Set-Cookie", "user=ada; Path=/")])
        else:
            self.echo()

    do_PUT = do_PATCH = do_DELETE = echo


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def seen(page):
    data = json.loads(page.text)
    data["headers"] = {k.lower(): v for k, v in data["headers"]}
    return data


def test_a_form_post_is_url_encoded_and_sent_as_a_navigation(base):
    got = seen(netweir.post(f"{base}/echo", form={"q": "rust books", "page": 2}))
    assert got["method"] == "POST"
    assert parse_qsl(got["body"]) == [("q", "rust books"), ("page", "2")]
    h = got["headers"]
    assert h["content-type"] == "application/x-www-form-urlencoded"
    assert h["origin"] == base
    assert h["sec-fetch-mode"] == "navigate"
    assert h["sec-fetch-site"] == "same-origin"
    assert h["cache-control"] == "max-age=0"


def test_json_is_sent_as_a_scripts_request(base):
    got = seen(netweir.post(f"{base}/echo", json={"q": "rust", "n": [1, 2]}))
    assert got["body"] == '{"q":"rust","n":[1,2]}'
    h = got["headers"]
    assert h["content-type"] == "application/json"
    assert h["accept"] == "*/*"
    assert h["sec-fetch-mode"] == "cors"
    assert "upgrade-insecure-requests" not in h


def test_a_raw_body_keeps_the_callers_content_type(base):
    got = seen(netweir.post(f"{base}/echo", body=b"a,b\n1,2", headers={"Content-Type": "text/csv"}))
    assert got["body"] == "a,b\n1,2"
    assert got["headers"]["content-type"] == "text/csv"
    got = seen(netweir.post(f"{base}/echo", body="plain"))
    assert got["headers"]["content-type"] == "text/plain;charset=UTF-8"


def test_only_one_kind_of_body_at_a_time(base):
    with pytest.raises(ValueError, match="one of"):
        netweir.post(f"{base}/echo", form={"a": 1}, json={"a": 1})


def test_any_method(base):
    got = seen(netweir.request("PUT", f"{base}/echo", json={"a": 1}))
    assert (got["method"], got["body"]) == ("PUT", '{"a":1}')
    got = seen(netweir.request("DELETE", f"{base}/echo"))
    assert got["method"] == "DELETE"


def test_a_post_answered_with_a_redirect_becomes_a_get(base):
    page = netweir.post(f"{base}/signin", form={"user": "ada"})
    got = seen(page)
    assert (got["method"], got["path"], got["body"]) == ("GET", "/home", "")
    assert page.url == f"{base}/home"
    assert "user=ada" in got["headers"]["cookie"]
    assert got["headers"]["cache-control"] == "max-age=0"


def test_a_get_from_a_page_looks_like_following_a_link(base):
    got = seen(netweir.get(f"{base}/echo", referer=f"{base}/form"))
    assert got["headers"]["referer"] == f"{base}/form"
    assert got["headers"]["sec-fetch-site"] == "same-origin"
    plain = seen(netweir.get(f"{base}/echo"))
    assert plain["headers"]["sec-fetch-site"] == "none"
    assert "referer" not in plain["headers"]


def test_a_client_posts_too(base):
    async def main():
        async with netweir.Client() as client:
            return seen(await client.post(f"{base}/echo", json={"x": 1}))

    assert asyncio.run(main())["method"] == "POST"


def test_a_form_on_a_page_is_read_as_a_browser_would_submit_it(base):
    page = netweir.get(f"{base}/form")
    form = page.form()
    assert (form.method, form.action, form.enctype) == (
        "POST",
        f"{base}/echo",
        "application/x-www-form-urlencoded",
    )
    assert form.fields == [
        ("token", "t0k"),
        ("q", "books"),
        ("new", "yes"),
        ("sort", "date"),
        ("size", "m"),
        ("colour", "red"),
        ("tags", "a"),
        ("tags", "c"),
        ("note", "hello"),
        ("go", "Search"),
    ]
    assert form.referer == f"{base}/form"


def test_data_replaces_fields_and_click_picks_the_button(base):
    page = netweir.get(f"{base}/form")
    form = page.form(data={"q": "maps", "extra": "1"}, click="alt")
    fields = dict(form.fields)
    assert fields["q"] == "maps" and fields["extra"] == "1"
    assert fields["alt"] == "Other" and "go" not in fields
    assert "go" not in dict(page.form(click=False).fields)


def test_a_form_can_be_picked_by_selector_name_or_number(base):
    page = netweir.get(f"{base}/form")
    assert page.form("form[name=lookup]").method == "GET"
    assert page.form(formname="lookup").fields == [("q", "first page")]
    assert page.form(formnumber=2).enctype == "multipart/form-data"
    assert page.form(formid="search").method == "POST"
    with pytest.raises(ValueError, match="no form"):
        page.form("#nope")


def test_submitting_a_form_posts_it_from_its_page(base):
    page = netweir.get(f"{base}/form")
    got = seen(netweir.submit(page.form(data={"q": "maps"})))
    assert got["method"] == "POST"
    assert ("q", "maps") in parse_qsl(got["body"])
    assert got["headers"]["referer"] == f"{base}/form"


def test_a_get_form_puts_its_fields_in_the_query(base):
    page = netweir.get(f"{base}/form")
    got = seen(netweir.submit(page.form(formname="lookup")))
    assert got["method"] == "GET"
    assert got["path"] == "/echo?q=first+page"


def test_a_multipart_form_is_sent_as_multipart(base):
    page = netweir.get(f"{base}/form")
    got = seen(netweir.submit(page.form(formnumber=2)))
    content_type = got["headers"]["content-type"]
    assert content_type.startswith("multipart/form-data; boundary=")
    boundary = content_type.split("boundary=")[1]
    assert (
        f'--{boundary}\r\nContent-Disposition: form-data; name="title"\r\n\r\nA book\r\n'
        in got["body"]
    )
    assert got["body"].endswith(f"--{boundary}--\r\n")


def parts(got):
    """The parts of a multipart body, as (headers, data)."""
    boundary = got["headers"]["content-type"].split("boundary=")[1]
    assert got["body"].endswith(f"--{boundary}--\r\n")
    chunks = got["body"].split(f"--{boundary}")[1:-1]
    out = []
    for chunk in chunks:
        head, _, data = chunk.removeprefix("\r\n").partition("\r\n\r\n")
        out.append((head, data.removesuffix("\r\n")))
    return out


def test_an_empty_file_input_is_sent_as_a_browser_sends_it(base):
    page = netweir.get(f"{base}/form")
    got = parts(seen(netweir.submit(page.form("#upload"))))
    assert got == [
        ('Content-Disposition: form-data; name="title"', "A book"),
        (
            'Content-Disposition: form-data; name="cover"; filename=""\r\n'
            "Content-Type: application/octet-stream",
            "",
        ),
        ('Content-Disposition: form-data; name="after"', "x"),
    ]


def test_a_file_from_disk_goes_where_its_input_is(base, tmp_path):
    cover = tmp_path / "cover.png"
    cover.write_bytes(b"\x89PNG\r\n\x00\xff")
    page = netweir.get(f"{base}/form")
    got = parts(seen(netweir.submit(page.form("#upload", files={"cover": cover}))))
    assert got[1] == (
        'Content-Disposition: form-data; name="cover"; filename="cover.png"\r\n'
        "Content-Type: image/png",
        "\x89PNG\r\n\x00\xff",
    )
    assert [p[1] for p in got] == ["A book", "\x89PNG\r\n\x00\xff", "x"]


def test_a_file_can_be_given_as_bytes_with_its_name_and_type(base):
    page = netweir.get(f"{base}/form")
    form = page.form(
        "#upload",
        files={"cover": [("a.bin", b"\x00\x01"), ('b "q".txt', b"hi", "text/x-thing")]},
    )
    got = parts(seen(netweir.submit(form)))
    assert got[1:3] == [
        (
            'Content-Disposition: form-data; name="cover"; filename="a.bin"\r\n'
            "Content-Type: application/octet-stream",
            "\x00\x01",
        ),
        (
            'Content-Disposition: form-data; name="cover"; filename="b %22q%22.txt"\r\n'
            "Content-Type: text/x-thing",
            "hi",
        ),
    ]


def test_a_form_that_isn_t_multipart_sends_only_the_file_s_name(base):
    page = netweir.get(f"{base}/form")
    got = seen(netweir.submit(page.form("#plain", files={"doc": ("notes.txt", b"secret")})))
    assert parse_qsl(got["body"], keep_blank_values=True) == [("doc", "notes.txt"), ("q", "y")]
    assert "secret" not in got["body"]


def test_from_form_takes_files_too(base):
    page = netweir.get(f"{base}/form")
    request = netweir.Request.from_form(page, "#upload", files={"cover": ("c.txt", b"data")})
    assert b'filename="c.txt"' in request.body


def test_a_content_type_a_browser_wouldn_t_send_is_sent_as_octet_stream(base):
    page = netweir.get(f"{base}/form")
    form = page.form("#upload", files={"cover": ("x.txt", b"d", "text/plain\r\nX-Evil: 1")})
    got = parts(seen(netweir.submit(form)))
    assert got[1][0].endswith("Content-Type: application/octet-stream")
    assert "X-Evil" not in got[1][0]


@pytest.mark.parametrize(
    ("filename", "content_type"),
    [
        ("a.tar.gz", "application/gzip"),
        ("notes.txt.gz", "application/gzip"),
        ("data:x.png", "image/png"),
        ("README", "application/octet-stream"),
    ],
)
def test_a_content_type_comes_from_the_last_extension(filename, content_type):
    file = netweir._form._file("f", (filename, b""))
    assert file.content_type == content_type


def test_an_empty_list_of_files_sends_the_input_empty(base):
    page = netweir.get(f"{base}/form")
    got = parts(seen(netweir.submit(page.form("#upload", files={"cover": []}))))
    assert 'name="cover"; filename=""' in got[1][0]


def test_bytes_like_data_and_a_file_are_taken(base):
    page = netweir.get(f"{base}/form")
    form = page.form(
        "#upload",
        files={
            "cover": [
                ("a", bytearray(b"1")),
                ("b", memoryview(b"2")),
                netweir.File("c", b"3", "text/plain"),
            ]
        },
    )
    assert [p[1] for p in parts(seen(netweir.submit(form)))[1:4]] == ["1", "2", "3"]


def test_a_file_that_isn_t_a_path_or_a_tuple_is_refused(base):
    page = netweir.get(f"{base}/form")
    with pytest.raises(TypeError, match="cover"):
        page.form("#upload", files={"cover": b"no name"})
    with pytest.raises(TypeError, match="data must be bytes"):
        page.form("#upload", files={"cover": ("x", "text, not bytes")})
    with pytest.raises(TypeError, match="filename must be a str"):
        page.form("#upload", files={"cover": (None, b"x")})


FAST = netweir.Settings(
    throttle=False, start_delay=0, obey_robots=False, backoff_base=0.01, backoff_max=0.02
)


class Stop(Exception):
    pass


def test_a_spider_submits_a_form_and_gets_the_result(base):
    got = []

    class Search(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/form"]

        def parse(self, page):
            yield netweir.Request.from_form(page, data={"q": "maps"}, callback=self.results)

        def results(self, page):
            got.append(seen(page))

    Search().run()
    assert len(got) == 1
    assert got[0]["method"] == "POST"
    assert ("q", "maps") in parse_qsl(got[0]["body"])
    assert got[0]["headers"]["referer"] == f"{base}/form"


def test_requests_differing_only_in_body_are_both_sent(base):
    bodies = []

    class Posts(netweir.Spider):
        settings = FAST

        def start(self):
            for q in ["a", "b", "a"]:
                yield netweir.Request(f"{base}/echo", method="POST", form={"q": q})

        def parse(self, page):
            bodies.append(seen(page)["body"])

    stats = Posts().run()
    assert sorted(bodies) == ["q=a", "q=b"]
    assert stats["duplicates"] == 1


def test_followed_links_say_where_they_came_from(base):
    got = []

    class Links(netweir.Spider):
        settings = FAST
        start_urls = [f"{base}/form"]

        def parse(self, page):
            yield page.follow("/echo?next", callback=self.next)

        def next(self, page):
            got.append(seen(page))

    Links().run()
    assert got[0]["headers"]["referer"] == f"{base}/form"
    assert got[0]["headers"]["sec-fetch-site"] == "same-origin"


def test_a_post_that_fails_on_the_server_is_only_sent_again_when_asked(base):
    def crawl(**kwargs):
        Handler.broken = 0

        class Broken(netweir.Spider):
            settings = netweir.Settings(
                throttle=False, start_delay=0, obey_robots=False, retries=2,
                backoff_base=0.01, backoff_max=0.02,
            )  # fmt: skip

            def start(self):
                yield netweir.Request(f"{base}/broken", method="POST", form={"a": 1}, **kwargs)

            def parse(self, page):
                pass

        Broken().run()
        return Handler.broken

    assert crawl() == 1
    assert crawl(retry_post=True) == 3


def test_only_a_get_can_go_to_chrome():
    with pytest.raises(ValueError, match="only a GET"):
        netweir.Request("https://example.com/", method="POST", form={"a": 1}, browser=True)


def test_an_interrupted_crawl_sends_its_posts_again_as_posts(base, tmp_path):
    seen_bodies = []
    stop = [True]

    class Searches(netweir.Spider):
        start_urls = [f"{base}/form"]

        def parse(self, page):
            for q in ["one", "two", "three"]:
                yield netweir.Request.from_form(page, data={"q": q}, callback="result")

        def result(self, page):
            if stop[0]:
                raise Stop
            got = seen(page)
            seen_bodies.append((got["method"], dict(parse_qsl(got["body"]))["q"]))

    spider = Searches()
    spider.settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, fail_fast=True,
        checkpoint=str(tmp_path / "state"), concurrency=1,
    )  # fmt: skip
    with pytest.raises(Stop):
        spider.run()
    stop[0] = False
    spider = Searches()
    spider.settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, checkpoint=str(tmp_path / "state")
    )
    spider.run()
    assert sorted(seen_bodies) == [("POST", "one"), ("POST", "three"), ("POST", "two")]


EDGE_FORM = """<!doctype html><html><head><base href="https://cdn.example/assets/"></head><body>
<form method="post">
  <fieldset disabled><input name="locked" value="1"><legend><input name="legend" value="2"></legend></fieldset>
  <input type="checkbox" name="empty" value="" checked>
  <select name="one"><option selected>a</option><option selected>b</option></select>
  <input type="radio" name="r" value="x" checked><input type="radio" name="r" value="y" checked>
  <textarea name="lines">a
b</textarea>
  <button type="submit" name="go" value="1" formaction="/elsewhere" formmethod="get">Go</button>
</form>
</body></html>"""


def edge_page(url="https://shop.example/basket"):
    return _page(EDGE_FORM, url)


def _page(html, url):
    from netweir._native import _response

    return netweir.Page(
        _response(url, 200, "HTTP/1.1", [("content-type", "text/html")], html.encode())
    )


def test_form_reading_follows_the_html_standard_at_the_edges():
    page = edge_page()
    form = page.form(click=False)
    fields = form.fields
    # A disabled fieldset disables its controls, except in its first legend.
    assert ("locked", "1") not in fields and ("legend", "2") in fields
    # A checked box with an empty value sends the empty value.
    assert ("empty", "") in fields
    # One option per single select, and one radio per group: the last.
    assert [v for k, v in fields if k == "one"] == ["b"]
    assert [v for k, v in fields if k == "r"] == ["y"]
    # An empty action is the page's own URL, not <base href>'s.
    assert form.action == "https://shop.example/basket"
    assert form.referer == "https://shop.example/basket"


def test_line_breaks_are_sent_as_crlf():
    form = edge_page().form(click=False)
    _, body, _ = form.encoded()
    assert b"lines=a%0D%0Ab" in body


def test_the_pressed_buttons_formaction_and_formmethod_win():
    form = edge_page().form(click="go")
    assert (form.method, form.action) == ("GET", "https://cdn.example/elsewhere")


def test_a_body_needs_a_method_that_can_carry_one():
    with pytest.raises(ValueError, match="POST"):
        netweir.Request("https://example.com/", method="GET", form={"a": 1})
    # Leaving the method out with a body means POST.
    assert netweir.Request("https://example.com/", form={"a": 1}).method == "POST"
    assert netweir.Request("https://example.com/").method == "GET"


def test_a_response_over_the_size_limit_fails(base):
    with pytest.raises(netweir.FetchError) as caught:
        netweir.get(f"{base}/big", max_size=10_000)
    assert caught.value.kind == "too_large"
    assert len(netweir.get(f"{base}/big").body) == 50_000


def test_a_crawl_stops_reading_a_response_over_its_limit(base):
    errors = []

    class Big(netweir.Spider):
        settings = netweir.Settings(
            throttle=False, start_delay=0, obey_robots=False, max_response_size=10_000
        )

        def start(self):
            yield netweir.Request(f"{base}/big", errback=self.lost)

        def parse(self, page):
            pass

        def lost(self, request, error):
            errors.append(error.kind)

    Big().run()
    assert errors == ["too_large"]
