"""WARC and HAR files of what a crawl fetched, read back with the tools
people use on them."""

import base64
import gzip
import json
import os
import subprocess
import sys
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import jsonschema
import pytest
from warcio.archiveiterator import ArchiveIterator

import netweir

PNG = b"\x89PNG\r\n\x1a\n\x00\xff\xfe binary"
COMPRESSED = b"<p>" + b"squeezed " * 200 + b"</p>"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def answer(self, status, body, headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        p = self.path
        html = [("Content-Type", "text/html; charset=utf-8")]
        if p == "/robots.txt":
            self.answer(200, b"User-agent: *\nAllow: /\n", [("Content-Type", "text/plain")])
        elif p == "/":
            links = "".join(f'<a href="{h}">x</a>' for h in ("/page", "/img.png", "/gz", "/go"))
            self.answer(200, links.encode(), html)
        elif p == "/go":
            self.answer(302, b"", [("Location", "/moved")])
        elif p == "/img.png":
            self.answer(200, PNG, [("Content-Type", "image/png")])
        elif p == "/gz":
            self.answer(200, gzip.compress(COMPRESSED), [*html, ("Content-Encoding", "gzip")])
        elif p.startswith(("/page", "/moved", "/book/")):
            self.answer(200, f"<h1>{p}</h1>".encode(), html)
        elif p == "/odd/empty":
            self.send_response(204)
            self.end_headers()
        elif p == "/odd/unchanged":
            self.send_response(304)
            self.send_header("ETag", '"v1"')
            self.end_headers()
        elif p == "/odd/old":
            # This one answer in HTTP/1.0, which ends with the connection.
            self.protocol_version = "HTTP/1.0"
            self.answer(200, b"from the past", html)
        elif p == "/odd/worded":
            self.send_response(200, "Fine Thanks")
            self.send_header("X-Name", "caf\u00e9")  # sent as Latin-1
            self.send_header("Set-Cookie", "a=1")
            self.send_header("X-Between", "yes")
            self.send_header("Set-Cookie", "b=2")
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
        elif p == "/odd/chunked":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for part in (b"in ", b"three ", b"parts"):
                self.wfile.write(b"%x\r\n%s\r\n" % (len(part), part))
            self.wfile.write(b"0\r\n\r\n")
        elif p == "/odd/endless":
            # A redirect whose body never ends.
            self.send_response(302)
            self.send_header("Location", "/page/after-endless")
            self.send_header("Connection", "close")
            self.end_headers()
            until = time.monotonic() + 20
            try:
                while time.monotonic() < until:
                    self.wfile.write(b"x" * 16384)
            except OSError:
                pass
        else:
            self.answer(404, b"not here")

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", "1234")
        self.end_headers()


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


def settings(**kw):
    return netweir.Settings(throttle=False, start_delay=0, **kw)


class Site(netweir.Spider):
    def parse(self, page):
        if page.url.endswith("/"):
            for href in page.css("a::attr(href)").getall():
                yield page.follow(href)
        yield {"url": page.url}


def crawl(base, **kw):
    spider = Site()
    spider.start_urls = [base + "/"]
    spider.settings = settings(**kw)
    return spider.run()


def warc_records(path):
    """(record, content) for every record, digests checked by warcio."""
    out = []
    with open(path, "rb") as f:
        for record in ArchiveIterator(f, check_digests=True):
            content = record.content_stream().read()
            if record.rec_type != "warcinfo":
                assert record.digest_checker.passed is True, record.rec_headers
                assert not record.digest_checker.problems
            out.append((record, content))
    return out


def responses(records):
    return {
        r.rec_headers.get_header("WARC-Target-URI"): (r, content)
        for r, content in records
        if r.rec_type == "response"
    }


@pytest.mark.parametrize("name", ["crawl.warc.gz", "crawl.warc"])
def test_every_response_reads_back_with_warcio(base, tmp_path, name):
    stats = crawl(base, warc=str(tmp_path / name))
    assert stats["archive_errors"] == 0
    records = warc_records(tmp_path / name)
    first, info = records[0]
    assert first.rec_type == "warcinfo"
    assert f"software: netweir/{netweir.__version__}".encode() in info
    got = responses(records)
    for path in ("/robots.txt", "/.well-known/tdmrep.json", "/", "/page", "/img.png", "/gz"):
        assert base + path in got, path
    # The redirect and where it led, each a request of its own.
    hop, _ = got[base + "/go"]
    assert hop.http_headers.get_statuscode() == "302"
    assert hop.http_headers.get_header("location") == "/moved"
    assert base + "/moved" in got
    assert got[base + "/.well-known/tdmrep.json"][0].http_headers.get_statuscode() == "404"
    assert got[base + "/img.png"][1] == PNG
    # Kept decompressed, and said so by the headers.
    gz, body = got[base + "/gz"]
    assert body == COMPRESSED
    assert gz.http_headers.get_header("content-encoding") is None
    assert gz.http_headers.get_header("content-length") == str(len(COMPRESSED))

    requests = {
        r.rec_headers.get_header("WARC-Concurrent-To"): r
        for r, _ in records
        if r.rec_type == "request"
    }
    assert len(requests) == len(got)
    for uri, (response, _) in got.items():
        request = requests[response.rec_headers.get_header("WARC-Record-ID")]
        assert request.rec_headers.get_header("WARC-Target-URI") == uri
        assert request.http_headers.protocol == "GET"
        assert "Chrome/" in request.http_headers.get_header("User-Agent")
        assert request.http_headers.get_header("Host") == base.removeprefix("http://")


# The fields HAR 1.2 requires, as a schema.
NAME_VALUE = {
    "type": "array",
    "items": {
        "type": "object",
        "required": ["name", "value"],
        "properties": {"name": {"type": "string"}, "value": {"type": "string"}},
    },
}
HAR_SCHEMA = {
    "type": "object",
    "required": ["log"],
    "properties": {
        "log": {
            "type": "object",
            "required": ["version", "creator", "entries"],
            "properties": {
                "version": {"const": "1.2"},
                "creator": {
                    "type": "object",
                    "required": ["name", "version"],
                    "properties": {"name": {"type": "string"}, "version": {"type": "string"}},
                },
                "entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": [
                            "startedDateTime",
                            "time",
                            "request",
                            "response",
                            "cache",
                            "timings",
                        ],
                        "properties": {
                            "startedDateTime": {"type": "string", "format": "date-time"},
                            "time": {"type": "number", "minimum": 0},
                            "request": {
                                "type": "object",
                                "required": [
                                    "method",
                                    "url",
                                    "httpVersion",
                                    "cookies",
                                    "headers",
                                    "queryString",
                                    "headersSize",
                                    "bodySize",
                                ],
                                "properties": {
                                    "method": {"type": "string"},
                                    "url": {"type": "string"},
                                    "httpVersion": {"type": "string"},
                                    "cookies": {"type": "array"},
                                    "headers": NAME_VALUE,
                                    "queryString": NAME_VALUE,
                                    "headersSize": {"type": "integer"},
                                    "bodySize": {"type": "integer"},
                                },
                            },
                            "response": {
                                "type": "object",
                                "required": [
                                    "status",
                                    "statusText",
                                    "httpVersion",
                                    "cookies",
                                    "headers",
                                    "content",
                                    "redirectURL",
                                    "headersSize",
                                    "bodySize",
                                ],
                                "properties": {
                                    "status": {"type": "integer"},
                                    "statusText": {"type": "string"},
                                    "httpVersion": {"type": "string"},
                                    "cookies": {"type": "array"},
                                    "headers": NAME_VALUE,
                                    "content": {
                                        "type": "object",
                                        "required": ["size", "mimeType"],
                                        "properties": {
                                            "size": {"type": "integer"},
                                            "mimeType": {"type": "string"},
                                            "text": {"type": "string"},
                                            "encoding": {"type": "string"},
                                        },
                                    },
                                    "redirectURL": {"type": "string"},
                                    "headersSize": {"type": "integer"},
                                    "bodySize": {"type": "integer"},
                                },
                            },
                            "cache": {"type": "object"},
                            "timings": {
                                "type": "object",
                                "required": ["send", "wait", "receive"],
                                "properties": {
                                    k: {"type": "number", "minimum": -1}
                                    for k in (
                                        "blocked",
                                        "dns",
                                        "connect",
                                        "send",
                                        "wait",
                                        "receive",
                                        "ssl",
                                    )
                                },
                            },
                        },
                    },
                },
            },
        }
    },
}


