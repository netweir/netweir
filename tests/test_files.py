"""netweir.files and netweir.images: what items link to, downloaded
through the crawl and stored once."""

import asyncio
import collections
import dataclasses
import hashlib
import io
import logging
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

import pytest

import netweir
from netweir._files import _extension_of_type, _extension_of_url, _Store

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    asked: collections.Counter = collections.Counter()
    #: The headers of the last request for each path.
    headers_of: dict = {}
    #: Requests being answered now, and the most there were at once.
    active = 0
    peak = 0
    #: Image bodies by path, made by the tests that need them.
    pictures: dict = {}
    #: Set to let /slow/ answer.
    release = threading.Event()
    # The server answers in threads, and += isn't atomic without the GIL.
    lock = threading.Lock()

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
        with Handler.lock:
            Handler.asked[self.path] += 1
            Handler.headers_of[self.path] = dict(self.headers)
            Handler.active += 1
            Handler.peak = max(Handler.peak, Handler.active)
        try:
            self.route()
        finally:
            with Handler.lock:
                Handler.active -= 1

    def route(self):
        path = self.path
        if path == "/robots.txt":
            self.answer(
                200, b"User-agent: *\nDisallow: /private/\n", [("Content-Type", "text/plain")]
            )
        elif path.startswith("/page"):
            self.answer(200, b"<title>a page</title>", [("Content-Type", "text/html")])
        elif path == "/list":
            self.answer(200, b'<a href="/product">p</a>', [("Content-Type", "text/html")])
        elif path == "/product":
            # The Host header only reaches a body, never a Location.
            link = f"http://{self.headers['Host']}/file/rule.pdf"
            body = f'<h1>p</h1><a class="pdf" href="{link}">pdf</a>'.encode()
            self.answer(200, body, [("Content-Type", "text/html")])
        elif path.startswith("/slow/"):
            # Held until the test lets it go, as a big file is.
            Handler.release.wait(30)
            self.answer(200, b"%PDF-1.4 slow", [("Content-Type", "application/pdf")])
        elif path.startswith(("/file/", "/private/")):
            # Slow enough for requests at once to overlap.
            time.sleep(0.05)
            self.answer(200, b"%PDF-1.4 " + path.encode(), [("Content-Type", "application/pdf")])
        elif path.startswith("/noext/"):
            self.answer(200, b"%PDF-1.4 no extension", [("Content-Type", "application/pdf")])
        elif path == "/to-ftp.pdf":
            # A fixed target: nothing from the request reaches the header.
            self.answer(302, b"", [("Location", "ftp://127.0.0.1/x.pdf")])
        elif path in Handler.pictures:
            self.answer(200, Handler.pictures[path], [("Content-Type", "image/png")])
        else:
            self.answer(404, b"not here", [("Content-Type", "text/plain")])


@pytest.fixture(scope="module")
def server():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


@pytest.fixture
def base(server):
    with Handler.lock:
        Handler.asked.clear()
        Handler.headers_of.clear()
        Handler.peak = 0
    return server


def spider(base, items, stage, settings=FAST, pages=1, pause=0.0):
    """A spider whose pages /page0, /page1 ... each give items[n], through
    ``stage`` and into ``out``, each after ``pause`` seconds."""
    out = []
    seen = []

    class Shop(netweir.Spider):
        start_urls = [f"{base}/page{n}" for n in range(pages)]

        def parse(self, page):
            # Holds the crawl loop, as a slow callback does, while the
            # engine fetches on.
            time.sleep(pause)
            seen.append(page.url)
            n = int(page.url.rsplit("page", 1)[1])
            yield from (dict(item) for item in items[n])

    Shop.settings = settings
    Shop.pipelines = [stage, lambda item: out.append(item) or item]
    shop = Shop()
    shop.out = out
    shop.seen = seen
    return shop


def sha1(url):
    return hashlib.sha1(url.encode()).hexdigest()


def md5(data):
    return hashlib.md5(data).hexdigest()


