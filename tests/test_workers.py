"""Callbacks in worker processes: Settings(workers=N)."""

import json
import logging
import os
import pickle
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import worker_spiders

import netweir


def test_a_response_survives_pickling():
    from netweir._native import _response

    r = _response("https://a.test/x", 201, "HTTP/2", [("x-a", "1")], b"<p>hi</p>")
    back = pickle.loads(pickle.dumps(r))
    assert (back.url, back.status, back.version, back.headers, back.body) == (
        "https://a.test/x",
        201,
        "HTTP/2",
        [("x-a", "1")],
        b"<p>hi</p>",
    )
    assert netweir.Page(back).css("p::text").get() == "hi"


ITEMS = 12


class Handler(BaseHTTPRequestHandler):
    redesigned = False

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/":
            links = "".join(f'<a class="item" href="/item/{i}">i</a>' for i in range(ITEMS))
            body = f"<html><body>{links}</body></html>"
        elif self.path.startswith("/item/"):
            body = f"<h1>{self.path.rsplit('/', 1)[1]}</h1>"
        elif self.path.startswith("/price"):
            cls = "cost" if Handler.redesigned else "price_color"
            body = f'<div><p class="{cls}">£51.77</p></div>'
        elif self.path == "/feed":
            body = "<rss><item><title>a&nbsp;b</title></item></rss>"
        else:
            body = "<p>x</p>"
        data = body.encode()
        self.send_response(200)
        kind = "application/rss+xml" if self.path == "/feed" else "text/html"
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


#: The module's server, for tests that don't take the fixture.
BASE_URL: list[str] = []


@pytest.fixture(scope="module", autouse=True)
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    BASE_URL[:] = [f"http://127.0.0.1:{server.server_port}"]
    yield BASE_URL[0]
    server.shutdown()
    server.server_close()


def settings(**kw):
    return netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, **kw
    )


def crawl(spider_class, url, **kw):
    items = []
    spider = spider_class()
    spider.start_urls = [url]
    spider.settings = settings(**kw)
    stats = spider.run(output=items.append)
    return items, stats


def test_workers_write_what_one_process_writes(base, tmp_path):
    one, _ = crawl(worker_spiders.Shop, base + "/", checkpoint=str(tmp_path / "one"))
    many, stats = crawl(
        worker_spiders.Shop, base + "/", workers=3, checkpoint=str(tmp_path / "many")
    )

    # Pages finish fetching in a different order each run, workers or not;
    # what must match is every item and its id.
    def strip(items):
        rows = [{k: v for k, v in i.items() if k != "pid"} for i in items]
        return sorted(rows, key=lambda r: r["_id"])

    assert strip(many) == strip(one), "the same items, with the same ids"
    pids = {i["pid"] for i in many if "pid" in i}
    assert len(pids) > 1 and os.getpid() not in pids, pids
    assert len(many) == 2 * ITEMS
    assert stats["callback_errors"] == 0


def test_a_callback_that_breaks_in_a_worker_is_counted_and_logged(base, caplog):
    with caplog.at_level(logging.ERROR, logger="netweir"):
        items, stats = crawl(worker_spiders.Breaks, base + "/", workers=2)
    assert items == [{"before": base + "/"}], "what came before the error is kept"
    assert stats["callback_errors"] == 1
    assert "ValueError: broke on" in caplog.text


def test_a_limit_stops_a_crawl_in_workers_exactly(base, tmp_path):
    script = tmp_path / "limited.py"
    script.write_text(
        "import netweir\n"
        "class Limited(netweir.Spider):\n"
        f"    start_urls = ['{base}/']\n"
        "    def parse(self, page):\n"
        "        for href in page.css('a.item::attr(href)').getall():\n"
        "            yield page.follow(href, callback='item')\n"
        "    def item(self, page):\n"
        "        yield {'name': page.css('h1::text').get()}\n",
        encoding="utf-8",
    )
    out = tmp_path / "items.jsonl"
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-o", str(out),
         "-s", "workers=2", "-s", "max_items=5", "-s", "throttle=false",
         "-s", "start_delay=0", "-s", "obey_robots=false", "-s", "obey_tdmrep=false"],
        capture_output=True, text=True, timeout=120,
    )  # fmt: skip
    assert done.returncode == 0, done.stderr
    assert len(out.read_text(encoding="utf-8").splitlines()) == 5
    # The callbacks it stopped waiting for aren't reported as unhandled.
    assert "never retrieved" not in done.stderr, done.stderr


def test_fail_fast_stops_the_crawl_with_the_workers_exception(base):
    with pytest.raises(ValueError, match="broke on"):
        crawl(worker_spiders.Breaks, base + "/", workers=2, fail_fast=True)