def har_entries(path):
    with open(path, encoding="utf-8") as f:
        har = json.load(f)
    jsonschema.validate(har, HAR_SCHEMA)
    return har["log"]["entries"]


def test_the_har_file_has_what_har_1_2_requires(base, tmp_path):
    crawl(base, har=str(tmp_path / "crawl.har"))
    entries = {e["request"]["url"]: e for e in har_entries(tmp_path / "crawl.har")}
    assert base + "/robots.txt" in entries
    image = entries[base + "/img.png"]["response"]["content"]
    assert image["encoding"] == "base64"
    assert base64.b64decode(image["text"]) == PNG
    assert image["mimeType"] == "image/png"
    page = entries[base + "/gz"]["response"]["content"]
    assert page["text"] == COMPRESSED.decode()
    assert "encoding" not in page
    hop = entries[base + "/go"]["response"]
    assert (hop["status"], hop["redirectURL"]) == (302, "/moved")


def test_pages_that_never_reach_python_are_recorded(base, tmp_path):
    class Heading(netweir.Item):
        title = netweir.css("h1::text")

    class Rules(netweir.Spider):
        start_urls = [base + "/"]
        rules = [netweir.Follow("a", extract=Heading)]

    Rules.settings = settings(warc=str(tmp_path / "rules.warc.gz"))
    stats = Rules().run()
    assert stats["items"] >= 3
    got = responses(warc_records(tmp_path / "rules.warc.gz"))
    for path in ("/", "/page", "/img.png", "/moved"):
        assert base + path in got, path


