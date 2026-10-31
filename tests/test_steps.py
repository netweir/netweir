"""Steps: what a request fetched in Chrome does on its page before the
callback sees it. The first tests need no Chrome; the crawls are skipped
without one, unless NETWEIR_REQUIRE_CHROME is set."""

import asyncio
import os
import pickle
import time
from http.server import BaseHTTPRequestHandler
from urllib.parse import parse_qs, urlsplit

import pytest
import step_spiders
from local_server import serve

import netweir
from netweir import steps


def test_the_builders_give_plain_json_dicts():
    assert steps.click("#more") == {"action": "click", "selector": "#more"}
    assert steps.click("#ok", timeout=3, on_error="continue", frame={"name": "consent"}) == {
        "action": "click",
        "selector": "#ok",
        "frame": {"name": "consent"},
        "timeout": 3,
        "on_error": "continue",
    }
    assert steps.type("#q", "tea", delay=(0.01, 0.02)) == {
        "action": "type",
        "selector": "#q",
        "text": "tea",
        "delay": [0.01, 0.02],
    }
    assert steps.select_option("#size", label=("S", "M")) == {
        "action": "select_option",
        "selector": "#size",
        "label": ["S", "M"],
    }
    assert steps.scroll(times=3) == {"action": "scroll", "times": 3}
    assert steps.screenshot(full_page=True) == {"action": "screenshot", "full_page": True}
    assert steps.screenshot() == {"action": "screenshot"}
    assert steps.pdf() == {"action": "pdf"}
    assert steps.pdf(landscape=True) == {"action": "pdf", "landscape": True}
    assert steps.wait(1.5) == {"action": "wait", "seconds": 1.5}
    assert steps.set_input_files("#cv", ["a.pdf"]) == {
        "action": "set_input_files",
        "selector": "#cv",
        "files": ["a.pdf"],
    }
    with pytest.raises(TypeError, match="call"):
        steps.call(3)


def test_a_request_with_steps_is_fetched_in_chrome_and_checked_where_it_is_made():
    r = netweir.Request("https://a.test/", steps=[steps.click("#more")])
    assert r.browser
    assert netweir.Request("https://a.test/", steps=[]).browser is False
    assert netweir.Request("https://a.test/").steps is None

    def mine(page, request):
        return None

    r = netweir.Request("https://a.test/", steps=[mine])
    assert r.steps == [{"action": "call", "method": mine}]
    with pytest.raises(ValueError, match=r'step 1: click has no field "selecter"'):
        netweir.Request("https://a.test/", steps=[{"action": "click", "selecter": "#a"}])
    with pytest.raises(ValueError, match="unknown action"):
        netweir.Request("https://a.test/", steps=[{"action": "tap", "selector": "#a"}])
    with pytest.raises(ValueError, match="on_error"):
        netweir.Request("https://a.test/", steps=[steps.click("#a", on_error="ignore")])
    with pytest.raises(TypeError, match="list of steps"):
        netweir.Request("https://a.test/", steps=steps.click("#a"))
    with pytest.raises(TypeError, match="a dict or a function"):
        netweir.Request("https://a.test/", steps=["click"])
    with pytest.raises(TypeError, match="JSON"):
        netweir.Request("https://a.test/", steps=[{"action": "evaluate", "js": object()}])
    with pytest.raises(ValueError, match="only a GET"):
        netweir.Request("https://a.test/", method="POST", body=b"x", steps=[steps.click("#a")])


def test_crawl_wide_steps_and_rules_are_checked_too():
    s = netweir.Settings(browser_steps=[steps.click("#a"), steps.call("log_in")])
    assert s.browser_steps == (steps.click("#a"), steps.call("log_in"))
    with pytest.raises(TypeError, match="as a string"):
        netweir.Settings(browser_steps=[lambda page, request: None])
    with pytest.raises(ValueError, match="browser_steps: step 1"):
        netweir.Settings(browser_steps=[{"action": "click"}])
    rule = netweir.Follow("a", steps=[steps.click("#a")])
    assert rule.browser and rule.steps == [steps.click("#a")]
    assert netweir.Follow("a", browser=True).browser
    assert not netweir.Follow("a").browser
    with pytest.raises(ValueError, match="Follow's steps"):
        netweir.Follow("a", steps=[{"action": "nope"}])


