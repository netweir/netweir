"""Watching a crawl while it runs: live stats, the progress line, and the
metrics endpoint."""

import contextlib
import dataclasses
import json
import logging
import re
import socket
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import worker_spiders

import netweir
from netweir import _cli, _files, _stats

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        # /N links to /N+1 up to /LAST; /slow/N takes a moment.
        parts = self.path.strip("/").split("/")
        if parts[0] == "slow":
            time.sleep(0.1)
            parts = parts[1:]
        n = int(parts[0] or 0)
        prefix = "/slow" if self.path.startswith("/slow") else ""
        link = f"<a href='{prefix}/{n + 1}'>next</a>" if n < 9 else ""
        body = f"<h1>{n}</h1>{link}".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def chain(base, path="/0", settings=FAST, **attrs):
    class Chain(netweir.Spider):
        name = "chain"
        start_urls = [base + path]

        def parse(self, page):
            yield {"n": page.css("h1::text").get()}
            yield from page.follow_all(page.css("a::attr(href)"))

    Chain.settings = settings
    for name, value in attrs.items():
        setattr(Chain, name, value)
    return Chain


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def get(url, headers=None):
    request = urllib.request.Request(url, headers=headers or {})
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.headers["Content-Type"], response.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.headers["Content-Type"], e.read().decode()


# --- spider.stats ----------------------------------------------------------------


def test_spider_stats_are_live_while_the_crawl_runs(base):
    seen = []

    def parse(self, page):
        seen.append(self.stats["fetched"])
        yield from page.follow_all(page.css("a::attr(href)"))

    spider = chain(base, parse=parse)()
    stats = spider.run()
    assert seen == sorted(seen) and seen[-1] >= 9 and seen[0] >= 1
    assert isinstance(spider.stats, netweir.Stats)
    assert dict(spider.stats) == stats == {**spider.stats}
    assert spider.stats.get_stats() == stats
    assert spider.stats.get_value("fetched") == 10
    assert spider.stats.get_value("nothing", 7) == 7


def test_a_spider_s_own_stats_go_with_netweir_s(base):
    def parse(self, page):
        self.stats.inc_value("pages_seen")
        self.stats.inc_value("by_ten", 10, start=5)
        self.stats.max_value("highest", int(page.css("h1::text").get()))
        self.stats.min_value("lowest", int(page.css("h1::text").get()))
        self.stats.set_value("last_url", page.url)
        yield from page.follow_all(page.css("a::attr(href)"))

    spider = chain(base, parse=parse)()
    stats = spider.run()
    assert stats["pages_seen"] == 10
    assert stats["by_ten"] == 105
    assert (stats["highest"], stats["lowest"]) == (9, 0)
    assert stats["last_url"].endswith("/9")
    assert spider.stats["pages_seen"] == 10


def test_netweir_s_own_stats_can_t_be_written(base):
    spider = chain(base)()
    spider.run()
    for key in ("items", "fetched", "queued", "files_downloaded"):
        with pytest.raises(ValueError, match="Netweir's own"):
            spider.stats.inc_value(key)


def test_writes_after_the_end_land_in_the_final_stats(base):
    def closed(self, reason):
        self.stats.set_value("closed_with", reason)

    spider = chain(base, closed=closed)()
    spider.run()
    assert spider.stats["closed_with"] == "finished"
    assert spider.stats["checks_failed"] == 0


def test_a_spider_s_own_stats_add_up_across_workers(base):
    spider = worker_spiders.Counts()
    spider.settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, workers=2
    )
    spider.start_urls = [base + "/0"]
    stats = spider.run()
    assert stats["fetched"] == 10
    assert stats["pages_counted"] == 10
    assert stats["deepest"] == 9


# --- the progress line -------------------------------------------------------------


def test_the_crawl_logs_its_progress(base, caplog):
    settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, stats_interval=0.2
    )
    with caplog.at_level(logging.INFO, logger="netweir"):
        chain(base, "/slow/0", settings)().run()
    lines = [r.getMessage() for r in caplog.records if r.getMessage().startswith("crawled ")]
    assert lines, "no progress line"
    pattern = (
        r"crawled \d+ pages \(at \d+ pages/min\), scraped \d+ items \(at \d+ items/min\);"
        r" \d+ errors, \d+ queued, \d+ in flight, [\d.]+ (B|KiB|MiB|GiB) received"
    )
    assert all(re.fullmatch(pattern, line) for line in lines), lines


