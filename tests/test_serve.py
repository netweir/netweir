"""netweir serve: jobs over a scrapyd-style HTTP API."""

import base64
import contextlib
import json
import logging
import os
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from netweir import _serve

posix = pytest.mark.skipif(sys.platform == "win32", reason="POSIX signals")


class Site(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        n = int(self.path.rsplit("/", 1)[1]) if self.path[-1].isdigit() else 0
        if self.path.startswith("/page/"):
            more = f'<li class="next"><a href="/page/{n + 1}">next</a></li>' if n < 3 else ""
            books = "".join(f"<article><h3><a>Book {n}-{i}</a></h3></article>" for i in range(3))
            body = f"<html><body>{books}<ul>{more}</ul></body></html>"
        elif self.path.startswith("/slow/"):
            # An endless chain of pages, each a moment late.
            time.sleep(0.2)
            body = f'<h1>{n}</h1><a href="/slow/{n + 1}">next</a>'
        else:
            body = "<h1>missing</h1>"
        data = body.encode()
        self.send_response(200 if body != "<h1>missing</h1>" else 404)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def site():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


SPIDERS = """
import netweir

FAST = netweir.Settings(throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False)


class Books(netweir.Spider):
    settings = FAST

    async def start(self):
        yield netweir.Request(self.site + "/page/1")

    def parse(self, page):
        for title in page.css("article h3 a::text").getall():
            yield {"title": title}
        if more := page.css("li.next a::attr(href)").get():
            yield page.follow(more)


class Slow(netweir.Spider):
    name = "slow"
    settings = FAST

    async def start(self):
        yield netweir.Request(self.site + "/slow/1")

    def parse(self, page):
        yield {"n": page.css("h1::text").get()}
        yield page.follow(page.css("a::attr(href)").get())
"""


@pytest.fixture
def project(tmp_path):
    folder = tmp_path / "shop"
    folder.mkdir()
    (folder / "spiders.py").write_text(SPIDERS, encoding="utf-8")
    # Not a spider file: projects read only their own folder's files.
    (folder / "lib").mkdir()
    (folder / "lib" / "other.py").write_text("raise SystemExit('read')\n", encoding="utf-8")
    return folder


@contextlib.contextmanager
def serving(tmp_path, project, **options):
    service = _serve.Service(tmp_path / "serve", {"shop": project}, **options)
    service.start()
    server = _serve.server(service, "127.0.0.1", 0)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        service.stop()


def call(url, data=None, auth=None, headers=None):
    """(HTTP status, JSON answer or body) for a GET, or a POST of `data`."""
    body = urllib.parse.urlencode(data, doseq=True).encode() if data is not None else None
    request = urllib.request.Request(url, data=body, headers=headers or {})
    if auth:
        token = base64.b64encode(f"{auth[0]}:{auth[1]}".encode()).decode()
        request.add_header("Authorization", f"Basic {token}")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            status, raw, kind = response.status, response.read(), response.headers["Content-Type"]
    except urllib.error.HTTPError as e:
        status, raw, kind = e.code, e.read(), e.headers["Content-Type"]
    return status, json.loads(raw) if kind == "application/json" else raw.decode()


def schedule(base, spider, site, **extra):
    status, answer = call(
        f"{base}/schedule.json", {"project": "shop", "spider": spider, "site": site, **extra}
    )
    assert status == 200 and answer["status"] == "ok", answer
    return answer["jobid"]


def job(base, jobid):
    """The job's listjobs.json entry, and which list it's in."""
    _, jobs = call(f"{base}/listjobs.json")
    for state in ("pending", "running", "finished"):
        for entry in jobs[state]:
            if entry["id"] == jobid:
                return state, entry
    raise AssertionError(f"no job {jobid}")


def wait(condition, timeout=60):
    deadline = time.monotonic() + timeout
    while not (result := condition()):
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)
    return result


def finished(base, jobid):
    return wait(lambda: (found := job(base, jobid))[0] == "finished" and found[1])


