"""Checkpoints: a crawl stopped at any point picks up where it was."""

import json
import os
import random
import shutil
import subprocess
import sys
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest

import netweir

PAGES = 6
PER_PAGE = 5


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    hits: list[str] = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        Handler.hits.append(self.path)
        p = self.path
        if p.startswith("/page/"):
            n = int(p.rsplit("/", 1)[1])
            links = "".join(f'<a class="item" href="/item/{n}-{i}">i</a>' for i in range(PER_PAGE))
            nxt = f'<a class="next" href="/page/{n + 1}">next</a>' if n < PAGES else ""
            body = f"<html><body>{links}{nxt}</body></html>"
        elif p.startswith("/item/"):
            body = f"<html><body><h1>{p.rsplit('/', 1)[1]}</h1></body></html>"
        else:
            body = "<html></html>"
        data = body.encode()
        # Slow enough that a kill lands mid-crawl.
        time.sleep(0.03)
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


def settings(tmp_path, **kw):
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        checkpoint=str(tmp_path / "state"),
        **kw,
    )


class Stop(Exception):
    pass


def test_an_interrupted_crawl_resumes_without_losing_or_repeating(base, tmp_path):
    out = tmp_path / "items.jsonl"
    stop_after = [7]

    class Items(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item", meta={"from": page.url})
            if nxt := page.css("a.next::attr(href)").get():
                yield page.follow(nxt)

        def item(self, page):
            stop_after[0] -= 1
            if stop_after[0] < 0:
                raise Stop  # the crawl dies here, mid-page
            yield {"name": page.css("h1::text").get(), "from": page.meta["from"]}

    spider = Items()
    spider.settings = settings(tmp_path, fail_fast=True)
    with pytest.raises(Stop):
        spider.run(output=str(out))
    first = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert 0 < len(first) < PAGES * PER_PAGE

    stop_after[0] = 10**6
    spider = Items()
    spider.settings = settings(tmp_path)
    stats = spider.run(output=str(out))
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    names = sorted(r["name"] for r in rows)
    assert names == sorted(f"{n}-{i}" for n in range(1, PAGES + 1) for i in range(PER_PAGE))
    assert len({r["_id"] for r in rows}) == len(rows), "no item twice"
    assert all(r["from"].endswith(f"/page/{r['name'].split('-')[0]}") for r in rows)
    assert stats["fetched"] < PAGES + PAGES * PER_PAGE, "finished pages aren't fetched again"


def holds_open(path) -> bool:
    """Whether this process still has a file under ``path`` open."""
    if os.name == "nt":
        # Windows won't rename a directory with an open file in it.
        moved = path.with_name(path.name + "-moved")
        try:
            path.rename(moved)
        except PermissionError:
            return True
        moved.rename(path)
        return False
    if not shutil.which("lsof"):
        pytest.skip("needs lsof")
    out = subprocess.run(
        ["lsof", "-Fn", "-p", str(os.getpid())], capture_output=True, text=True
    ).stdout
    return str(path.resolve()) in out


def test_the_checkpoint_is_closed_when_a_crawl_dies(base, tmp_path):
    class Dies(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            raise Stop

    spider = Dies()
    spider.settings = settings(tmp_path, fail_fast=True)
    # The traceback keeps the crawl's frames alive while it's held.
    with pytest.raises(Stop) as held:
        spider.run()
    assert not holds_open(tmp_path / "state")
    del held


def test_with_a_checkpoint_callbacks_must_be_methods(base, tmp_path):
    class Lambdas(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            yield page.follow("/item/x", callback=lambda p: None)

    spider = Lambdas()
    spider.settings = settings(tmp_path)
    stats = spider.run()
    assert stats["callback_errors"] == 1


def test_meta_must_be_json(base, tmp_path, caplog):
    class Meta(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            yield page.follow("/item/x", meta={"when": object()})

    spider = Meta()
    spider.settings = settings(tmp_path)
    with caplog.at_level("ERROR", logger="netweir"):
        assert spider.run()["callback_errors"] == 1
    assert "JSON" in caplog.text


SPIDER = textwrap.dedent(
    """
    import netweir

    class Items(netweir.Spider):
        start_urls = ["{base}/page/1"]

        def parse(self, page):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item")
            if nxt := page.css("a.next::attr(href)").get():
                yield page.follow(nxt)

        def item(self, page):
            yield {{"name": page.css("h1::text").get()}}
    """
)


@pytest.mark.parametrize(
    ("output", "workers"), [("items.jsonl", 1), ("items.csv", 1), ("items.jsonl", 2)]
)
def test_kill_9_at_random_points_loses_nothing_and_repeats_nothing(base, tmp_path, output, workers):
    script = tmp_path / "spider.py"
    script.write_text(SPIDER.format(base=base), encoding="utf-8")
    out = tmp_path / output
    command = [
        sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-o", str(out),
        "-s", "throttle=false", "-s", "start_delay=0", "-s", "obey_robots=false",
        "-s", "obey_tdmrep=false", "-s", "per_domain=1", "-s", "concurrency=1",
        "-s", f"checkpoint={tmp_path / 'state'}", "-s", f"workers={workers}",
    ]  # fmt: skip
    rng = random.Random(1234)
    kills = 0
    # A dozen runs killed at random points, then one left to finish: a slow
    # machine may spend longer than any of the random waits just starting
    # the workers, and never get to the end otherwise.
    for attempt in range(13):
        proc = subprocess.Popen(command, stderr=subprocess.PIPE, text=True)
        try:
            proc.wait(timeout=rng.uniform(0.15, 0.9) if attempt < 12 else 120)
        except subprocess.TimeoutExpired:
            proc.kill()  # SIGKILL: no cleanup of any kind
            proc.wait()
            kills += 1
            continue
        assert proc.returncode == 0, proc.stderr.read()
        break
    else:
        pytest.fail("the crawl never finished")
    assert kills > 0, "the test never interrupted the crawl"
    if output.endswith(".jsonl"):
        rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    else:
        import csv

        with out.open(encoding="utf-8", newline="") as f:
            rows = list(csv.DictReader(f))
    names = sorted(r["name"] for r in rows)
    expected = sorted(f"{n}-{i}" for n in range(1, PAGES + 1) for i in range(PER_PAGE))
    assert names == expected, f"after {kills} kills"
    assert len({r["_id"] for r in rows}) == len(rows)


class Breaking(netweir.Spider):
    """Items, with ``at`` called before each: the n-th, from 1."""

    start_urls: list[str] = []
    at: Any = None

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback="item")
        if nxt := page.css("a.next::attr(href)").get():
            yield page.follow(nxt)

    def item(self, page):
        type(self).seen += 1
        type(self).at(type(self).seen)
        yield {"name": page.css("h1::text").get()}


def names_in(out):
    return sorted(json.loads(line)["name"] for line in out.read_text(encoding="utf-8").splitlines())


ALL = sorted(f"{n}-{i}" for n in range(1, PAGES + 1) for i in range(PER_PAGE))


def test_failing_commits_are_warned_of_and_so_is_their_end(base, tmp_path, caplog):
    db = tmp_path / "state" / "crawl.sqlite3"

    def sql(statement):
        # From another process: Python's SQLite is a second copy of SQLite
        # in this one, and closing its connection drops the locks the
        # crawl's copy holds on the file (POSIX locks are per process).
        subprocess.run(
            [sys.executable, "-c", "import sqlite3, sys; sqlite3.connect(sys.argv[1]).execute(sys.argv[2])", str(db), statement],
            check=True,
        )  # fmt: skip

    def at(n):
        # The commits after the 4th item's batch fail, as on a full disk,
        # until the 6th lets them through. Each pause lets the writer
        # commit (or fail) before the batch is settled.
        if n == 4:
            sql(
                "CREATE TRIGGER full BEFORE INSERT ON items BEGIN SELECT RAISE(ABORT, 'disk full'); END"
            )
        if n == 6:
            time.sleep(0.3)
            sql("DROP TRIGGER full")
        if n in (5, 7):
            time.sleep(0.3)

    class Items(Breaking):
        start_urls = [f"{base}/page/1"]
        seen = 0

    Items.at = staticmethod(at)
    spider = Items()
    spider.settings = settings(tmp_path)
    out = tmp_path / "items.jsonl"
    with caplog.at_level("WARNING", logger="netweir"):
        stats = spider.run(output=str(out))
    failing = [r.message for r in caplog.records if "each commit tries" in r.message]
    assert len(failing) == 1 and "disk full" in failing[0], caplog.text
    assert caplog.text.count("checkpoint commits work again") == 1, caplog.text
    assert stats["checkpoint_failures"] >= 1
    assert names_in(out) == ALL


def test_a_checkpoint_that_has_stopped_stops_the_crawl(base, tmp_path, monkeypatch):
    real = netweir._crawl._Run.watch_checkpoint
    given_up = "checkpoint write failed: disk full, and the checkpoint has stopped"

    def watch(self):
        if Items.seen < 8:
            return real(self)
        engine = self.engine

        class Stopped:
            def __getattr__(self, name):
                return getattr(engine, name)

            def checkpoint_failing(self):
                return (given_up, True)

        self.engine = Stopped()
        try:
            real(self)
        finally:
            self.engine = engine

    monkeypatch.setattr(netweir._crawl._Run, "watch_checkpoint", watch)

    class Items(Breaking):
        start_urls = [f"{base}/page/1"]
        seen = 0
        at = staticmethod(lambda n: None)

    out = tmp_path / "items.jsonl"
    spider = Items()
    spider.settings = settings(tmp_path)
    with pytest.raises(OSError, match="checkpoint has stopped"):
        spider.run(output=str(out))
    assert 0 < len(names_in(out)) < len(ALL)
    monkeypatch.setattr(netweir._crawl._Run, "watch_checkpoint", real)
    spider = Items()
    spider.settings = settings(tmp_path)
    assert "checkpoint_failures" in spider.run(output=str(out))
    assert names_in(out) == ALL


def test_a_rule_with_extract_and_callback_keeps_both_items(base, tmp_path):
    class Name(netweir.Item):
        name = netweir.css("h1::text")

    class Both(netweir.Spider):
        start_urls = [f"{base}/page/1"]
        rules = [netweir.Follow("a.item", extract=Name, callback="more")]

        def more(self, page):
            yield {"more": page.css("h1::text").get()}

    spider = Both()
    spider.settings = settings(tmp_path)
    items = []
    spider.pipelines = [lambda item: items.append(item) or item]
    stats = spider.run()
    assert stats["items_already_written"] == 0
    assert sum("name" in i for i in items) == PER_PAGE
    assert sum("more" in i for i in items) == PER_PAGE
    assert len({i["_id"] for i in items}) == len(items)


def test_a_crawl_that_cannot_start_leaves_the_output_alone(base, tmp_path):
    import sqlite3

    out = tmp_path / "items.jsonl"
    out.write_text('{"kept": 1}\n', encoding="utf-8")
    state = tmp_path / "state"
    state.mkdir()
    db = sqlite3.connect(state / "crawl.sqlite3")
    db.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
    db.execute("INSERT INTO meta VALUES ('format', '99')")
    db.commit()
    db.close()

    class Any(netweir.Spider):
        start_urls = [f"{base}/page/1"]

    spider = Any()
    spider.settings = settings(tmp_path)
    with pytest.raises(ValueError, match="newer netweir"):
        spider.run(output=str(out))
    assert out.read_text(encoding="utf-8") == '{"kept": 1}\n'


def test_parquet_parts_appear_only_once_recorded(base, tmp_path, monkeypatch):
    pq = pytest.importorskip("pyarrow.parquet")
    out = tmp_path / "items.parquet"
    (tmp_path / "unrelated").mkdir()
    out.write_bytes(b"")  # a stale file from before the checkpoint existed

    class Items(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            yield {"url": page.url}
            if nxt := page.css("a.next::attr(href)").get():
                yield page.follow(nxt)

    # The process dies after the file is closed, before it's recorded.
    real_record = netweir._crawl._Run.record

    def die(self, closed=False):
        raise KeyboardInterrupt

    monkeypatch.setattr(netweir._crawl._Run, "record", die)
    spider = Items()
    spider.settings = settings(tmp_path)
    with pytest.raises(KeyboardInterrupt):
        spider.run(output=str(out))
    monkeypatch.setattr(netweir._crawl._Run, "record", real_record)
    assert not list(tmp_path.glob("items.*.parquet")), "nothing unrecorded is published"

    spider = Items()
    spider.settings = settings(tmp_path)
    spider.run(output=str(out))
    parts = sorted(tmp_path.glob("items*.parquet"))
    rows = sum(pq.read_table(p).num_rows for p in parts if p.stat().st_size)
    assert rows == PAGES, f"{[(p.name, p.stat().st_size) for p in parts]}"
    assert not list(tmp_path.glob("*.partial"))


def test_csv_fields_dont_warn_about_the_checkpoint_id(base, tmp_path, caplog):
    class Rows(netweir.Spider):
        start_urls = [f"{base}/item/x"]

        def parse(self, page):
            yield {"name": page.css("h1::text").get()}

    spider = Rows()
    spider.settings = settings(tmp_path)
    spider.pipelines = [netweir.export.csv(str(tmp_path / "rows.csv"), fields=["name"])]
    with caplog.at_level("WARNING", logger="netweir"):
        spider.run()
    assert "_id" not in caplog.text