def test_the_progress_line_can_be_turned_off(base, caplog):
    settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, stats_interval=None
    )
    with caplog.at_level(logging.INFO, logger="netweir"):
        chain(base, "/slow/5", settings)().run()
    assert not [r for r in caplog.records if r.getMessage().startswith("crawled ")]


def test_the_progress_line_s_rates_are_per_minute():
    stats = {"fetched": 120, "items": 30, "failed": 1, "callback_errors": 2, "bytes": 3 << 20}
    line = _stats.progress(stats, {"queued": 5, "in_flight": 2}, {"fetched": 60, "items": 0}, 30)
    assert line == (
        "crawled 120 pages (at 120 pages/min), scraped 30 items (at 60 items/min);"
        " 3 errors, 5 queued, 2 in flight, 3.0 MiB received"
    )


@pytest.mark.parametrize("value", [0, -1])
def test_stats_interval_must_be_positive(value):
    with pytest.raises(ValueError, match="stats_interval"):
        netweir.Settings(stats_interval=value)


# --- the metrics endpoint -----------------------------------------------------------


def watched(base, port, answers, **settings):
    """A crawl that, from its fourth page's callback, reads the endpoint:
    the event loop is busy meanwhile, as a slow callback keeps it."""

    def parse(self, page):
        if page.url.endswith("/3"):
            self.stats.inc_value("custom_count", 2)
            self.stats.set_value("label", "not a number")
            for path in ("/metrics", "/stats.json", "/nowhere"):
                answers[path] = get(f"http://127.0.0.1:{port}{path}")
            answers["origin"] = get(
                f"http://127.0.0.1:{port}/metrics", {"Origin": "http://evil.example"}
            )
            answers["rebound"] = get(
                f"http://127.0.0.1:{port}/metrics", {"Host": f"evil.example:{port}"}
            )
        yield {"n": page.url}
        yield from page.follow_all(page.css("a::attr(href)"))

    s = netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        metrics_port=port,
        **settings,
    )
    return chain(base, settings=s, parse=parse)()


def test_the_endpoint_serves_prometheus_metrics(base):
    port = free_port()
    answers = {}
    spider = watched(base, port, answers)
    spider.run()
    status, kind, text = answers["/metrics"]
    assert status == 200
    assert kind == "text/plain; version=0.0.4; charset=utf-8"
    labels = rf'\{{spider="chain",process="{re.escape(socket.gethostname())}:\d+"\}}'
    assert "# HELP netweir_fetched_total " in text
    assert "# TYPE netweir_fetched_total counter" in text
    assert re.search(rf"^netweir_fetched_total{labels} [34]$", text, re.M), text
    assert "# TYPE netweir_queued gauge" in text
    assert re.search(rf"^netweir_in_flight{labels} \d+$", text, re.M)
    assert re.search(rf"^netweir_start_time_seconds{labels} \d+\.\d+$", text, re.M)
    assert "# TYPE netweir_custom_stat untyped" in text
    assert re.search(r'^netweir_custom_stat\{.*,stat="custom_count"\} 2$', text, re.M)
    assert "not a number" not in text
    # Every family has its HELP and TYPE once, before its samples.
    families = re.findall(r"^# TYPE (\S+) (\S+)$", text, re.M)
    assert len(families) == len({name for name, _ in families})
    for line in text.splitlines():
        if not line.startswith("#"):
            assert re.fullmatch(r"[a-zA-Z_:][a-zA-Z0-9_:]*\{[^}]*\} \S+", line), line
    assert text.endswith("\n")


def test_the_endpoint_serves_json_and_refuses_the_rest(base):
    port = free_port()
    answers = {}
    watched(base, port, answers).run()
    status, kind, text = answers["/stats.json"]
    assert (status, kind) == (200, "application/json")
    answer = json.loads(text)
    assert answer["spider"] == "chain"
    assert answer["process"].startswith(socket.gethostname() + ":")
    assert answer["stats"]["fetched"] >= 3 and answer["stats"]["custom_count"] == 2
    assert answer["elapsed"] >= 0 and answer["queued"] >= 0 and answer["in_flight"] >= 0
    assert answers["/nowhere"][0] == 404
    assert answers["origin"][0] == 403
    assert answers["rebound"][0] == 403