def test_a_job_runs_and_its_items_log_and_stats_are_served(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        jobid = schedule(base, "Books", site)
        entry = finished(base, jobid)
        assert entry["outcome"] == "finished"
        assert entry["log_url"] == f"/logs/shop/Books/{jobid}.log"
        assert entry["items_url"] == f"/items/shop/Books/{jobid}.jl"
        status, items = call(base + entry["items_url"])
        assert status == 200
        assert [json.loads(line)["title"] for line in items.splitlines()][:2] == [
            "Book 1-0",
            "Book 1-1",
        ]
        assert len(items.splitlines()) == 9
        assert call(f"{base}/items/shop/Books/{jobid}.jsonl") == (200, items)
        status, log = call(base + entry["log_url"])
        assert status == 200 and "crawl finished" in log
        status, stats = call(f"{base}/jobstats.json?job={jobid}")
        assert status == 200 and stats["status"] == "ok"
        assert stats["state"] == "finished" and stats["outcome"] == "finished"
        assert stats["stats"]["items"] == 9 and stats["stats"]["finish_reason"] == "finished"


def test_answers_have_scrapyd_s_shape(tmp_path, project, site):
    with serving(tmp_path, project, max_jobs=3) as base:
        status, answer = call(f"{base}/daemonstatus.json")
        assert status == 200
        assert set(answer) == {"node_name", "status", "pending", "running", "finished"}
        assert answer["status"] == "ok" and answer["running"] == 0
        assert call(f"{base}/listprojects.json")[1]["projects"] == ["shop"]
        assert call(f"{base}/listspiders.json?project=shop")[1]["spiders"] == ["Books", "slow"]

        jobid = schedule(base, "Books", site, jobid="my-job.1")
        assert jobid == "my-job.1"
        entry = finished(base, jobid)
        assert {"id", "project", "spider", "start_time", "end_time", "log_url", "items_url"} <= set(
            entry
        )
        assert entry["project"] == "shop" and entry["spider"] == "Books"
        time.strptime(entry["start_time"].split(".")[0], "%Y-%m-%d %H:%M:%S")
        _, jobs = call(f"{base}/listjobs.json?project=shop")
        assert set(jobs) == {"node_name", "status", "pending", "running", "finished"}
        assert call(f"{base}/listjobs.json?project=other")[1]["finished"] == []

        status, error = call(f"{base}/listspiders.json")
        assert status == 400
        assert error["status"] == "error" and "project" in error["message"]
        assert "node_name" in error
        assert call(f"{base}/nothing.json")[0] == 404
        assert call(f"{base}/schedule.json")[0] == 405  # a GET
        status, error = call(f"{base}/delproject.json", {"project": "shop"})
        assert status == 400 and "--project" in error["message"]
        assert call(f"{base}/addversion.json", {"project": "shop"})[0] == 404
        status, answer = call(f"{base}/listversions.json?project=shop")
        assert status == 200 and answer["status"] == "ok" and answer["versions"] == []
        assert call(f"{base}/listversions.json?project=attic")[0] == 400


def test_scheduling_mistakes_are_errors_then(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        for data, message in [
            ({"project": "shop", "spider": "Nope"}, "Nope"),
            ({"project": "attic", "spider": "Books"}, "attic"),
            ({"project": "shop"}, "spider"),
            ({"project": "shop", "spider": "Books", "setting": "concurrency=lots"}, "whole number"),
            ({"project": "shop", "spider": "Books", "setting": "colour=red"}, "colour"),
            ({"project": "shop", "spider": "Books", "not-a-name": "1"}, "not-a-name"),
            ({"project": "shop", "spider": "Books", "priority": "high"}, "priority"),
            ({"project": "shop", "spider": "Books", "priority": "nan"}, "finite"),
            ({"project": "shop", "spider": "Books", "priority": "inf"}, "finite"),
            ({"project": "shop", "spider": "Books", "priority": "-Infinity"}, "finite"),
            # What the crawl would refuse as it started.
            ({"project": "shop", "spider": "Books", "setting": "concurrency=0"}, "concurrency"),
            (
                {"project": "shop", "spider": "Books", "setting": ["min_delay=5", "max_delay=1"]},
                "min_delay",
            ),
            (
                {"project": "shop", "spider": "Books", "setting": "proxy_rotation=bogus"},
                "bogus",
            ),
            ({"project": "shop", "spider": "Books", "setting": "browser=bogus"}, "bogus"),
            ({"project": "shop", "spider": "Books", "setting": "profile=netscape"}, "netscape"),
            # Spider arguments that would change what the spider is.
            ({"project": "shop", "spider": "Books", "_job": "1"}, "'_'"),
            ({"project": "shop", "spider": "Books", "pipelines": "x"}, "pipelines"),
            ({"project": "shop", "spider": "Books", "allowed_domains": "x"}, "allowed_domains"),
            ({"project": "shop", "spider": "Books", "parse": "x"}, "parse"),
        ]:
            status, answer = call(f"{base}/schedule.json", data)
            assert status == 400, (data, answer)
            assert answer["status"] == "error" and message in answer["message"], answer
        jobid = schedule(base, "Books", site)
        status, answer = call(
            f"{base}/schedule.json", {"project": "shop", "spider": "Books", "jobid": jobid}
        )
        assert status == 400 and "already" in answer["message"]


def test_settings_reach_the_job(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        jobid = schedule(base, "Books", site, setting=["max_items=4", "concurrency=2"])
        assert finished(base, jobid)["outcome"] == "finished"
        _, items = call(f"{base}/items/shop/Books/{jobid}.jsonl")
        assert len(items.splitlines()) == 4
        _, stats = call(f"{base}/jobstats.json?job={jobid}")
        assert stats["stats"]["finish_reason"] == "max_items"


def test_max_jobs_queues_the_rest(tmp_path, project, site):
    with serving(tmp_path, project, max_jobs=1) as base:
        first = schedule(base, "slow", site)
        second = schedule(base, "slow", site)
        wait(lambda: job(base, first)[0] == "running")
        time.sleep(0.5)
        assert job(base, second)[0] == "pending"
        status = call(f"{base}/daemonstatus.json")[1]
        assert (status["running"], status["pending"]) == (1, 1)
        assert call(f"{base}/cancel.json", {"project": "shop", "job": first})[1]["prevstate"] == (
            "running"
        )
        wait(lambda: job(base, second)[0] == "running")
        assert job(base, first)[0] == "finished"


def test_a_higher_priority_goes_first(tmp_path, project, site):
    with serving(tmp_path, project, max_jobs=1) as base:
        blocker = schedule(base, "slow", site)
        wait(lambda: job(base, blocker)[0] == "running")
        low = schedule(base, "slow", site)
        high = schedule(base, "slow", site, priority="5")
        assert [e["id"] for e in call(f"{base}/listjobs.json")[1]["pending"]] == [high, low]
        call(f"{base}/cancel.json", {"project": "shop", "job": blocker})
        wait(lambda: job(base, high)[0] == "running")
        assert job(base, low)[0] == "pending"


@posix
def test_a_cancelled_job_keeps_its_items(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        jobid = schedule(base, "slow", site)
        wait(lambda: job(base, jobid)[0] == "running")
        time.sleep(2)  # pages come every 0.2 s
        status, answer = call(f"{base}/cancel.json", {"project": "shop", "job": jobid})
        assert status == 200 and answer["prevstate"] == "running"
        entry = finished(base, jobid)
        assert entry["outcome"] == "cancelled"
        _, items = call(f"{base}{entry['items_url']}")
        rows = [json.loads(line) for line in items.splitlines()]
        assert len(rows) >= 3
        _, stats = call(f"{base}/jobstats.json?job={jobid}")
        assert stats["stats"]["finish_reason"] == "interrupted"
        assert stats["stats"]["items"] == len(rows)
        # Already finished: nothing to cancel.
        assert (
            call(f"{base}/cancel.json", {"project": "shop", "job": jobid})[1]["prevstate"] is None
        )


def test_a_pending_job_can_be_cancelled(tmp_path, project, site):
    with serving(tmp_path, project, max_jobs=1) as base:
        first = schedule(base, "slow", site)
        second = schedule(base, "slow", site)
        answer = call(f"{base}/cancel.json", {"project": "shop", "job": second})[1]
        assert answer == {"node_name": answer["node_name"], "status": "ok", "prevstate": "pending"}
        state, entry = job(base, second)
        assert state == "finished" and entry["outcome"] == "cancelled"
        call(f"{base}/cancel.json", {"project": "shop", "job": first})


def test_only_the_newest_finished_jobs_are_kept(tmp_path, project, site):
    with serving(tmp_path, project, keep=1) as base:
        first = schedule(base, "Books", site)
        finished(base, first)
        log = tmp_path / "serve" / "logs" / "shop" / "Books" / f"{first}.log"
        assert log.exists()
        second = schedule(base, "Books", site)
        finished(base, second)
        _, jobs = call(f"{base}/listjobs.json")
        assert [e["id"] for e in jobs["finished"]] == [second]
        assert not log.exists()
        assert call(f"{base}/items/shop/Books/{first}.jsonl")[0] == 404


def test_jobs_outlive_a_restart(tmp_path, project, site):
    with serving(tmp_path, project, max_jobs=1) as base:
        done = schedule(base, "Books", site)
        finished(base, done)
        running = schedule(base, "slow", site)
        waiting = schedule(base, "Books", site)
        wait(lambda: job(base, running)[0] == "running")
    # Stopping the service cancelled the running job and kept the pending.
    with serving(tmp_path, project, max_jobs=1) as base:
        assert job(base, done)[1]["outcome"] == "finished"
        assert job(base, running)[1]["outcome"] == "cancelled"
        assert finished(base, waiting)["outcome"] == "finished"
        _, items = call(f"{base}/items/shop/Books/{done}.jsonl")
        assert len(items.splitlines()) == 9


def serve_command(directory, project, *extra):
    """The service as `netweir serve` runs it, and its address."""
    proc = subprocess.Popen(
        [sys.executable, "-m", "netweir", "serve", "--port", "0", "--dir", str(directory),
         "--project", f"shop={project}", *extra],
        stderr=subprocess.PIPE, text=True,
    )  # fmt: skip
    for line in proc.stderr:
        if "serving on" in line:
            return proc, line.split("serving on ")[1].strip()
    raise AssertionError(f"the service didn't start: {proc.wait()}")


@posix
def test_a_job_running_when_the_service_died_is_lost(tmp_path, project, site):
    proc, base = serve_command(tmp_path / "serve", project)
    try:
        jobid = schedule(base, "slow", site)
        pid = wait(lambda: job(base, jobid)[1].get("pid"))
    finally:
        proc.kill()
        proc.wait()
    # Known limit: the job carries on unwatched; the test stops it.
    os.kill(pid, signal.SIGKILL)
    proc, base = serve_command(tmp_path / "serve", project)
    try:
        state, entry = job(base, jobid)
        assert state == "finished" and entry["outcome"] == "lost"
        assert entry["end_time"]
    finally:
        proc.send_signal(signal.SIGINT)
        assert proc.wait(timeout=30) == 0


def test_an_open_bind_without_auth_is_refused(tmp_path, project):
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "serve", "--bind", "0.0.0.0", "--port", "0",
         "--dir", str(tmp_path / "serve"), "--project", f"shop={project}"],
        capture_output=True, text=True, timeout=60,
    )  # fmt: skip
    assert done.returncode == 2
    assert "--insecure-no-auth" in done.stderr and "Traceback" not in done.stderr
    assert not (tmp_path / "serve" / "jobs.db").exists()


def test_insecure_no_auth_starts_with_a_warning(tmp_path, project):
    proc = subprocess.Popen(
        [sys.executable, "-m", "netweir", "serve", "--bind", "0.0.0.0", "--port", "0",
         "--dir", str(tmp_path / "serve"), "--project", f"shop={project}",
         "--insecure-no-auth"],
        stderr=subprocess.PIPE, text=True,
    )  # fmt: skip
    try:
        lines = []
        for line in proc.stderr:
            lines.append(line)
            if "serving on" in line:
                break
        assert any("WARNING" in line and "any web page" in line for line in lines), lines
        assert "serving on" in lines[-1]
    finally:
        proc.kill()
        proc.wait()


@pytest.mark.parametrize(
    "args, message",
    [
        (["--project", "shop"], "no such file or directory"),
        (["--project", "../x=."], "../x"),
        (["--username", "me"], "--password"),
        (["--max-jobs", "0"], "--max-jobs"),
        (["--project", "shop=.", "--project", "SHOP=."], "twice"),
    ],
)
def test_command_line_mistakes(tmp_path, args, message):
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "serve", "--dir", str(tmp_path / "serve"), *args],
        capture_output=True, text=True, timeout=60, cwd=tmp_path,
    )  # fmt: skip
    assert done.returncode == 2, done.stderr
    assert message in done.stderr and "Traceback" not in done.stderr