def test_step_results_survive_pickling_as_a_worker_gets_them():
    from netweir._native import _response

    outcomes = (
        '[{"action": "screenshot", "status": "ok", "error": null, "elapsed": 0.5,'
        ' "value": null, "image": "iVBORw=="},'
        ' {"action": "evaluate", "status": "failed", "error": "no", "elapsed": 0.1,'
        ' "value": {"n": 1}, "image": null}]'
    )
    r = _response("https://a.test/", 200, "browser", [], b"<p>hi</p>", outcomes)
    back = pickle.loads(pickle.dumps(r))
    page = netweir.Page(back)
    assert page.steps == [
        {
            "action": "screenshot",
            "status": "ok",
            "error": None,
            "elapsed": 0.5,
            "result": b"\x89PNG",
        },
        {
            "action": "evaluate",
            "status": "failed",
            "error": "no",
            "elapsed": 0.1,
            "result": {"n": 1},
        },
    ]
    assert netweir.Page(_response("https://a.test/", 200, "HTTP/2", [], b"")).steps == []
    with pytest.raises(ValueError, match="outcomes"):
        _response("https://a.test/", 200, "browser", [], b"", "[1]")


def test_a_fetch_error_keeps_its_kind_and_steps_when_pickled():
    e = netweir.FetchError("step 1 (click) failed: x", "step", [{"action": "click"}])
    back = pickle.loads(pickle.dumps(e))
    assert (str(back), back.kind, back.steps) == (str(e), "step", [{"action": "click"}])
    assert netweir.FetchError("x").steps == []


# The crawls, in Chrome.

SHOP = """<!DOCTYPE html>
<style>#banner { position: fixed; inset: 0; background: white; } li { height: 30px; }</style>
<div id=banner><p>We use cookies</p>
<button id=accept onclick="document.cookie = 'consent=yes; path=/'; this.parentNode.remove()">OK</button></div>
<ul id=list><li>one</li></ul>
<button id=more onclick="more()">Load more</button>
<a class=plain href=/plain/1>1</a> <a class=plain href=/plain/2>2</a>
<script>
  async function more() {
    const r = await fetch('/api/items');
    for (const name of await r.json()) {
      document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
    }
  }
</script>"""

FORM = """<!DOCTYPE html><title>Search</title>
<form action=/results>
  <input id=q name=q>
  <select id=size name=size><option value=s>S</option><option value=m>M</option></select>
  <button id=go>Go</button>
</form>"""

FEED = """<!DOCTYPE html><style>li { height: 200px; }</style><ul id=feed></ul>
<script>
  let next = 1, loading = false, done = false;
  async function more() {
    if (loading || done) return;
    loading = true;
    const r = await fetch('/api/feed?page=' + next);
    if (r.ok) {
      for (const name of await r.json()) {
        document.querySelector('#feed').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
      }
      next++;
    } else {
      done = true;
    }
    loading = false;
  }
  addEventListener('scroll', () => {
    if (innerHeight + scrollY > document.body.scrollHeight - 300) more();
  });
  more();
</script>"""

CHALLENGE = "<title>Just a moment...</title><p>checking your browser</p>"

UPLOADS: list[bytes] = []

UPLOAD = """<form id=f method=post action=/upload enctype=multipart/form-data>
<input type=file id=file name=file></form>
<script>document.querySelector('#file').addEventListener('change',
  () => document.querySelector('#f').submit())</script>"""

LATER = """<form id=f method=post action=/upload enctype=multipart/form-data>
<input type=file id=file name=file><button id=go>go</button></form>"""

OVERLAY = """<input id=q value="old" style="width: 300px; height: 40px">
<div id=ov style="position: fixed; inset: 0; background: rgba(0, 0, 0, 0.5)">loading</div>
<script>
window.log = [];
const q = document.querySelector('#q');
for (const t of ['mousedown', 'focus'])
  q.addEventListener(t, () => log.push(t + ':' + !!document.querySelector('#ov')));
setTimeout(() => document.querySelector('#ov').remove(), 800);
</script>"""

MASK = """<input id=q value="555-0100">
<script>document.querySelector('#q').addEventListener('keydown',
  e => { if (e.ctrlKey || e.metaKey) e.preventDefault(); });</script>"""

