"""The cache: a spider run again is served from disk, not the site."""

import collections
import contextlib
import shutil
import sqlite3
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)

BLOCK = b"<title>Attention Required! | Cloudflare</title><h1>Sorry, you have been blocked</h1>"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    asked: collections.Counter = collections.Counter()
    served: collections.Counter = collections.Counter()

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
        Handler.asked[self.path] += 1
        Handler.served[self.path] += 1
        n = Handler.served[self.path]
        prefix, _, last = self.path.rpartition("/")
        if self.path == "/go1":
            # A fixed target: nothing from the request reaches the header.
            self.answer(302, b"", [("Location", "/there1")])
        elif self.path.startswith("/busy"):
            self.answer(503, b"busy")
        elif self.path.startswith("/blocked"):
            self.answer(403, BLOCK, [("Server", "cloudflare")])
        elif self.path.startswith("/tree/"):
            # Page n links to 2n and 2n+1, forever.
            n = int(last)
            body = f'<a href="{prefix}/{2 * n}">a</a><a href="{prefix}/{2 * n + 1}">b</a>'
            self.answer(200, body.encode(), [("Content-Type", "text/html")])
        elif self.path.startswith("/few/"):
            # Page 1 links to 2 to 5, which link nowhere.
            links = "".join(f'<a href="{prefix}/{i}">x</a>' for i in range(2, 6))
            body = links if last == "1" else f"leaf {last}"
            self.answer(200, body.encode(), [("Content-Type", "text/html")])
        else:
            body = f"<title>{self.path}</title> version {n}".encode()
            self.answer(200, body, [("Content-Type", "text/html"), ("X-Made", "here")])

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        Handler.asked[self.path] += 1
        self.answer(200, b"you sent " + body, [("Content-Type", "text/plain")])


@pytest.fixture(scope="module")
def server():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


@pytest.fixture
def base(server):
    Handler.asked.clear()
    Handler.served.clear()
    return server


def asked():
    return sum(Handler.asked.values())


def crawl(urls, cache, follow=False, requests=(), **settings):
    """Runs a spider over `urls` and returns its stats and the pages its
    callback saw, as (url, status, headers, text)."""
    seen = []

    class Spider(netweir.Spider):
        start_urls = list(urls)

        async def start(self):
            for url in self.start_urls:
                yield netweir.Request(url)
            for request in requests:
                yield request

        def parse(self, page):
            seen.append((page.url, page.status, dict(page.headers), page.text))
            if follow:
                yield from page.follow_all(page.css("a::attr(href)"))

    Spider.settings = netweir.Settings(**{**vars(FAST), "cache": str(cache), **settings})
    spider = Spider()
    stats = spider.run()
    return stats, sorted(seen, key=lambda p: (p[0], p[3])), spider


def test_a_rerun_asks_the_site_nothing_and_sees_the_same_pages(base, tmp_path):
    urls = [f"{base}/a", f"{base}/b"]
    stats, first, _ = crawl(urls, tmp_path / "cache")
    assert (stats["cache_hits"], stats["cache_stores"]) == (0, 2)
    assert asked() == 2
    stats, second, _ = crawl(urls, tmp_path / "cache")
    assert asked() == 2
    assert second == first
    assert (stats["cache_hits"], stats["cache_stores"], stats["fetched"]) == (2, 0, 2)
    assert (tmp_path / "cache" / "cache.sqlite3").exists()


def test_the_cache_lets_go_of_its_file_when_the_crawl_ends(base, tmp_path):
    cache = tmp_path / "cache"
    crawl([f"{base}/a"], cache)
    # The last connection to close removes the write-ahead log; Windows
    # won't delete a directory with a file still open in it.
    assert not (cache / "cache.sqlite3-wal").exists()
    shutil.rmtree(cache)


def database(cache):
    return contextlib.closing(sqlite3.connect(cache / "cache.sqlite3"))


def test_a_stale_page_is_fetched_again(base, tmp_path):
    url = [f"{base}/stale"]
    cache = tmp_path / "cache"
    crawl(url, cache, cache_expiry=600)
    _, fresh, _ = crawl(url, cache, cache_expiry=600)
    assert "version 1" in fresh[0][3]
    # Ten minutes on, without the wait.
    with database(cache) as db, db:
        db.execute("UPDATE responses SET stored_at = stored_at - 601")
    _, stale, _ = crawl(url, cache, cache_expiry=600)
    assert "version 2" in stale[0][3]
    assert asked() == 2