def test_auth_is_required_everywhere(tmp_path, project, site):
    me = ("me", "s3cret")
    with serving(tmp_path, project, credentials=me) as base:
        for path in ["/daemonstatus.json", "/listjobs.json", "/logs/shop/Books/x.log", "/"]:
            assert call(base + path)[0] == 401, path
            assert call(base + path, auth=("me", "wrong"))[0] == 401, path
        status, _ = call(f"{base}/schedule.json", {"project": "shop", "spider": "Books"})
        assert status == 401
        assert call(f"{base}/daemonstatus.json", auth=me)[0] == 200
        status, answer = call(
            f"{base}/schedule.json",
            {"project": "shop", "spider": "Books", "site": site},
            auth=me,
        )
        assert status == 200 and answer["status"] == "ok"


@pytest.mark.parametrize(
    "path",
    [
        "/logs/../jobs.db",
        "/logs/shop/Books/../../../jobs.db",
        "/logs/shop/%2e%2e/x.log",
        "/logs/shop/Books/..%2Fx.log",
        "/logs/shop/Books/%2e%2e.log",
        "/items/shop/Books/.x.jsonl",
        "/items/shop/Books/.x.jl",
        "/logs/shop/Books/x.jl",
        "/logs/../../secret.jl",
        "/items/shop/Books/x.log",
        "/logs/../../secret.log",
        "/logs/%2e%2e/%2e%2e/secret.log",
        "/logs/..%2F../x/secret.log",
        "//etc/passwd",
    ],
)
def test_paths_outside_the_job_files_are_refused(tmp_path, project, path):
    # Where ../../secret.log from the logs folder would be.
    (tmp_path / "secret.log").write_text("the contents", encoding="utf-8")
    with serving(tmp_path, project) as base:
        # urllib sends the path as written, dots and escapes included.
        status, body = call(base + path)
        assert status == 404, path
        assert "the contents" not in str(body)