TYPING = """<!DOCTYPE html><input id=q value="old"><textarea id=long></textarea>
<p id=cell ondblclick="this.textContent = 'edited'">cell</p>
<script>
window.log = [];
for (const t of ['mousedown', 'keydown', 'keyup', 'input']) {
  document.addEventListener(t, e => log.push({t, key: e.key, code: e.code, keyCode: e.keyCode,
    shift: e.shiftKey, at: e.timeStamp, data: e.data ?? null}), true);
}
</script>"""


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        url = urlsplit(self.path)
        status, kind, extra = 200, "text/html", {}
        if url.path in ("/", "/shop", "/again"):
            body = SHOP
        elif url.path == "/shops":
            body = '<a class=shop href="/shop">shop</a>'
        elif url.path == "/api/items":
            body, kind = '["two", "three"]', "application/json"
        elif url.path == "/form":
            body = FORM
        elif url.path == "/results":
            q = parse_qs(url.query)
            body = f"<title>Results</title><p id=q>{q['q'][0]}</p><p id=size>{q['size'][0]}</p>"
        elif url.path == "/feed":
            body = FEED
        elif url.path == "/api/feed":
            page = int(parse_qs(url.query)["page"][0])
            if page > 3:
                status, body = 404, "[]"
            else:
                body = str([f"post {page}.{i}" for i in range(5)]).replace("'", '"')
            kind = "application/json"
        elif url.path == "/typing":
            body = TYPING
        elif url.path == "/sitemap.xml":
            host = f"http://{self.headers['Host']}"
            body = (
                '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">'
                f"<url><loc>{host}/plain/from-sitemap</loc></url></urlset>"
            )
            kind = "application/xml"
        elif url.path == "/up":
            body = UPLOAD
        elif url.path == "/mask":
            body = MASK
        elif url.path == "/later":
            body = LATER
        elif url.path == "/overlay":
            body = OVERLAY
        elif url.path == "/to-gate":
            body = "<a id=go href=/gate>go</a>"
        elif url.path == "/gate":
            status, body, extra = 403, CHALLENGE, {"cf-mitigated": "challenge"}
        else:
            body = f"<p>{url.path}</p>"
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", kind)
        for k, v in extra.items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        size = int(self.headers.get("Content-Length", 0))
        UPLOADS.append(self.rfile.read(size))
        data = b"<p id=done>got it</p>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
        try:
            asyncio.run(_probe())
        except netweir.BrowserError as e:
            pytest.skip(str(e))
    servers = serve(Handler)
    yield f"http://127.0.0.1:{servers[0].server_port}"
    for server in servers:
        server.shutdown()
        server.server_close()


async def _probe():
    async with netweir.browser():
        pass


def settings(**kw):
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        retries=1,
        backoff_base=0.01,
        backoff_max=0.05,
        **kw,
    )


def run(spider, **kw):
    items = []
    spider.settings = settings(**kw)
    stats = spider.run(output=items.append)
    return items, stats


def test_steps_dismiss_a_banner_and_load_more_before_the_callback(base):
    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/shop",
                steps=[
                    steps.click("#accept"),
                    steps.click("#more"),
                    steps.wait_for_response("**/api/items"),
                    steps.wait_for("li:nth-child(3)"),
                    steps.evaluate("document.querySelectorAll('li').length"),
                    steps.screenshot(),
                ],
            )

        async def parse(self, page):
            yield {
                "items": page.css("li::text").getall(),
                "banner": page.css("#banner").get(),
                "steps": page.steps,
                "cookies": [c["name"] for c in await page.browser.cookies()],
            }

    items, stats = run(Shop())
    (item,) = items
    assert item["items"] == ["one", "two", "three"]
    assert item["banner"] is None, "the HTML is the page as the steps left it"
    assert [s["status"] for s in item["steps"]] == ["ok"] * 6
    assert item["steps"][2]["result"]["status"] == 200
    assert item["steps"][4]["result"] == 3
    assert item["steps"][5]["result"].startswith(b"\x89PNG")
    assert "consent" in item["cookies"]
    assert stats["browser_steps"] == 6
    assert stats["browser_steps_failed"] == 0


def test_steps_fill_a_form_and_follow_it_to_the_results(base):
    class Search(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/form",
                steps=[
                    steps.type("#q", "green tea", delay=0.01),
                    steps.select_option("#size", "m"),
                    steps.click("#go"),
                    steps.wait_for_navigation(),
                ],
            )

        def parse(self, page):
            yield {
                "url": page.url,
                "q": page.css("#q::text").get(),
                "size": page.css("#size::text").get(),
                "chose": page.steps[1]["result"],
            }

    items, _ = run(Search())
    assert items == [
        {"url": base + "/results?q=green+tea&size=m", "q": "green tea", "size": "m", "chose": ["m"]}
    ]


def test_a_scroll_step_reads_an_endless_feed_to_its_end(base):
    class Feed(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/feed",
                steps=[steps.wait_for("#feed li"), steps.scroll(idle=1, times=10)],
            )

        def parse(self, page):
            yield {"posts": len(page.css("#feed li")), "rounds": page.steps[1]["result"]}

    items, _ = run(Feed())
    assert items == [{"posts": 15, "rounds": 2}]