def test_a_file_is_stored_under_its_hashed_path(base, tmp_path):
    url = f"{base}/file/a.pdf"
    shop = spider(base, [[{"name": "a", "file_urls": [url]}]], netweir.files(tmp_path))
    stats = shop.run()
    path = f"full/{sha1(url)}.pdf"
    body = (tmp_path / path).read_bytes()
    assert body == b"%PDF-1.4 /file/a.pdf"
    assert shop.out[0]["files"] == [
        {"url": url, "path": path, "checksum": md5(body), "status": "downloaded"}
    ]
    assert stats["files_downloaded"] == 1 and stats["files_failed"] == 0
    # The file was no page: parse saw only the page.
    assert shop.seen == [f"{base}/page0"]


def test_the_extension_comes_from_the_content_type_without_one_in_the_url(base, tmp_path):
    url = f"{base}/noext/report"
    items = [[{"file_urls": [url]}]]
    spider(base, items, netweir.files(tmp_path)).run()
    assert (tmp_path / f"full/{sha1(url)}.pdf").exists()
    # And is found again by the next run.
    again = spider(base, items, netweir.files(tmp_path))
    again.run()
    assert again.out[0]["files"][0]["status"] == "uptodate"
    assert Handler.asked["/noext/report"] == 1


def test_extensions_are_short_known_and_alphanumeric():
    assert _extension_of_url("http://h/a/B.PDF") == "pdf"
    assert _extension_of_url("http://h/a/b.php?id=3") == ""
    # Netweir's own table, the same on every Python: 3.10's mimetypes
    # knows neither of these, 3.14's knows both.
    assert _extension_of_url("http://h/a/b.webp") == "webp"
    assert _extension_of_url("http://h/a/b.avif") == "avif"
    assert _extension_of_type("image/webp") == "webp"
    assert _extension_of_type("image/jpeg; charset=binary") == "jpg"
    assert _extension_of_type("application/x-unheard-of") == ""
    assert _extension_of_url("http://h/a/b") == ""
    assert _extension_of_url("http://h/a/b.pdf%2F..%2F..%2Fx") == ""
    assert _extension_of_url("http://h/%2E%2E/%2E%2E/x.jpg") == "jpg"


def test_the_same_url_in_two_items_is_downloaded_once(base, tmp_path):
    url = f"{base}/file/shared.pdf"
    items = [[{"file_urls": [url, url]}, {"file_urls": [url]}]]
    shop = spider(base, items, netweir.files(tmp_path))
    stats = shop.run()
    assert Handler.asked["/file/shared.pdf"] == 1
    assert [len(item["files"]) for item in shop.out] == [2, 1]
    assert stats["files_downloaded"] == 1


def test_a_second_run_finds_the_file_up_to_date_without_a_request(base, tmp_path):
    url = f"{base}/file/kept.pdf"
    items = [[{"file_urls": [url]}]]
    spider(base, items, netweir.files(tmp_path)).run()
    again = spider(base, items, netweir.files(tmp_path))
    stats = again.run()
    assert Handler.asked["/file/kept.pdf"] == 1
    entry = again.out[0]["files"][0]
    assert entry["status"] == "uptodate"
    assert entry["checksum"] == md5(b"%PDF-1.4 /file/kept.pdf")
    assert stats["files_uptodate"] == 1 and stats["files_downloaded"] == 0


def test_an_expired_file_is_fetched_again(base, tmp_path):
    url = f"{base}/file/old.pdf"
    items = [[{"file_urls": [url]}]]
    spider(base, items, netweir.files(tmp_path, expires=60)).run()
    stored = tmp_path / f"full/{sha1(url)}.pdf"
    an_hour_ago = time.time() - 3600
    os.utime(stored, (an_hour_ago, an_hour_ago))
    again = spider(base, items, netweir.files(tmp_path, expires=60))
    again.run()
    assert Handler.asked["/file/old.pdf"] == 2
    assert again.out[0]["files"][0]["status"] == "downloaded"