def test_names_that_aren_t_names_are_refused(tmp_path, project):
    with serving(tmp_path, project) as base:
        for data in [
            {"project": "..", "spider": "Books"},
            {"project": "shop", "spider": "../Books"},
            {"project": "shop", "spider": "Books", "jobid": "../../x"},
            {"project": "shop", "spider": "Books", "jobid": "a/b"},
            {"project": "shop", "spider": "Books", "jobid": ".hidden"},
        ]:
            status, answer = call(f"{base}/schedule.json", data)
            assert status == 400 and answer["status"] == "error", data
        assert call(f"{base}/listspiders.json?project=..")[0] == 400
        assert call(f"{base}/jobstats.json?job=../x")[0] == 400
        assert call(f"{base}/cancel.json", {"project": "shop", "job": "/x"})[0] == 400


def test_requests_from_web_pages_are_refused(tmp_path, project):
    with serving(tmp_path, project) as base:
        status, answer = call(
            f"{base}/schedule.json",
            {"project": "shop", "spider": "Books"},
            headers={"Origin": "https://evil.example"},
        )
        assert status == 403 and answer["status"] == "error"
        # A name that resolves to 127.0.0.1 now, but is someone's site.
        assert call(f"{base}/daemonstatus.json", headers={"Host": "evil.example"})[0] == 403
        assert call(f"{base}/daemonstatus.json", headers={"Host": "localhost:1"})[0] == 200
        for host in ["LocalHost:1", "localhost.:1", "localhost.", "[::1]:1", "127.0.0.1."]:
            assert call(f"{base}/daemonstatus.json", headers={"Host": host})[0] == 200, host
        assert call(f"{base}/daemonstatus.json", headers={"Host": "localhost.evil:1"})[0] == 403