def test_tracked_selectors_relocate_in_workers_and_say_so_once(base, tmp_path, caplog):
    Handler.redesigned = False
    crawl(worker_spiders.Tracks, base + "/price", workers=2, checkpoint=str(tmp_path / "s"))
    Handler.redesigned = True
    try:
        with caplog.at_level(logging.WARNING, logger="netweir"):
            items, _ = crawl(
                worker_spiders.Tracks, base + "/price?2", workers=2, checkpoint=str(tmp_path / "s")
            )
    finally:
        Handler.redesigned = False
    assert items[0]["price"] == "£51.77"
    assert caplog.text.count("found by similarity") == 1


def test_a_spider_defined_in_a_function_is_refused_with_workers(base):
    class Local(netweir.Spider):
        pass

    with pytest.raises(TypeError, match="module level"):
        crawl(Local, base + "/", workers=2)


def test_workers_must_be_at_least_one():
    with pytest.raises(ValueError, match="workers"):
        netweir.Settings(workers=0)


def test_spider_arguments_reach_the_workers(base, tmp_path):
    script = tmp_path / "spider.py"
    script.write_text(
        "import netweir\n\n"
        "class Args(netweir.Spider):\n"
        f"    start_urls = ['{base}/']\n"
        "    def parse(self, page):\n"
        "        for href in page.css('a.item::attr(href)').getall():\n"
        "            yield page.follow(href, callback='item')\n"
        "    def item(self, page):\n"
        "        yield {'colour': self.colour}\n",
        encoding="utf-8",
    )
    out = tmp_path / "items.jsonl"
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-o", str(out),
         "-a", "colour=blue", "-s", "workers=2", "-s", "throttle=false", "-s", "start_delay=0",
         "-s", "obey_robots=false", "-s", "obey_tdmrep=false"],
        capture_output=True, text=True, timeout=120,
    )  # fmt: skip
    assert done.returncode == 0, done.stderr
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert len(rows) == ITEMS and {r["colour"] for r in rows} == {"blue"}


def test_the_command_runs_a_files_spider_in_workers(base, tmp_path):
    script = tmp_path / "spider.py"
    script.write_text(
        "import os\nimport netweir\n\n"
        "class Files(netweir.Spider):\n"
        f"    start_urls = ['{base}/']\n"
        "    def parse(self, page):\n"
        "        for href in page.css('a.item::attr(href)').getall():\n"
        "            yield page.follow(href, callback='item')\n"
        "    def item(self, page):\n"
        "        yield {'name': page.css('h1::text').get(), 'pid': os.getpid()}\n",
        encoding="utf-8",
    )
    out = tmp_path / "items.jsonl"
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-o", str(out),
         "-s", "workers=2", "-s", "throttle=false", "-s", "start_delay=0",
         "-s", "obey_robots=false", "-s", "obey_tdmrep=false"],
        capture_output=True, text=True, timeout=120,
    )  # fmt: skip
    assert done.returncode == 0, done.stderr
    rows = out.read_text(encoding="utf-8").splitlines()
    assert len(rows) == ITEMS


@pytest.mark.skipif(sys.platform == "win32", reason="uses pgrep")
def test_workers_go_when_the_crawl_is_killed(base, tmp_path):
    import shutil
    import signal
    import time

    if not shutil.which("pgrep"):
        pytest.skip("needs pgrep")
    script = tmp_path / "slow.py"
    script.write_text(
        "import time\nimport netweir\n\n"
        "class Slow(netweir.Spider):\n"
        f"    start_urls = ['{base}/']\n"
        "    def parse(self, page):\n"
        "        time.sleep(60)\n"
        "        yield {}\n",
        encoding="utf-8",
    )
    crawl = subprocess.Popen(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-s", "workers=2",
         "-s", "obey_robots=false", "-s", "obey_tdmrep=false"],
    )  # fmt: skip

    def children():
        found = subprocess.run(["pgrep", "-P", str(crawl.pid)], capture_output=True, text=True)
        return found.stdout.split()

    deadline = time.monotonic() + 30
    while not children() and time.monotonic() < deadline:
        time.sleep(0.1)
    workers = children()
    assert workers, "the crawl started workers"
    os.kill(crawl.pid, signal.SIGKILL)
    crawl.wait()
    deadline = time.monotonic() + 10
    alive = workers
    while alive and time.monotonic() < deadline:
        time.sleep(0.2)
        alive = [w for w in workers if _running(int(w))]
    assert not alive, f"workers outlived the crawl: {alive}"