def test_a_failed_download_is_logged_and_the_item_goes_on(base, tmp_path, caplog):
    good, gone = f"{base}/file/good.pdf", f"{base}/file-gone.pdf"
    shop = spider(base, [[{"file_urls": [gone, good, "not a url"]}]], netweir.files(tmp_path))
    with caplog.at_level(logging.WARNING, logger="netweir"):
        stats = shop.run()
    assert [entry["url"] for entry in shop.out[0]["files"]] == [good]
    assert stats["files_failed"] == 2 and stats["items"] == 1
    assert f"{gone}: not downloaded: HTTP 404" in caplog.text
    assert "not a url: not downloaded: not an absolute http(s) URL" in caplog.text


def test_a_redirect_to_another_scheme_isnt_followed(base, tmp_path, caplog):
    shop = spider(base, [[{"file_urls": [f"{base}/to-ftp.pdf"]}]], netweir.files(tmp_path))
    with caplog.at_level(logging.WARNING, logger="netweir"):
        stats = shop.run()
    assert shop.out[0]["files"] == [] and stats["files_failed"] == 1
    assert "not downloaded: redirect to an unsupported URL ftp://" in caplog.text
    assert not (tmp_path / "full").exists()


def test_robots_txt_is_obeyed(base, tmp_path, caplog):
    settings = netweir.Settings(throttle=False, start_delay=0, obey_tdmrep=False)
    url = f"{base}/private/secret.pdf"
    shop = spider(base, [[{"file_urls": [url]}]], netweir.files(tmp_path), settings)
    with caplog.at_level(logging.WARNING, logger="netweir"):
        stats = shop.run()
    assert Handler.asked["/private/secret.pdf"] == 0
    assert shop.out[0]["files"] == [] and stats["files_failed"] == 1
    assert "robots.txt disallows it" in caplog.text


def test_the_hosts_concurrency_applies(base, tmp_path):
    urls = [f"{base}/file/c{i}.pdf" for i in range(4)]
    one = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, per_domain=1
    )
    spider(base, [[{"file_urls": urls}]], netweir.files(tmp_path), one).run()
    assert Handler.peak == 1
    # And with room for more, an item's files are fetched at once.
    Handler.peak = 0
    urls = [f"{base}/file/d{i}.pdf" for i in range(4)]
    spider(base, [[{"file_urls": urls}]], netweir.files(tmp_path / "more")).run()
    assert Handler.peak > 1


def test_downloads_dont_count_toward_max_pages(base, tmp_path):
    settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, max_pages=2
    )
    items = [[{"file_urls": [f"{base}/file/p{n}-{i}.pdf" for i in range(3)]}] for n in range(2)]
    shop = spider(base, items, netweir.files(tmp_path), settings, pages=2)
    # Counted, they would stop the crawl's pages and themselves for good.
    stats = asyncio.run(asyncio.wait_for(shop.crawl(), 30))
    assert len(shop.seen) == 2 and stats["files_downloaded"] == 6
    assert shop.finish_reason == "finished"


def test_downloads_start_while_pages_wait_to_be_read(base, tmp_path):
    # concurrency=1 holds pages back once two events wait to be read; a
    # file the pipeline waits for mustn't wait behind them.
    settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, concurrency=1
    )
    items = [[{"file_urls": [f"{base}/file/w{n}.pdf"]}] for n in range(6)]
    shop = spider(base, items, netweir.files(tmp_path), settings, pages=6, pause=0.2)
    stats = asyncio.run(asyncio.wait_for(shop.crawl(), 30))
    assert stats["files_downloaded"] == 6 and stats["items"] == 6


def test_a_file_is_requested_as_a_page_s_script_would(base, tmp_path):
    url = f"{base}/file/script.pdf"
    spider(base, [[{"file_urls": [url]}]], netweir.files(tmp_path)).run()
    headers = Handler.headers_of["/file/script.pdf"]
    assert headers["Sec-Fetch-Dest"] == "empty"
    assert headers["Referer"] == f"{base}/page0"