def test_the_endpoint_goes_with_the_crawl(base):
    port = free_port()
    watched(base, port, {}).run()
    with pytest.raises(OSError):
        socket.create_connection(("127.0.0.1", port), timeout=1).close()


def test_a_taken_port_stops_the_crawl_before_it_starts(base, tmp_path):
    with socket.socket() as taken:
        taken.bind(("127.0.0.1", 0))
        taken.listen()
        port = taken.getsockname()[1]
        spider = watched(base, port, {})
        with pytest.raises(OSError, match=f"metrics_port {port}"):
            spider.run(output=str(tmp_path / "out.jsonl"))
    assert spider.stats["fetched"] == 0
    assert not (tmp_path / "out.jsonl").exists()


def test_port_zero_takes_a_free_port_and_says_which(base, caplog):
    with caplog.at_level(logging.INFO, logger="netweir"):
        watched(base, 0, {}).run()
    assert any(
        re.fullmatch(r"metrics at http://127\.0\.0\.1:\d+/metrics", r.getMessage())
        for r in caplog.records
    )


def test_a_public_address_comes_with_a_warning(base, caplog):
    with caplog.at_level(logging.WARNING, logger="netweir"):
        watched(base, 0, {}, metrics_host="0.0.0.0").run()
    assert any("no password" in r.getMessage() for r in caplog.records)


@pytest.mark.parametrize("port", [-1, 65536])
def test_metrics_port_must_be_a_port(port):
    with pytest.raises(ValueError, match="metrics_port"):
        netweir.Settings(metrics_port=port)


def test_every_built_in_stat_has_a_metric(base):
    stats = chain(base)().run()
    keys = set(stats) | {"queued", "in_flight", "pushed", "leased", "leases_reclaimed"}
    keys |= {"queue_bad", "images_too_small", *_files._Media.counters}
    missing = keys - set(_stats.METRICS)
    assert not missing


def test_prometheus_text_escapes_and_formats():
    text = _stats.prometheus(
        {"fetched": 3, "mine": 1.5, "flag": True, "inf": float("inf"), "text": "x"},
        {"queued": 1, "in_flight": 0},
        {"spider": 'a"b\\c\nd', "process": "h:1"},
        1700000000.25,
    )
    assert 'netweir_fetched_total{spider="a\\"b\\\\c\\nd",process="h:1"} 3\n' in text
    assert 'stat="mine"} 1.5\n' in text
    assert 'stat="inf"} +Inf\n' in text
    assert 'stat="flag"' not in text and 'stat="text"' not in text
    assert "netweir_start_time_seconds{" in text and "} 1700000000.25\n" in text


# --- the --stats file -----------------------------------------------------------------


def test_the_stats_file_is_rewritten_while_the_crawl_runs(base, tmp_path, monkeypatch):
    monkeypatch.setattr(_cli, "STATS_FILE_INTERVAL", 0.1)
    spider_file = tmp_path / "chain.py"
    spider_file.write_text(
        "import netweir\n"
        "class Chain(netweir.Spider):\n"
        "    settings = netweir.Settings(throttle=False, start_delay=0, obey_robots=False,"
        " obey_tdmrep=False)\n"
        f"    start_urls = [{base + '/slow/0'!r}]\n"
        "    def parse(self, page):\n"
        "        yield from page.follow_all(page.css('a::attr(href)'))\n",
        encoding="utf-8",
    )
    path = tmp_path / "stats.json"
    snapshots = []
    done = threading.Event()

    def watch():
        while not done.is_set():
            with contextlib.suppress(OSError, ValueError):
                snapshots.append(json.loads(path.read_text(encoding="utf-8")))
            time.sleep(0.02)

    watcher = threading.Thread(target=watch)
    watcher.start()
    try:
        assert _cli.main(["crawl", str(spider_file), "--stats", str(path), "-q"]) == 0
    finally:
        done.set()
        watcher.join()
    live = [s for s in snapshots if s["finish_reason"] is None]
    # Some rewrite caught the crawl part way. The last one before the end
    # may already count all ten: the fetching is over before the crawl is.
    assert any(0 < s["fetched"] < 10 for s in live), live
    final = json.loads(path.read_text(encoding="utf-8"))
    assert final["finish_reason"] == "finished" and final["fetched"] == 10
    assert not list(tmp_path.glob("*.tmp"))