def test_a_file_that_won_t_import_is_an_error_not_a_crash(tmp_path, project, site):
    (project / "broken.py").write_text("raise RuntimeError('broken on import')\n")
    with serving(tmp_path, project) as base:
        status, answer = call(f"{base}/listspiders.json?project=shop")
        assert status == 400 and "broken on import" in answer["message"]
        assert call(f"{base}/daemonstatus.json")[0] == 200


def test_a_spider_name_in_two_files_is_refused(tmp_path, project, site):
    (project / "again.py").write_text(SPIDERS, encoding="utf-8")
    with serving(tmp_path, project) as base:
        status, answer = call(
            f"{base}/schedule.json", {"project": "shop", "spider": "Books", "site": site}
        )
        assert status == 400
        assert "again.py" in answer["message"] and "spiders.py" in answer["message"]


def test_a_failing_job_is_failed(tmp_path, project, site):
    (project / "spiders.py").write_text(
        SPIDERS
        + "\nclass Fails(Books):\n    def opened(self):\n        raise RuntimeError('no')\n",
        encoding="utf-8",
    )
    with serving(tmp_path, project) as base:
        jobid = schedule(base, "Fails", site)
        assert finished(base, jobid)["outcome"] == "failed"
        _, log = call(f"{base}/logs/shop/Fails/{jobid}.log")
        assert "RuntimeError: no" in log