def test_a_remote_store_through_fsspec(base):
    fsspec = pytest.importorskip("fsspec")
    url = f"{base}/file/remote.pdf"
    items = [[{"file_urls": [url]}]]
    spider(base, items, netweir.files("memory://netweir-test-files")).run()
    fs = fsspec.filesystem("memory")
    path = f"full/{sha1(url)}.pdf"
    assert fs.cat_file(f"/netweir-test-files/{path}") == b"%PDF-1.4 /file/remote.pdf"
    again = spider(base, items, netweir.files("memory://netweir-test-files"))
    again.run()
    assert again.out[0]["files"][0]["status"] == "uptodate"
    # The memory store records no checksum, and the file isn't read back.
    assert again.out[0]["files"][0]["checksum"] is None
    assert Handler.asked["/file/remote.pdf"] == 1
    fs.rm("/netweir-test-files", recursive=True)


def test_a_store_needs_a_filesystem_fsspec_knows():
    pytest.importorskip("fsspec")
    with pytest.raises((ValueError, ImportError)):
        netweir.files("nosuchscheme://bucket/files")


def picture(width, height, mode="RGBA"):
    from PIL import Image

    out = io.BytesIO()
    Image.new(mode, (width, height), (200, 30, 30, 128)[: len(mode)]).save(out, "PNG")
    return out.getvalue()


def test_images_become_jpegs_with_thumbnails(base, tmp_path):
    from PIL import Image

    Handler.pictures["/img/wide.png"] = picture(100, 80)
    url = f"{base}/img/wide.png"
    stage = netweir.images(tmp_path, thumbs={"small": (50, 50), "big": (200, 200)})
    shop = spider(base, [[{"image_urls": [url]}]], stage)
    stats = shop.run()
    name = sha1(url)
    entry = shop.out[0]["images"][0]
    assert entry["path"] == f"full/{name}.jpg" and entry["status"] == "downloaded"
    full = (tmp_path / entry["path"]).read_bytes()
    assert entry["checksum"] == md5(full)
    with Image.open(io.BytesIO(full)) as image:
        assert (image.format, image.mode, image.size) == ("JPEG", "RGB", (100, 80))
        # Half-transparent red over white.
        r, g, b = image.getpixel((50, 40))
        assert r > 200 and 120 < g < 160
    with Image.open(tmp_path / f"thumbs/small/{name}.jpg") as small:
        assert small.size == (50, 40)
    # A thumbnail never grows an image.
    with Image.open(tmp_path / f"thumbs/big/{name}.jpg") as big:
        assert big.size == (100, 80)
    assert stats["files_downloaded"] == 1


def test_an_image_is_requested_as_an_img_would(base, tmp_path):
    pytest.importorskip("PIL")
    Handler.pictures["/img/sent.png"] = picture(4, 4, "RGB")
    spider(base, [[{"image_urls": [f"{base}/img/sent.png"]}]], netweir.images(tmp_path)).run()
    headers = Handler.headers_of["/img/sent.png"]
    assert headers["Sec-Fetch-Dest"] == "image"
    assert headers["Sec-Fetch-Mode"] == "no-cors"
    assert headers["Accept"].startswith("image/")
    assert headers["Referer"] == f"{base}/page0"


def test_images_below_min_size_are_skipped(base, tmp_path):
    pytest.importorskip("PIL")
    Handler.pictures["/img/tiny.png"] = picture(10, 10)
    Handler.pictures["/img/fine.png"] = picture(40, 40)
    urls = [f"{base}/img/tiny.png", f"{base}/img/fine.png"]
    stage = netweir.images(tmp_path, min_size=(20, 20))
    shop = spider(base, [[{"image_urls": urls}]], stage)
    stats = shop.run()
    assert [entry["url"] for entry in shop.out[0]["images"]] == [urls[1]]
    assert stats["images_too_small"] == 1
    assert not (tmp_path / f"full/{sha1(urls[0])}.jpg").exists()