def test_server_errors_and_blocks_are_not_kept(base, tmp_path):
    urls = [f"{base}/busy", f"{base}/blocked"]
    for _ in range(2):
        stats, _, _ = crawl(urls, tmp_path / "cache", retries=0)
        assert (stats["cache_hits"], stats["cache_stores"]) == (0, 0)
    assert Handler.asked["/busy"] == 2
    assert Handler.asked["/blocked"] == 2


def test_a_redirect_is_served_as_where_it_ended(base, tmp_path):
    crawl([f"{base}/go1"], tmp_path / "cache")
    assert asked() == 2
    stats, pages, _ = crawl([f"{base}/go1"], tmp_path / "cache")
    assert asked() == 2
    assert pages[0][0] == f"{base}/there1"
    assert stats["cache_hits"] == 1


def test_posts_with_different_bodies_are_kept_apart(base, tmp_path):
    def searches():
        return [
            netweir.Request(f"{base}/search", method="POST", form={"q": term})
            for term in ("rust", "python")
        ]

    _, first, _ = crawl([], tmp_path / "cache", requests=searches())
    stats, second, _ = crawl([], tmp_path / "cache", requests=searches())
    assert asked() == 2
    assert stats["cache_hits"] == 2
    assert [p[3] for p in second] == ["you sent q=python", "you sent q=rust"]
    assert second == first


def test_max_pages_counts_pages_from_the_cache(base, tmp_path):
    # Every page to depth 5 is cached, so which 20 the second run takes
    # doesn't matter.
    crawl([f"{base}/tree/1"], tmp_path / "cache", follow=True, max_depth=5)
    assert asked() == 63
    stats, pages, spider = crawl(
        [f"{base}/tree/1"], tmp_path / "cache", follow=True, max_depth=5, max_pages=20
    )
    assert asked() == 63
    assert (stats["fetched"], stats["cache_hits"], len(pages)) == (20, 20, 20)
    assert spider.finish_reason == "max_pages"


def test_a_checkpoint_works_the_same_with_the_cache(base, tmp_path):
    start = [f"{base}/few/1"]
    cache = tmp_path / "cache"
    stats, first, _ = crawl(start, cache, follow=True, checkpoint=str(tmp_path / "one"))
    assert (len(first), asked()) == (5, 5)
    stats, again, _ = crawl(start, cache, follow=True, checkpoint=str(tmp_path / "two"))
    assert (again, asked(), stats["cache_hits"]) == (first, 5, 5)
    # The hits were acked: the second checkpoint has nothing left to do.
    _, left, _ = crawl(start, cache, follow=True, checkpoint=str(tmp_path / "two"))
    assert (left, asked()) == ([], 5)


def test_a_cache_that_breaks_mid_crawl_is_warned_about_once(base, tmp_path, caplog):
    cache = tmp_path / "cache"
    seen = []

    class Breaks(netweir.Spider):
        settings = netweir.Settings(**{**vars(FAST), "cache": str(cache)})
        start_urls = [f"{base}/first"]

        def parse(self, page):
            seen.append(page.url)
            if page.url.endswith("/first"):
                # As another process or a failing disk might.
                with database(cache) as db, db:
                    db.execute("DROP TABLE responses")
                yield page.follow("/second")
                yield page.follow("/third")

    with caplog.at_level("WARNING", logger="netweir"):
        stats = Breaks().run()
    assert len(seen) == 3, "the crawl carries on"
    warned = [r.getMessage() for r in caplog.records if "cache" in r.getMessage()]
    assert len(warned) == 1, warned
    assert "couldn't be written" in warned[0]
    assert str(cache / "cache.sqlite3") in warned[0]
    assert stats["cache_stores"] == 1


def test_the_cache_is_a_setting_from_the_shell():
    from netweir._cli import _parse_setting

    fields = {f.name: f for f in netweir.Settings.__dataclass_fields__.values()}
    assert _parse_setting("cache=.cache", fields) == ("cache", ".cache")
    assert _parse_setting("cache_expiry=3600", fields) == ("cache_expiry", 3600.0)


@pytest.mark.parametrize("expiry", [0, -1])
def test_cache_expiry_must_be_positive(expiry):
    with pytest.raises(ValueError, match="cache_expiry"):
        netweir.Settings(cache_expiry=expiry)