@pytest.mark.parametrize("key", ["checkpoint", "cache", "deltafetch", "warc", "har"])
@pytest.mark.parametrize(
    "value",
    [
        "/tmp/anywhere",
        "../escape",
        "a/../../escape",
        "..",
        "s3://bucket/state",
        "file:///tmp/x",
        "C:\\state",
        "a\\..\\..\\escape",
        "link/inside",  # link points outside the data directory
        "",
    ],
)
def test_path_settings_stay_in_the_project_s_data(tmp_path, project, key, value):
    data = tmp_path / "serve" / "data" / "shop"
    data.mkdir(parents=True)
    (tmp_path / "elsewhere").mkdir()
    if sys.platform != "win32":  # symlinks there need privileges
        (data / "link").symlink_to(tmp_path / "elsewhere")
    elif value.startswith("link"):
        pytest.skip("symlinks")
    with serving(tmp_path, project) as base:
        status, answer = call(
            f"{base}/schedule.json",
            {"project": "shop", "spider": "Books", "setting": f"{key}={value}"},
        )
        assert status == 400 and answer["status"] == "error", answer
        assert key in answer["message"]
        # Said plainly, not left for the symlink check to catch.
        if not value.startswith("link"):
            assert "relative path" in answer["message"], answer
    assert list((tmp_path / "elsewhere").iterdir()) == []