def _running(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    # A zombie still answers; it has exited, which is what matters.
    state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
    return bool(state.stdout.strip()) and not state.stdout.strip().startswith("Z")


def test_an_exception_that_wont_rebuild_doesnt_break_the_pool(base, caplog):
    with caplog.at_level(logging.ERROR, logger="netweir"):
        items, stats = crawl(worker_spiders.Unpicklable, base + "/", workers=2)
    names = sorted(i.get("name") or i.get("kept") for i in items)
    # Item 0 raised; item 1's lambda can't travel, but what came before it
    # does; the rest are whole.
    assert names == sorted(["1"] + [str(i) for i in range(2, ITEMS)]), items
    assert stats["callback_errors"] == 2
    assert "odd one" in caplog.text


def test_abandoning_a_crawl_stops_callbacks_the_workers_are_running():
    import asyncio
    import time

    from netweir import _workers
    from netweir._native import _response

    async def go():
        spider = worker_spiders.SlowAfterFirst()
        pool = _workers.Pool(spider, settings(workers=2), _track_path())
        slow = pool.submit(
            "item",
            _response(BASE_URL[0] + "/item/5", 200, "HTTP/1.1", [], b"<h1>5</h1>"),
            netweir.Request(BASE_URL[0] + "/item/5"),
        )
        await asyncio.sleep(1.5)  # started, and four seconds from done
        started = time.monotonic()
        pool.close(abandon=True)
        assert time.monotonic() - started < 2, "it waited for the callback"
        slow.cancel()

    asyncio.run(go())


def _track_path():
    import tempfile

    return os.path.join(tempfile.mkdtemp(), "tracks.db")


def test_fail_fast_raises_the_workers_own_exception():
    with pytest.raises(ValueError, match="stop here"):
        crawl(
            worker_spiders.SlowAfterFirst,
            BASE_URL[0] + "/",
            workers=2,
            fail_fast=True,
            per_domain=1,
        )


def test_a_worker_that_dies_ends_the_crawl_saying_so(base):
    with pytest.raises(RuntimeError, match="worker process"):
        crawl(worker_spiders.Dies, base + "/", workers=2)


def test_a_spider_workers_cant_build_is_refused_up_front(base):
    spider = worker_spiders.NeedsArguments("x")
    spider.start_urls = [base + "/"]
    spider.settings = settings(workers=2)
    with pytest.raises(TypeError, match="no arguments"):
        spider.run()


@pytest.mark.skipif(sys.platform == "win32", reason="uses pgrep")
def test_workers_go_when_start_fails(base):
    import time

    with pytest.raises(RuntimeError, match="start broke"):
        crawl(worker_spiders.StartFails, base + "/", workers=2)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        found = subprocess.run(
            ["pgrep", "-f", "multiprocessing.spawn"], capture_output=True, text=True
        ).stdout.split()
        mine = [p for p in found if _parent(int(p)) == os.getpid()]
        if not mine:
            break
        time.sleep(0.2)
    assert not mine, mine


def _parent(pid: int) -> int:
    out = subprocess.run(["ps", "-o", "ppid=", "-p", str(pid)], capture_output=True, text=True)
    return int(out.stdout.strip() or 0)


def test_workers_send_page_html_for_a_repair_only_when_asked_once(monkeypatch, tmp_path):
    from netweir import _track, _workers
    from netweir._native import TrackStore, _response

    class Spider:
        def parse(self, page):
            yield {"price": page.css(".price::text", track="price").get()}

    monkeypatch.setattr(_workers, "_spider", Spider())
    token = _track._crawl.set((TrackStore(str(tmp_path / "t.db")), 0.75))
    try:
        request = netweir.Request("http://a.test/")
        before = _response("http://a.test/", 200, "HTTP/1.1", [], b'<p class="price">7</p>')
        after = _response("http://a.test/", 200, "HTTP/1.1", [], b'<p class="cost">7</p>')
        assert _workers.run("parse", before, request)[0] == [{"price": "7"}]
        # The selector breaks; a repair is asked for only if the crawl
        # repairs, and with the page only once.
        monkeypatch.setattr(_workers, "_repairing", False)
        assert _workers.run("parse", after, request)[2] == []
        monkeypatch.setattr(_workers, "_repairing", True)
        monkeypatch.setattr(_workers, "_asked", set())
        first = _workers.run("parse", after, request)[2]
        again = _workers.run("parse", after, request)[2]
        assert len(first) == 1 and "cost" in first[0][4] and again == []
    finally:
        _track._crawl.reset(token)


def test_abandoning_the_pool_cancels_the_jobs_it_handed_out(tmp_path):
    # Their BrokenProcessPool, set after the loop's last turn, would
    # otherwise be reported as an exception nobody retrieved.
    import asyncio

    from netweir import _workers
    from netweir._native import _response

    async def go():
        pool = _workers.Pool(
            worker_spiders.SlowAfterFirst(), settings(workers=1), str(tmp_path / "track")
        )
        url = BASE_URL[0] + "/item/1"
        job = pool.submit(
            "item", _response(url, 200, "HTTP/1.1", [], b"<p>x</p>"), netweir.Request(url)
        )
        pool.close(abandon=True)
        return job

    assert asyncio.run(go()).cancelled()


def test_xml_recover_reaches_the_workers(base):
    items, stats = crawl(worker_spiders.Feed, base + "/feed", workers=2)
    assert items == [] and stats["callback_errors"] == 1
    items, _ = crawl(worker_spiders.Feed, base + "/feed", workers=2, xml_recover=True)
    assert items == [{"title": "a\xa0b"}]