@pytest.mark.skipif(sys.platform == "win32", reason="checked where replace can't fail")
def test_stats_file_writes_replace_the_file_whole(tmp_path):
    path = tmp_path / "s.json"
    _cli._write_stats(str(path), {"items": 1}, None)
    assert json.loads(path.read_text()) == {"items": 1, "finish_reason": None}
    assert [p.name for p in tmp_path.iterdir()] == ["s.json"]


# --- review follow-ups --------------------------------------------------------------


def test_writes_after_the_last_batch_land_in_the_final_stats(base):
    def late(stats, spider):
        spider.stats.inc_value("checked_late")

    spider = chain(base, checks=[late])()
    stats = spider.run()
    assert stats["checked_late"] == 1
    assert spider.stats["checked_late"] == 1


def test_the_final_stats_are_a_dict_to_json_pickle_and_copy(base):
    import copy
    import pickle

    spider = chain(base)()
    stats = spider.run()
    assert isinstance(spider.stats, dict)
    assert json.loads(json.dumps(spider.stats)) == stats
    for again in (pickle.loads(pickle.dumps(spider.stats)), copy.deepcopy(spider.stats)):
        assert type(again) is dict and again == stats
    assert copy.copy(spider.stats) == stats
    with pytest.raises(TypeError, match="read-only"):
        spider.stats["items"] = 0
    with pytest.raises(TypeError, match="read-only"):
        spider.stats.update(items=0)


def test_live_stats_pickle_as_a_snapshot(base):
    import pickle

    seen = []

    def parse(self, page):
        seen.append(pickle.loads(pickle.dumps(self.stats)))
        yield from page.follow_all(page.css("a::attr(href)"))

    chain(base, parse=parse)().run()
    assert type(seen[-1]) is dict and seen[-1]["fetched"] >= 9


def test_scrapy_s_spider_argument_is_taken(base):
    spider = chain(base)()
    spider.run()
    spider.stats.inc_value("mine", spider=spider)
    spider.stats.set_value("other", 2, spider=spider)
    spider.stats.max_value("other", 3, spider=spider)
    spider.stats.min_value("other", 1, spider=spider)
    assert spider.stats.get_value("mine", spider=spider) == 1
    assert spider.stats.get_stats(spider)["other"] == 1


@pytest.mark.parametrize("value", [float("inf"), float("nan")])
def test_stats_interval_must_be_finite(value):
    with pytest.raises(ValueError, match="stats_interval"):
        netweir.Settings(stats_interval=value)


def test_a_monitor_that_won_t_stop_doesn_t_stop_the_wind_up(base, tmp_path, monkeypatch, caplog):
    def broken(self):
        raise RuntimeError("stuck")

    monkeypatch.setattr(_stats.Monitor, "stop", broken)
    spider = chain(base)()
    stats = spider.run(output=str(tmp_path / "out.jsonl"))
    assert len((tmp_path / "out.jsonl").read_text().splitlines()) == 10
    assert dict(spider.stats) == stats
    assert any("stopping the progress line" in r.getMessage() for r in caplog.records)


def test_a_public_endpoint_answers_only_to_addresses_and_known_names(base):
    port = free_port()
    answers = {}

    def parse(self, page):
        if page.url.endswith("/3"):
            for host in ("evil.example", "metrics.example", "Metrics.Example.", "127.0.0.1"):
                answers[host] = get(f"http://127.0.0.1:{port}/metrics", {"Host": f"{host}:{port}"})[
                    0
                ]
        yield from page.follow_all(page.css("a::attr(href)"))

    s = dataclasses.replace(
        FAST,
        metrics_port=port,
        metrics_host="0.0.0.0",
        metrics_allowed_hosts=("metrics.example",),
    )
    chain(base, settings=s, parse=parse)().run()
    assert answers == {
        "evil.example": 403,
        "metrics.example": 200,
        "Metrics.Example.": 200,
        "127.0.0.1": 200,
    }


def test_metrics_allowed_hosts_is_a_tuple():
    with pytest.raises(ValueError, match="metrics_allowed_hosts"):
        netweir.Settings(metrics_allowed_hosts="metrics.example")
