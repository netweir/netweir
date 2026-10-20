"""Steps: what a request fetched in Chrome does on its page before the
callback sees it. The first tests need no Chrome; the crawls are skipped
without one, unless NETWEIR_REQUIRE_CHROME is set."""

import asyncio
import os
import pickle
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
        ' "value": null, "png": "iVBORw=="},'
        ' {"action": "evaluate", "status": "failed", "error": "no", "elapsed": 0.1,'
        ' "value": {"n": 1}, "png": null}]'
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


def test_typing_a_key_at_a_time_sends_every_key(base):
    async def go():
        async with netweir.browser() as browser:
            page = await browser.new_page()
            await page.goto(base + "/form")
            await page.evaluate(
                "() => { window.keys = 0; addEventListener('keydown', () => keys++) }"
            )
            await page.type("#q", "tea", delay=(0.01, 0.02))
            return await page.evaluate("[keys, document.querySelector('#q').value]")

    assert asyncio.run(go()) == [3, "tea"]