def test_a_resumed_crawl_adds_to_both_files_and_both_stay_readable(base, tmp_path):
    warc, har = tmp_path / "crawl.warc.gz", tmp_path / "crawl.har"
    pages = 0

    class Stop(Exception):
        pass

    class Dies(Site):
        def parse(self, page):
            nonlocal pages
            pages += 1
            if pages == 3:
                raise Stop
            yield from super().parse(page)

    spider = Dies()
    spider.start_urls = [base + "/"]
    spider.settings = settings(
        warc=str(warc), har=str(har), checkpoint=str(tmp_path / "state"), fail_fast=True
    )
    with pytest.raises(Stop):
        spider.run()
    first = len(warc_records(warc))
    first_entries = har_entries(har)
    # As if the process died part way through a record.
    with open(warc, "ab") as f:
        f.write(gzip.compress(b"WARC/1.1\r\nWARC-Type: response\r\n")[:20])
    spider = Site()
    spider.start_urls = [base + "/"]
    spider.settings = settings(warc=str(warc), har=str(har), checkpoint=str(tmp_path / "state"))
    spider.run()
    records = warc_records(warc)
    assert len(records) > first
    assert [r.rec_type for r, _ in records].count("warcinfo") == 2
    assert base + "/gz" in responses(records)
    entries = har_entries(har)
    assert entries[: len(first_entries)] == first_entries
    assert len(entries) > len(first_entries)


def test_cache_hits_are_not_fetched_so_not_recorded(base, tmp_path):
    kw = {"cache": str(tmp_path / "cache"), "obey_robots": False, "obey_tdmrep": False}
    crawl(base, warc=str(tmp_path / "first.warc"), **kw)
    stats = crawl(base, warc=str(tmp_path / "again.warc"), **kw)
    assert stats["cache_hits"] > 0
    # Every page came from the cache: the file has the run's warcinfo and
    # nothing else.
    records = warc_records(tmp_path / "again.warc")
    assert [r.rec_type for r, _ in records] == ["warcinfo"]