def test_something_that_isnt_an_image_fails(base, tmp_path):
    pytest.importorskip("PIL")
    shop = spider(base, [[{"image_urls": [f"{base}/file/x.pdf"]}]], netweir.images(tmp_path))
    stats = shop.run()
    assert shop.out[0]["images"] == [] and stats["files_failed"] == 1


def test_thumbnail_names_stay_inside_the_store(tmp_path):
    pytest.importorskip("PIL")
    with pytest.raises(ValueError, match="isn't a name"):
        netweir.images(tmp_path, thumbs={"../up": (10, 10)})
    with pytest.raises(ValueError, match="width, height"):
        netweir.images(tmp_path, min_size=(10,))


def test_a_resumed_crawl_downloads_what_the_stopped_one_didnt_deliver(base, tmp_path):
    checkpoint = netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        concurrency=1,
        checkpoint=str(tmp_path / "state"),
        max_items=1,
    )
    store = tmp_path / "files"
    items = [[{"file_urls": [f"{base}/file/r{n}.pdf"]}] for n in range(2)]
    first = spider(base, items, netweir.files(store), checkpoint, pages=2)
    first.run()
    assert first.finish_reason == "max_items" and len(first.out) == 1
    # The downloads were never saved as requests: only the page whose item
    # wasn't delivered is left, and its file comes with it.
    resumed = dataclasses.replace(checkpoint, max_items=None)
    second = spider(base, items, netweir.files(store), resumed, pages=2)
    stats = second.run()
    assert stats["items"] == 1 and stats["items_already_written"] <= 1
    names = sorted(
        entry["url"].rsplit("/", 1)[1] for item in first.out + second.out for entry in item["files"]
    )
    assert names == ["r0.pdf", "r1.pdf"]
    assert sum(Handler.asked[f"/file/r{n}.pdf"] for n in range(2)) == 2


def test_downloads_are_kept_in_the_cache_and_the_archive(base, tmp_path):
    from warcio.archiveiterator import ArchiveIterator

    settings = dataclasses.replace(
        FAST, cache=str(tmp_path / "cache"), warc=str(tmp_path / "a.warc")
    )
    url = f"{base}/file/archived.pdf"
    items = [[{"file_urls": [url]}]]
    spider(base, items, netweir.files(tmp_path / "one"), settings).run()
    with open(tmp_path / "a.warc", "rb") as f:
        targets = [r.rec_headers.get_header("WARC-Target-URI") for r in ArchiveIterator(f)]
    assert url in targets
    # A store that hasn't got it takes it from the cache, not the site.
    again = spider(base, items, netweir.files(tmp_path / "two"), settings)
    stats = again.run()
    assert again.out[0]["files"][0]["status"] == "cached" and stats["files_cached"] == 1
    assert Handler.asked["/file/archived.pdf"] == 1


def test_a_store_that_fails_fails_the_file_not_the_item(base, tmp_path, monkeypatch, caplog):
    def refuse(self, relative, data):
        raise RuntimeError("the bucket said no")

    monkeypatch.setattr(_Store, "write", refuse)
    url = f"{base}/file/refused.pdf"
    items = [[{"file_urls": [url]}, {"file_urls": [url]}, {"n": 3}]]
    shop = spider(base, items, netweir.files(tmp_path))
    with caplog.at_level(logging.WARNING, logger="netweir"):
        stats = shop.run()
    # Every item of the page is delivered, and the failure isn't an error.
    assert stats["items"] == 3 and stats["callback_errors"] == 0
    assert [item.get("files") for item in shop.out] == [[], [], []]
    assert stats["files_failed"] == 1
    assert "the bucket said no" in caplog.text


def test_an_image_with_too_many_pixels_fails(base, tmp_path):
    pytest.importorskip("PIL")
    Handler.pictures["/img/huge.png"] = picture(300, 300, "RGB")
    stage = netweir.images(tmp_path, max_pixels=200 * 200)
    shop = spider(base, [[{"image_urls": [f"{base}/img/huge.png"]}]], stage)
    stats = shop.run()
    assert shop.out[0]["images"] == [] and stats["files_failed"] == 1