def test_crawl_wide_steps_run_on_every_rendered_page_unless_it_has_its_own(base):
    class Pages(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/shop", browser=True)
            yield netweir.Request(base + "/again", steps=[])

        def parse(self, page):
            yield {
                "url": page.url,
                "banner": page.css("#banner").get() is not None,
                "steps": [s["status"] for s in page.steps],
            }

    items, _ = run(
        Pages(),
        browser_steps=[
            steps.click("#accept", timeout=2, on_error="continue"),
            steps.click("#nothing-here", timeout=0.3, on_error="continue"),
        ],
    )
    by_url = {i["url"]: i for i in items}
    assert by_url[base + "/shop"] == {
        "url": base + "/shop",
        "banner": False,
        "steps": ["ok", "failed"],
    }
    assert by_url[base + "/again"]["steps"] == [] and by_url[base + "/again"]["banner"]


def test_a_failing_step_continues_stops_or_fails_the_request(base):
    failures = []

    class Picky(netweir.Spider):
        async def start(self):
            missing = {"action": "click", "selector": "#missing", "timeout": 0.3}
            yield netweir.Request(
                base + "/shop?continue",
                steps=[{**missing, "on_error": "continue"}, steps.click("#accept")],
            )
            yield netweir.Request(
                base + "/shop?stop",
                steps=[{**missing, "on_error": "stop"}, steps.click("#accept")],
            )
            yield netweir.Request(
                base + "/shop?fail",
                steps=[steps.click("#accept"), missing, steps.click("#more")],
                errback=self.failed,
            )

        def parse(self, page):
            yield {"url": page.url, "steps": [s["status"] for s in page.steps]}

        def failed(self, request, error):
            failures.append(
                (request.url, error.kind, str(error), [s["status"] for s in error.steps])
            )

    items, stats = run(Picky())
    by_url = {i["url"]: i["steps"] for i in items}
    assert by_url == {
        base + "/shop?continue": ["failed", "ok"],
        base + "/shop?stop": ["failed", "skipped"],
    }
    ((url, kind, message, statuses),) = failures
    assert (url, kind, statuses) == (base + "/shop?fail", "step", ["ok", "failed", "skipped"])
    assert message.startswith("step 2 (click) failed: ") and "#missing" in message
    assert stats["retries"] == 0, "a step that fails isn't tried again"


def test_call_steps_run_spider_methods_and_functions(base):
    def plain(page, request):
        return {"url": request.url}

    class Calls(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/shop",
                meta={"who": "ada"},
                steps=[
                    steps.call(self.dismiss),
                    plain,
                    steps.call("title"),
                    steps.call(self.broken, on_error="continue"),
                    steps.call(self.slow, timeout=0.3, on_error="continue"),
                    steps.call(self.not_json, on_error="continue"),
                ],
            )

        async def dismiss(self, page, request):
            await page.click("#accept")
            return request.meta["who"]

        async def title(self, page, request):
            return await page.evaluate("document.querySelectorAll('button').length")

        def broken(self, page, request):
            raise RuntimeError("no luck")

        async def slow(self, page, request):
            await asyncio.sleep(5)

        def not_json(self, page, request):
            return object()

        def parse(self, page):
            yield {"steps": [(s["status"], s["result"], s["error"]) for s in page.steps]}

    items, _ = run(Calls())
    (item,) = items
    assert item["steps"][:3] == [
        ("ok", "ada", None),
        ("ok", {"url": base + "/shop"}, None),
        ("ok", 1, None),
    ]
    assert item["steps"][3] == ("failed", None, "RuntimeError: no luck")
    assert item["steps"][4][0] == "failed" and "0.3" in item["steps"][4][2]
    assert item["steps"][5][0] == "failed" and "isn't JSON" in item["steps"][5][2]


def test_a_step_that_leads_to_a_challenge_goes_to_on_block(base):
    blocked = []

    class Gate(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/to-gate",
                steps=[steps.click("#go"), steps.wait_for("#prize", timeout=0.5)],
            )

        def parse(self, page):
            yield {"reached": page.url}

        def on_block(self, request, page):
            blocked.append((page.url, page.blocked, [s["status"] for s in page.steps]))

    items, _ = run(Gate(), timeout=2)
    assert items == []
    assert blocked == [(base + "/gate", "cloudflare", ["ok", "failed"])]


def test_a_rules_spider_carries_steps_on_its_rules(base):
    spider = step_spiders.RuledSteps()
    spider.start_urls = [base + "/shops"]
    items, stats = run(spider)
    assert [(i["items"], i["results"]) for i in items] == [
        (["one", "two", "three"], [None, None, None, 3])
    ]
    assert stats["browser_fetches"] == 1


def test_steps_with_workers_and_a_rules_page_sent_to_one(base):
    spider = step_spiders.Counted()
    spider.start_urls = [base + "/shop"]
    items, _ = run(spider, workers=2)
    rendered = [i for i in items if "items" in i]
    assert rendered == [
        {
            "items": ["one", "two", "three"],
            "steps": [
                ("click", "ok", None),
                ("click", "ok", None),
                ("wait_for", "ok", None),
                ("call", "ok", 3),
            ],
            "pid": os.getpid(),
        }
    ], "a live page's callback, and its call steps, run in this process"
    assert sorted(i["plain"] for i in items if "plain" in i) == [
        base + "/plain/1",
        base + "/plain/2",
    ]

    spider = step_spiders.RuledSteps()
    spider.start_urls = [base + "/shops"]
    items, _ = run(spider, workers=2)
    ((item),) = items
    assert item["results"] == [None, None, None, 3], "the steps' results reach the worker"
    assert item["pid"] != os.getpid()


def test_a_checkpoint_keeps_a_pending_requests_steps(base, tmp_path):
    class Stop(Exception):
        pass

    seen = []

    class Once(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/plain/first")

        def parse(self, page):
            if page.url.endswith("/first"):
                yield netweir.Request(
                    base + "/shop",
                    callback="shop",
                    steps=[
                        steps.call("dismiss"),
                        steps.click("#more"),
                        steps.wait_for("li:nth-child(3)"),
                    ],
                )
                raise Stop

        async def dismiss(self, page, request):
            await page.click("#accept")
            return "dismissed"

        def shop(self, page):
            seen.append((page.css("li::text").getall(), page.steps[0]["result"]))

    spider = Once()
    spider.settings = settings(checkpoint=str(tmp_path / "state"), fail_fast=True)
    with pytest.raises(Stop):
        spider.run()
    assert seen == []
    spider = Once()
    spider.settings = settings(checkpoint=str(tmp_path / "state"))
    spider.run()
    assert seen == [(["one", "two", "three"], "dismissed")]


def test_a_checkpoint_wont_keep_a_call_step_it_cant_name(base, tmp_path):
    class Lambda(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/shop", steps=[lambda page, request: None])

    spider = Lambda()
    spider.settings = settings(checkpoint=str(tmp_path / "state"))
    with pytest.raises(TypeError, match="call steps are saved by name"):
        spider.run()


def test_typing_clicks_holds_keys_and_shifts_as_a_person_does(base):
    async def go():
        async with netweir.browser() as browser:
            page = await browser.new_page()
            await page.goto(base + "/typing")
            await page.type("#q", "Ab.@é tea", delay=(0.06, 0.22))
            return (
                await page.evaluate("log"),
                await page.evaluate("document.querySelector('#q').value"),
            )

    log, value = asyncio.run(go())
    assert value == "Ab.@é tea"
    clicked = next(e["at"] for e in log if e["t"] == "mousedown")
    downs = [e for e in log if e["t"] == "keydown"]
    ups = [e for e in log if e["t"] == "keyup"]
    assert downs[0]["at"] - clicked >= 150, "a moment between the click and the first key"
    key = {e["key"]: e for e in downs}
    assert (key["A"]["code"], key["A"]["keyCode"], key["A"]["shift"]) == ("KeyA", 65, True)
    assert (key["."]["code"], key["."]["keyCode"]) == ("Period", 190)
    assert (key["@"]["code"], key["@"]["keyCode"], key["@"]["shift"]) == ("Digit2", 50, True)
    assert "é" not in key and any(e["data"] == "é" for e in log if e["t"] == "input")
    typed = [e for e in downs if len(e["key"]) == 1]
    holds = []
    for down in typed:
        up = next(u for u in ups if u["code"] == down["code"] and u["at"] >= down["at"])
        holds.append(up["at"] - down["at"])
        assert holds[-1] >= 40, (down["key"], holds[-1])
    # Keys are held 45 to 180 ms. A busy runner can only lengthen one, and
    # a macOS runner's "." came up after 427.5 ms, so the ceiling is on the
    # middle hold rather than on every key.
    assert sorted(holds)[len(holds) // 2] <= 400, holds
    gaps = [b["at"] - a["at"] for a, b in zip(typed, typed[1:], strict=False)]
    mean = sum(gaps) / len(gaps)
    spread = (sum((g - mean) ** 2 for g in gaps) / len(gaps)) ** 0.5
    assert spread > 5, gaps


def test_a_long_type_step_has_time_for_every_key(base):
    class Long(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/typing",
                steps=[
                    steps.type("#long", "x" * 40, delay=0, timeout=1),
                    steps.evaluate("document.querySelector('#long').value.length"),
                ],
            )

        def parse(self, page):
            yield {"steps": [(s["status"], s["result"]) for s in page.steps]}

    items, _ = run(Long())
    assert items == [{"steps": [("ok", None), ("ok", 40)]}]


def test_huge_and_overlong_steps_are_refused_where_they_are_written(base):
    with pytest.raises(ValueError, match="at most"):
        netweir.Request(base + "/", steps=[steps.click("#a", timeout=1e19)])
    with pytest.raises(ValueError, match="at most"):
        netweir.Request(base + "/", steps=[steps.wait(1e300)])
    with pytest.raises(ValueError, match="browser_steps_max"):
        netweir.Settings(browser_steps=[steps.wait(200)])
    with pytest.raises(ValueError, match="positive"):
        netweir.Settings(browser_steps_max=0)
    assert netweir.Settings(browser_steps=[steps.wait(200)], browser_steps_max=None)

    class Long(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/shop", steps=[steps.wait(200)])

        def parse(self, page):
            yield {"fetched": page.url}

    items, stats = run(Long())
    assert items == [] and stats["callback_errors"] == 1
    assert stats.get("browser_fetches", 0) == 0


def test_a_pages_steps_stop_at_browser_steps_max(base):
    failures = []

    class Slow(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/shop",
                steps=[
                    steps.wait(0.5),
                    steps.wait_for("#never", on_error="continue"),
                    steps.click("#more"),
                ],
                errback=self.lost,
            )

        def parse(self, page):
            yield {"reached": page.url}

        def lost(self, request, error):
            failures.append((error.kind, str(error), [s["status"] for s in error.steps]))

    started = time.monotonic()
    items, _ = run(Slow(), browser_steps_max=1.5)
    assert time.monotonic() - started < 20
    assert items == []
    ((kind, message, statuses),) = failures
    assert kind == "step" and "browser_steps_max" in message
    assert statuses == ["ok", "failed", "skipped"]


def test_other_steps_on_the_same_url_are_another_request(base):
    class Twice(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/shop", browser=True)
            yield netweir.Request(base + "/shop", steps=[steps.click("#accept")])
            yield netweir.Request(
                base + "/shop", steps=[steps.click("#accept", timeout=4, on_error="continue")]
            )
            yield netweir.Request(
                base + "/shop",
                steps=[steps.click("#more")],
                meta={"steps_in_fingerprint": False},
            )

        def parse(self, page):
            yield {"steps": [s["action"] for s in page.steps]}

    items, stats = run(Twice())
    assert sorted(i["steps"] for i in items) == [[], ["click"]]
    assert stats["duplicates"] == 2, "a timeout doesn't make another page, nor a flag's steps"


def test_a_plain_request_after_one_with_steps_is_still_fetched(base):
    class Then(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/shop", steps=[steps.click("#accept")])

        def parse(self, page):
            yield {"steps": [s["action"] for s in page.steps]}
            if page.steps:
                # Chrome ended at /shop, with the steps: the plain page
                # hasn't been seen.
                yield netweir.Request(base + "/shop", browser=True)

    items, stats = run(Then())
    assert [i["steps"] for i in items] == [["click"], []]
    assert stats["duplicates"] == 0


def test_deltafetch_keys_a_page_by_its_steps(base, tmp_path):
    delta = str(tmp_path / "delta")

    def crawl(spider):
        out = tmp_path / f"items{time.monotonic_ns()}.jsonl"
        spider.settings = settings(deltafetch=delta)
        stats = spider.run(output=str(out))
        return out.read_text().splitlines() if out.exists() else [], stats

    class Once(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/shop", steps=[steps.click("#accept")])

        def parse(self, page):
            yield {"url": page.url}

    items, _ = crawl(Once())
    assert len(items) == 1
    items, stats = crawl(Once())
    assert items == [] and stats["skipped_delta"] == 1

    class Other(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/shop", steps=[steps.click("#accept"), steps.click("#more")]
            )
            yield netweir.Request(base + "/shop", browser=True)

        def parse(self, page):
            yield {"url": page.url}

    items, stats = crawl(Other())
    assert len(items) == 2 and stats.get("skipped_delta", 0) == 0


def queued(spider, **kw):
    """Runs ``spider`` with a shared queue (the stand-in for Redis), where
    every request, the spider's own too, is read back from the queue."""
    from fake_redis import FakeRedis

    fake = FakeRedis()
    try:
        return run(spider, queue=fake.url, queue_key="steps", queue_idle=1.0, **kw)
    finally:
        fake.close()


def test_with_a_queue_a_spiders_own_callbacks_go_through(base, caplog):
    class Own(netweir.Spider):
        sitemap_urls = [base + "/sitemap.xml"]

        def parse(self, page):
            yield {"parse": page.url}
            if page.url.endswith("/from-sitemap"):
                yield netweir.Request(base + "/plain/private", callback=self._detail)
                yield netweir.Request(base + "/plain/lost", errback=self._lost)

        def _detail(self, page):
            yield {"private": page.url}

        def _lost(self, request, error):
            pass

    with caplog.at_level("WARNING", logger="netweir"):
        items, stats = queued(Own())
    assert sorted(next(iter(i.values())) for i in items) == [
        base + "/plain/from-sitemap",
        base + "/plain/lost",
        base + "/plain/private",
    ], caplog.text
    assert stats.get("queue_bad", 0) == 0


def test_with_a_queue_netweirs_own_machinery_is_never_a_callback(base, caplog):
    class Steered(netweir.Spider):
        async def start(self):
            # start and closed are this spider's own, and still netweir's
            # hooks: never a callback.
            for name in (
                "closed",
                "crawl",
                "run",
                "opened",
                "start",
                "sitemap_filter",
                "logger",
            ):
                yield netweir.Request(base + f"/plain/{name}", callback=name, dont_filter=True)
            yield netweir.Request(base + "/plain/dunder", callback="__init__")
            yield netweir.Request(base + "/plain/none", callback="nowhere")
            yield netweir.Request(base + "/plain/call", steps=[steps.call("closed")])
            yield netweir.Request(base + "/plain/fine", callback="on_block")

        def on_block(self, request, page=None):
            yield {"on_block": True}

        def closed(self, reason):
            CLOSED.append(reason)

    CLOSED = []
    with caplog.at_level("WARNING", logger="netweir"):
        items, stats = queued(Steered())
    assert items == [{"on_block": True}]
    assert stats["queue_bad"] == 10
    assert CLOSED == ["finished"], "closed() only ever runs as the crawl ends"
    said = caplog.text
    assert "netweir.Spider's own closed, which netweir calls itself" in said
    assert "netweir.Spider's own start" in said
    assert "netweir.Spider's own crawl" in said and "netweir.Spider's own run" in said
    assert "'__init__' starts with two underscores" in said
    assert "'nowhere' isn't defined on Steered" in said
    assert "its call step 'closed'" in said


def test_with_a_queue_every_upload_needs_browser_upload_dir(base, tmp_path, caplog):
    folder = tmp_path / "uploads"
    folder.mkdir()
    (folder / "cv.txt").write_text("MY-CV")
    outside = tmp_path / "secret.txt"
    outside.write_text("SECRET")
    (folder / "link.txt").symlink_to(outside)

    def uploader(*paths):
        class Up(netweir.Spider):
            async def start(self):
                for i, path in enumerate(paths):
                    yield netweir.Request(
                        base + f"/up?{i}",
                        steps=[steps.set_input_files("#file", str(path)), steps.wait_for("#done")],
                    )

            def parse(self, page):
                yield {"done": page.css("#done::text").get()}

        return Up()

    UPLOADS.clear()
    with caplog.at_level("WARNING", logger="netweir"):
        items, stats = queued(uploader(folder / "cv.txt"))
    assert items == [] and stats["queue_bad"] == 1
    assert "Settings.browser_upload_dir, which isn't set" in caplog.text
    assert "with a shared queue, every request is" in caplog.text

    caplog.clear()
    escapes = [folder / "link.txt", folder / ".." / "secret.txt"]
    with caplog.at_level("WARNING", logger="netweir"):
        items, stats = queued(uploader(folder / "cv.txt", *escapes), browser_upload_dir=str(folder))
    assert items == [{"done": "got it"}]
    assert stats["queue_bad"] == 2, caplog.text
    assert caplog.text.count("isn't in Settings.browser_upload_dir") == 2
    assert len(UPLOADS) == 1 and b"MY-CV" in UPLOADS[0] and b"SECRET" not in UPLOADS[0]


def test_an_upload_swapped_for_a_link_after_the_check_is_refused(base, tmp_path):
    folder = tmp_path / "uploads"
    folder.mkdir()
    cv = folder / "cv.txt"
    cv.write_text("MY-CV")
    outside = tmp_path / "secret.txt"
    outside.write_text("SECRET")

    class Swap(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/up",
                steps=[
                    steps.call("swap"),
                    steps.set_input_files("#file", str(cv)),
                    steps.wait_for("#done", timeout=2),
                ],
                errback=self.lost,
            )

        async def swap(self, page, request):
            # After the check made as the request came back from the queue.
            cv.unlink()
            cv.symlink_to(outside)

        def parse(self, page):
            yield {"done": True}

        def lost(self, request, error):
            yield {"error": str(error)}

    UPLOADS.clear()
    items, _ = queued(Swap(), browser_upload_dir=str(folder))
    assert len(items) == 1 and "outside the upload folder" in items[0]["error"], items
    assert UPLOADS == []


def test_type_replaces_a_field_that_swallows_select_all(base):
    class Mask(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/mask",
                steps=[
                    steps.type("#q", "555-0199", delay=0.01),
                    steps.evaluate("document.querySelector('#q').value"),
                ],
            )

        def parse(self, page):
            yield {"value": page.steps[1]["result"]}

    items, _ = run(Mask())
    assert items == [{"value": "555-0199"}]


def test_an_upload_on_another_windows_drive_is_outside_the_folder(monkeypatch):
    import ntpath

    from netweir._crawl import _inside

    assert _inside("/up", "/up/cv.pdf") and not _inside("/up", "/upload/cv.pdf")
    # Windows has no common path for two drives: that's outside, not an error.
    monkeypatch.setattr(os.path, "commonpath", ntpath.commonpath)
    assert not _inside("c:\\up", "d:\\etc\\hosts")
    assert _inside("c:\\up", "c:\\up\\cv.pdf")
    # And it writes the common path with backslashes, however it was given.
    assert _inside("/up", "/up/cv.pdf")


def test_a_resumed_upload_step_needs_browser_upload_dir(base, tmp_path):
    class Stop(Exception):
        pass

    upload = tmp_path / "up"
    upload.mkdir()
    (upload / "a.txt").write_text("a")
    seen = []

    class Uploads(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/plain/first")

        def parse(self, page):
            if page.url.endswith("/first"):
                yield netweir.Request(
                    base + "/shop",
                    callback="shop",
                    steps=[
                        steps.evaluate("1"),
                        steps.set_input_files("#none", str(upload / "a.txt"), on_error="continue"),
                    ],
                )
                raise Stop

        def shop(self, page):
            seen.append(page.url)

    state = str(tmp_path / "state")
    spider = Uploads()
    spider.settings = settings(checkpoint=state, fail_fast=True)
    with pytest.raises(Stop):
        spider.run()
    spider = Uploads()
    spider.settings = settings(checkpoint=state)
    spider.run()
    assert seen == [], "refused on resume without browser_upload_dir"
    spider = Uploads()
    spider.settings = settings(checkpoint=state, browser_upload_dir=str(upload))
    spider.run()
    assert seen == [base + "/shop"]


def test_a_plain_function_call_step_is_warned_about_once(base, caplog):
    def plain(page, request):
        return 1

    class Plain(netweir.Spider):
        async def start(self):
            for i in range(2):
                yield netweir.Request(base + f"/shop?{i}", steps=[plain])

        def parse(self, page):
            yield {"r": page.steps[0]["result"]}

    with caplog.at_level("WARNING", logger="netweir"):
        items, _ = run(Plain())
    assert [i["r"] for i in items] == [1, 1]
    assert sum("plain function" in r.getMessage() for r in caplog.records) == 1


def test_a_rules_call_step_gets_the_request_the_rule_made(base):
    seen = []

    class Ruled(netweir.Spider):
        start_urls = [base + "/shops"]
        rules = (netweir.Follow("a.shop", callback="shop", steps=[steps.call("look")]),)

        async def look(self, page, request):
            seen.append((request.url, request.depth, request.callback))
            return "looked"

        def shop(self, page):
            yield {"result": page.steps[0]["result"]}

    items, _ = run(Ruled())
    assert items == [{"result": "looked"}]
    assert seen == [(base + "/shop", 1, "shop")]


def test_double_clicks_xpath_jpeg_and_cached_steps(base):
    class Cell(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/typing",
                steps=[
                    steps.double_click("xpath=//p[@id='cell']"),
                    steps.screenshot(quality=50),
                ],
            )

        def parse(self, page):
            yield {
                "cell": page.css("#cell::text").get(),
                "jpeg": page.steps[1]["result"][:2] == b"\xff\xd8",
                "cached": page.steps is page.steps,
            }

    items, _ = run(Cell())
    assert items == [{"cell": "edited", "jpeg": True, "cached": True}]


def test_an_upload_swapped_after_it_was_set_still_sends_what_was_checked(base, tmp_path):
    folder = tmp_path / "uploads"
    folder.mkdir()
    cv = folder / "cv.txt"
    cv.write_text("MY-CV")
    outside = tmp_path / "secret.txt"
    outside.write_text("SECRET")

    class Later(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/later",
                steps=[
                    steps.set_input_files("#file", str(cv)),
                    # Chrome reads the file only as the form goes.
                    steps.call("swap"),
                    steps.click("#go"),
                    steps.wait_for("#done", timeout=5),
                ],
            )

        async def swap(self, page, request):
            cv.unlink()
            cv.symlink_to(outside)

        def parse(self, page):
            yield {"done": page.css("#done::text").get()}

    UPLOADS.clear()
    items, _ = queued(Later(), browser_upload_dir=str(folder))
    assert items == [{"done": "got it"}]
    (sent,) = UPLOADS
    assert b"MY-CV" in sent and b"SECRET" not in sent
    assert b'filename="cv.txt"' in sent, "the copy keeps the file's own name"


def test_type_waits_for_an_overlay_rather_than_focusing_from_script(base):
    class Overlay(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                base + "/overlay",
                steps=[
                    steps.type("#q", "new", delay=0.01),
                    steps.evaluate("[log, document.querySelector('#q').value]"),
                ],
            )

        def parse(self, page):
            yield {"r": page.steps[1]["result"]}

    items, _ = run(Overlay())
    ((log, value),) = [i["r"] for i in items]
    assert value == "new"
    assert log[0] == "mousedown:false", "clicked once the overlay had gone"