def test_settings_check_the_files_before_the_crawl_starts(tmp_path):
    with pytest.raises(ValueError, match=r"\.warc\.gz or \.warc"):
        netweir.Settings(warc=str(tmp_path / "crawl.zip"))
    with pytest.raises(ValueError, match="directory"):
        netweir.Settings(har=str(tmp_path / "missing" / "crawl.har"))
    with pytest.raises(ValueError, match="directory"):
        netweir.Settings(warc=str(tmp_path / "missing" / "crawl.warc"))
    s = netweir.Settings(warc=tmp_path / "crawl.warc.gz", har=tmp_path / "crawl.har")
    assert s.warc == os.fspath(tmp_path / "crawl.warc.gz")
    assert isinstance(s.har, str)


def raw_responses(path):
    """Each response record's HTTP head, from the file's own bytes."""
    with open(path, "rb") as f:
        data = f.read()
    heads = {}
    for record in data.split(b"WARC/1.1\r\n")[1:]:
        warc_head, _, block = record.partition(b"\r\n\r\n")
        if b"WARC-Type: response" in warc_head:
            uri = warc_head.split(b"WARC-Target-URI: ")[1].split(b"\r\n")[0].decode()
            heads[uri] = block.partition(b"\r\n\r\n")[0]
    return heads


def test_odd_answers_are_recorded_as_they_came(base, tmp_path):
    class Odd(netweir.Spider):
        async def start(self):
            for path in ("empty", "unchanged", "old", "worded", "chunked"):
                yield netweir.Request(f"{base}/odd/{path}")
            yield netweir.Request(f"{base}/page/head", method="HEAD")

        def parse(self, page):
            pass

    spider = Odd()
    spider.settings = settings(obey_robots=False, obey_tdmrep=False, warc=str(tmp_path / "o.warc"))
    spider.run()
    got = responses(warc_records(tmp_path / "o.warc"))
    heads = raw_responses(tmp_path / "o.warc")

    def lines(path):
        return heads[base + path].split(b"\r\n")

    # No body to measure: no Content-Length is made up.
    for path, status in (("/odd/empty", b"204"), ("/odd/unchanged", b"304")):
        head = lines(path)
        assert head[0].startswith(b"HTTP/1.1 " + status)
        assert not any(h.lower().startswith(b"content-length") for h in head), head
    # A HEAD keeps the length the server gave, with nothing stored.
    head, body = got[base + "/page/head"]
    assert head.http_headers.get_header("content-length") == "1234"
    assert body == b""
    # HTTP/1.0 says so, in the status line and the record.
    old, body = got[base + "/odd/old"]
    assert lines("/odd/old")[0] == b"HTTP/1.0 200 OK"
    assert old.rec_headers.get_header("WARC-Protocol") == "http/1.0"
    assert body == b"from the past"
    # The server's own reason phrase, and header values byte for byte.
    worded = lines("/odd/worded")
    assert worded[0] == b"HTTP/1.1 200 Fine Thanks"
    assert b"x-name: caf\xe9" in worded
    # Repeated names come together, as the client keeps them.
    cookies = [i for i, h in enumerate(worded) if h.startswith(b"set-cookie")]
    assert cookies == [cookies[0], cookies[0] + 1]
    # Dechunked: the length of what's stored, and no Transfer-Encoding.
    chunked, body = got[base + "/odd/chunked"]
    assert body == b"in three parts"
    assert chunked.http_headers.get_header("content-length") == str(len(body))
    assert chunked.http_headers.get_header("transfer-encoding") is None