def test_an_up_to_date_remote_file_isnt_read_back(tmp_path):
    fsspec = pytest.importorskip("fsspec")
    store = _Store("memory://netweir-test-checksum")
    store.write("full/a.pdf", b"stored")
    # A stand-in, so the memory filesystem (one, shared) is left as it was.
    store.fs = mock.Mock(wraps=fsspec.filesystem("memory"))
    # fsspec's memory store keeps no checksum: None, not a download.
    assert store.checksum("full/a.pdf") is None
    assert not store.fs.cat_file.called and not store.fs.open.called
    # A bucket that says what it holds is believed: S3's ETag, GCS's md5Hash.
    store.fs.info = lambda path: {"ETag": '"5d41402abc4b2a76b9719d911017c592"'}
    assert store.checksum("full/a.pdf") == "5d41402abc4b2a76b9719d911017c592"
    store.fs.info = lambda path: {"ETag": '"5d41402abc4b2a76b9719d911017c592-3"'}
    assert store.checksum("full/a.pdf") is None
    store.fs.info = lambda path: {"md5Hash": "XUFAKrxLKna5cZ2REBfFkg=="}
    assert store.checksum("full/a.pdf") == "5d41402abc4b2a76b9719d911017c592"
    fsspec.filesystem("memory").rm("/netweir-test-checksum", recursive=True)


def test_a_file_url_store_is_the_directory_it_names(tmp_path):
    # file:///C:/x on Windows, file:///tmp/x elsewhere.
    store = _Store(tmp_path.as_uri())
    store.write("full/a.pdf", b"here")
    assert (tmp_path / "full" / "a.pdf").read_bytes() == b"here"


def test_items_from_rules_send_their_page_as_referer(base, tmp_path):
    class Product(netweir.Item):
        file_urls = netweir.css("a.pdf::attr(href)", all=True)

    out = []

    class Shop(netweir.Spider):
        start_urls = [f"{base}/list"]
        rules = [netweir.Follow("a", extract=Product)]
        settings = FAST
        pipelines = [netweir.files(tmp_path), lambda item: out.append(item) or item]

    Shop().run()
    assert out[0]["files"][0]["status"] == "downloaded"
    assert Handler.headers_of["/file/rule.pdf"]["Referer"] == f"{base}/product"


def test_a_crawl_stopped_mid_download_gets_the_item_on_resume(base, tmp_path):
    settings = dataclasses.replace(FAST, checkpoint=str(tmp_path / "state"))
    store = tmp_path / "files"
    fast, slow = f"{base}/file/before.pdf", f"{base}/slow/big.pdf"
    items = [[{"file_urls": [fast, slow]}]]
    Handler.release.clear()

    async def stop_mid_download():
        shop = spider(base, items, netweir.files(store), settings)
        crawl = asyncio.ensure_future(shop.crawl())
        stored = store / "full" / f"{sha1(fast)}.pdf"
        while not (Handler.asked["/slow/big.pdf"] and stored.exists()):
            await asyncio.sleep(0.02)
        crawl.cancel()
        with pytest.raises(asyncio.CancelledError):
            await crawl
        return shop

    try:
        first = asyncio.run(stop_mid_download())
    finally:
        Handler.release.set()
    # Nothing delivered, and nothing half-written left in the store.
    assert first.out == []
    names = sorted(p.name for p in (store / "full").iterdir())
    assert names == [f"{sha1(fast)}.pdf"]
    # The resumed crawl fetches the page again, and the item with its files.
    second = spider(base, items, netweir.files(store), settings)
    stats = second.run()
    assert stats["items"] == 1
    status = {entry["url"]: entry["status"] for entry in second.out[0]["files"]}
    assert status == {fast: "uptodate", slow: "downloaded"}
    assert Handler.asked["/file/before.pdf"] == 1