def test_a_checkpoint_goes_in_the_project_s_data(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        jobid = schedule(base, "Books", site, setting=["checkpoint=state/books", "cache=none"])
        assert finished(base, jobid)["outcome"] == "finished"
        assert (tmp_path / "serve" / "data" / "shop" / "state" / "books").is_dir()
        assert not (project / "state").exists()


def test_deltafetch_and_archives_go_in_the_project_s_data(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        jobid = schedule(
            base,
            "Books",
            site,
            setting=["deltafetch=seen", "warc=kept/run.warc.gz", "har=kept/run.har"],
        )
        assert finished(base, jobid)["outcome"] == "finished"
    data = tmp_path / "serve" / "data" / "shop"
    assert (data / "seen" / "deltafetch.sqlite3").is_file()
    assert (data / "kept" / "run.warc.gz").stat().st_size > 0
    assert (data / "kept" / "run.har").stat().st_size > 0
    # Checking the settings when the job was scheduled made nothing.
    assert not (project / "seen").exists() and not (project / "kept").exists()


def test_checking_a_spider_s_own_path_settings_makes_nothing(tmp_path, project):
    own = "class Books(netweir.Spider):\n    settings = dataclasses.replace(FAST, deltafetch='own')"
    (project / "spiders.py").write_text(
        SPIDERS.replace("class Books(netweir.Spider):\n    settings = FAST", own).replace(
            "import netweir\n", "import dataclasses\nimport netweir\n", 1
        ),
        encoding="utf-8",
    )
    # No job runs: only scheduling's check of the settings.
    with serving(tmp_path, project, max_jobs=0) as base:
        status, answer = call(
            f"{base}/schedule.json", {"project": "shop", "spider": "Books", "setting": "har=x.har"}
        )
        assert status == 200, answer
    assert not (project / "own").exists()


def test_scheduling_a_distributed_job_doesn_t_reach_redis(tmp_path, project):
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        closed = s.getsockname()[1]  # nothing listens here once it's closed
    with serving(tmp_path, project, max_jobs=0) as base:
        status, answer = call(
            f"{base}/schedule.json",
            {
                "project": "shop",
                "spider": "Books",
                "setting": f"queue=redis://127.0.0.1:{closed}/0",
            },
        )
        assert status == 200, answer
        # Still checked: a queue with a checkpoint can't work.
        status, answer = call(
            f"{base}/schedule.json",
            {
                "project": "shop",
                "spider": "Books",
                "setting": ["queue=redis://127.0.0.1:6379/0", "checkpoint=state"],
            },
        )
        assert status == 400 and "checkpoint too" in answer["message"], answer


def test_a_path_setting_s_own_mistakes_are_caught_when_scheduled(tmp_path, project):
    with serving(tmp_path, project) as base:
        status, answer = call(
            f"{base}/schedule.json",
            {"project": "shop", "spider": "Books", "setting": "warc=kept/run.txt"},
        )
        assert status == 400 and ".warc.gz" in answer["message"], answer
        status, answer = call(
            f"{base}/schedule.json",
            {"project": "shop", "spider": "Books", "setting": ["warc=a.warc", "har=A.WARC"]},
        )
        assert status == 400 and "file of its own" in answer["message"], answer
    assert not (tmp_path / "serve" / "data" / "shop").exists()


def raw(base, request: bytes) -> bytes:
    """What the service answers to bytes sent as they are."""
    host, port = base.removeprefix("http://").split(":")
    with socket.create_connection((host, int(port)), timeout=30) as conn:
        conn.sendall(request)
        conn.shutdown(socket.SHUT_WR)
        answer = b""
        while chunk := conn.recv(65536):
            answer += chunk
    return answer


@pytest.mark.parametrize("length", ["-1", "-2", "lots"])
def test_a_bad_content_length_is_refused(tmp_path, project, length):
    with serving(tmp_path, project) as base:
        answer = raw(
            base,
            b"POST /schedule.json HTTP/1.1\r\nHost: 127.0.0.1\r\n"
            b"Content-Length: " + length.encode() + b"\r\n\r\nproject=shop&spider=Books",
        )
        assert answer.startswith(b"HTTP/1.0 400"), answer
        assert call(f"{base}/daemonstatus.json")[1]["pending"] == 0


def test_an_idle_connection_is_dropped(tmp_path, project, monkeypatch):
    monkeypatch.setattr(_serve._Handler, "timeout", 0.5)
    with serving(tmp_path, project) as base:
        host, port = base.removeprefix("http://").split(":")
        with socket.create_connection((host, int(port)), timeout=10) as conn:
            started = time.monotonic()
            assert conn.recv(1) == b""  # closed, having been sent nothing
            assert time.monotonic() - started < 5


def test_a_second_service_on_the_same_dir_is_refused(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        jobid = schedule(base, "slow", site)
        wait(lambda: job(base, jobid)[0] == "running")
        with pytest.raises(ValueError, match="in use"):
            _serve.Service(tmp_path / "serve", {"shop": project})
        # Its job wasn't marked lost behind its back.
        assert job(base, jobid)[0] == "running"
        call(f"{base}/cancel.json", {"project": "shop", "job": jobid})


def test_names_differing_only_in_case_are_one_name(tmp_path, project, site):
    with pytest.raises(ValueError, match="case"):
        _serve.Service(tmp_path / "serve", {"shop": project, "Shop": project})
    with serving(tmp_path, project, max_jobs=1) as base:
        blocker = schedule(base, "slow", site, jobid="Job1")
        status, answer = call(
            f"{base}/schedule.json", {"project": "shop", "spider": "Books", "jobid": "job1"}
        )
        assert status == 400 and "already" in answer["message"]
        call(f"{base}/cancel.json", {"project": "shop", "job": blocker})


@posix
def test_killing_a_job_kills_what_it_started(tmp_path, project, site):
    (project / "stubborn.py").write_text(
        "import signal, subprocess, sys\n"
        "from spiders import Slow\n"
        "class Stubborn(Slow):\n"
        "    name = 'stubborn'\n"
        "    def opened(self):\n"
        "        signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
        "        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(300)'])\n"
        "        open(self.pidfile, 'w').write(str(child.pid))\n",
        encoding="utf-8",
    )
    pidfile = tmp_path / "child.pid"
    with serving(tmp_path, project) as base:
        jobid = schedule(base, "stubborn", site, pidfile=str(pidfile))
        wait(lambda: pidfile.exists() and pidfile.read_text())
        child = int(pidfile.read_text())
        assert call(f"{base}/cancel.json", {"project": "shop", "job": jobid})[1]["prevstate"]
        time.sleep(0.5)
        assert job(base, jobid)[0] == "running", "SIGTERM is ignored"
        call(f"{base}/cancel.json", {"project": "shop", "job": jobid})  # again: killed
        assert finished(base, jobid)["outcome"] == "cancelled"

        def gone():
            try:
                os.kill(child, 0)
            except ProcessLookupError:
                return True
            return False

        wait(gone, timeout=10)


def test_a_job_that_ended_isn_t_recorded_as_cancelled(tmp_path, project):
    service = _serve.Service(tmp_path / "serve", {"shop": project})
    try:
        process = subprocess.Popen([sys.executable, "-c", "pass"])
        process.wait()
        with open(tmp_path / "x.log", "wb") as log:
            service.running["x"] = (process, log)
            service._signal("x")
        assert "x" not in service.cancelling
    finally:
        service.close()


class Flaky:
    """A connection whose first `UPDATE ... state = 'running'` fails."""

    def __init__(self, db):
        self.db, self.failed = db, False

    def execute(self, sql, *args):
        if "SET state = 'running'" in sql and not self.failed:
            self.failed = True
            raise sqlite3.OperationalError("disk I/O error")
        return self.db.execute(sql, *args)

    def __getattr__(self, name):
        return getattr(self.db, name)


def test_a_job_whose_start_wasn_t_recorded_runs_once(tmp_path, project, site, caplog):
    caplog.set_level(logging.INFO, logger="netweir.serve")
    service = _serve.Service(tmp_path / "serve", {"shop": project})
    service.db = Flaky(service.db)
    service.start()
    server = _serve.server(service, "127.0.0.1", 0)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        jobid = schedule(base, "slow", site)
        wait(lambda: service.db.failed and jobid in service.running)
        time.sleep(1.5)  # time enough for the scheduler to go round again
        started = [r for r in caplog.records if "started, pid" in r.getMessage()]
        assert len(started) == 1, started
        answer = call(f"{base}/cancel.json", {"project": "shop", "job": jobid})[1]
        assert answer["prevstate"] == "running"
        assert finished(base, jobid)["outcome"] == "cancelled"
    finally:
        server.shutdown()
        server.server_close()
        service.stop()


def test_a_job_id_in_another_case_is_the_same_job(tmp_path, project, site):
    with serving(tmp_path, project) as base:
        schedule(base, "slow", site, jobid="Case")
        wait(lambda: job(base, "Case")[0] == "running")
        status, answer = call(f"{base}/cancel.json", {"project": "shop", "job": "CASE"})
        assert status == 200 and answer["prevstate"] == "running", answer
        assert finished(base, "Case")["outcome"] == "cancelled"
        assert call(f"{base}/jobstats.json?job=cASE")[1]["id"] == "Case"


def test_a_slow_request_is_cut_off(tmp_path, project, monkeypatch):
    monkeypatch.setattr(_serve, "REQUEST_DEADLINE", 1.0)
    with serving(tmp_path, project) as base:
        host, port = base.removeprefix("http://").split(":")
        with socket.create_connection((host, int(port)), timeout=10) as conn:
            started = time.monotonic()
            conn.sendall(b"GET /daemonstatus.json HTTP/1.1\r\n")

            def trickle():
                # A header byte now and then: each read on its own is quick.
                with contextlib.suppress(OSError):
                    while time.monotonic() - started < 8:
                        conn.sendall(b"X")
                        time.sleep(0.2)

            threading.Thread(target=trickle, daemon=True).start()
            # Closed with the trickle unread, the socket may be reset
            # rather than ended (aborted, as Windows names it): any way,
            # cut off.
            with contextlib.suppress(ConnectionResetError, ConnectionAbortedError):
                assert conn.recv(1) == b""
            assert time.monotonic() - started < 5
        # Others are answered as before.
        assert call(f"{base}/daemonstatus.json")[0] == 200


def test_connections_are_capped_per_client_and_in_all():
    slots = _serve._Slots(per_client=2, total=3)
    assert slots.take("a") and slots.take("a")
    assert not slots.take("a"), "a third from one address"
    assert slots.take("b")
    assert not slots.take("c"), "a fourth in all"
    slots.give_back("a")
    assert slots.take("c")
    assert not slots.take("a")
    slots.give_back("b")
    assert slots.take("a")


TUNED = """
import dataclasses
from spiders import Books


class Tuned(Books):
    settings = dataclasses.replace(
        Books.settings, proxies=("http://127.0.0.1:9",), max_delay=2.0
    )
"""


def test_settings_are_checked_over_the_spider_s_own(tmp_path, project, site):
    (project / "tuned.py").write_text(TUNED, encoding="utf-8")
    with serving(tmp_path, project) as base:
        for spider, setting, ok in [
            # Fine for this spider, wrong for the defaults, and back.
            ("Tuned", "proxy_rotation=per_request", True),
            ("Books", "proxy_rotation=per_request", False),
            ("Tuned", "min_delay=5", False),
            ("Books", "min_delay=5", True),
        ]:
            status, answer = call(
                f"{base}/schedule.json",
                {"project": "shop", "spider": spider, "setting": setting, "site": site},
            )
            assert (status == 200) is ok, (spider, setting, answer)
            if ok:
                call(f"{base}/cancel.json", {"project": "shop", "job": answer["jobid"]})
            else:
                assert answer["message"].startswith("setting:"), answer