def test_a_redirect_body_that_never_ends_holds_nothing_up(base, tmp_path):
    for warc in (None, str(tmp_path / "e.warc.gz")):

        class Endless(netweir.Spider):
            start_urls = [base + "/odd/endless"]

            def parse(self, page):
                yield {"url": page.url}

        spider = Endless()
        spider.settings = settings(obey_robots=False, obey_tdmrep=False, timeout=15, warc=warc)
        started = time.monotonic()
        items = []
        spider.run(output=items.append)
        assert time.monotonic() - started < 5
        assert items == [{"url": base + "/page/after-endless"}]
    hop, body = responses(warc_records(tmp_path / "e.warc.gz"))[base + "/odd/endless"]
    assert hop.rec_headers.get_header("WARC-Truncated") == "length"
    assert body == b"x" * len(body) and 0 < len(body) <= 64 * 1024


def test_a_crawl_that_cannot_start_leaves_its_archive_alone(base, tmp_path):
    warc, har = tmp_path / "kept.warc", tmp_path / "kept.har"
    warc.write_bytes(b"WARC/1.1\r\nfrom before\r\n")
    har.write_text("{}", encoding="utf-8")

    class Broken:
        def open_spider(self, spider):
            raise RuntimeError("no database")

    class Any(netweir.Spider):
        start_urls = [base + "/"]
        pipelines = [Broken()]

    spider = Any()
    spider.settings = settings(warc=str(warc), har=str(har))
    with pytest.raises(RuntimeError, match="no database"):
        spider.run()
    assert warc.read_bytes() == b"WARC/1.1\r\nfrom before\r\n"
    assert har.read_text(encoding="utf-8") == "{}"


def test_warc_and_har_cannot_share_a_file(tmp_path):
    with pytest.raises(ValueError, match="file of its own"):
        netweir.Settings(warc=str(tmp_path / "a.warc"), har=str(tmp_path / "a.warc"))
    os.symlink(tmp_path / "a.warc", tmp_path / "b.har")
    with pytest.raises(ValueError, match="file of its own"):
        netweir.Settings(warc=str(tmp_path / "a.warc"), har=str(tmp_path / "b.har"))
    # One file on a case-insensitive disk.
    with pytest.raises(ValueError, match="file of its own"):
        netweir.Settings(warc=str(tmp_path / "a.warc"), har=str(tmp_path / "A.WARC"))


MEMORY = """
import os, resource, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import netweir
import http.server
from netweir import _serve

# Without http.server's lookup of a name (conftest.py).
http.server.HTTPServer.server_bind = _serve._Server.server_bind

SIZE = int(sys.argv[2])
BODY = bytes(range(256)) * (SIZE // 256)

class Big(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)

server = ThreadingHTTPServer(("127.0.0.1", 0), Big)
threading.Thread(target=server.serve_forever, daemon=True).start()

class One(netweir.Spider):
    start_urls = [f"http://127.0.0.1:{server.server_port}/big"]
    def parse(self, page):
        yield {"n": len(page.body)}

def peak():
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return rss if sys.platform == "darwin" else rss * 1024

out = sys.argv[1]
kw = {} if out == "-" else {"warc": out + ".warc.gz", "har": out + ".har"}
before = peak()
spider = One()
spider.settings = netweir.Settings(
    throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, **kw
)
spider.run()
print(peak() - before)
"""


@pytest.mark.skipif(sys.platform == "win32", reason="peak memory is read with resource")
def test_a_large_body_is_written_without_copies_of_it(tmp_path):
    size = 32 * 1024 * 1024

    def growth(out):
        run = subprocess.run(
            [sys.executable, "-c", textwrap.dedent(MEMORY), out, str(size)],
            capture_output=True,
            text=True,
            timeout=300,
        )
        assert run.returncode == 0, run.stderr
        return int(run.stdout.split()[-1])

    without = growth("-")
    archived = growth(str(tmp_path / "big"))
    # The archive streams from the buffer the crawl already holds: what it
    # adds is well under one more copy of the body.
    assert archived - without < size / 2, (without, archived)
    records = warc_records(tmp_path / "big.warc.gz")
    assert len(responses(records)[next(iter(responses(records)))][1]) == size
    with open(tmp_path / "big.har", encoding="utf-8") as f:
        (entry,) = json.load(f)["log"]["entries"]
    assert len(base64.b64decode(entry["response"]["content"]["text"])) == size
