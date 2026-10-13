"""What a Chrome page fetches, from Python: reading responses, and
blocking, changing and answering requests. Skipped without Chrome, unless
NETWEIR_REQUIRE_CHROME is set."""

import asyncio
import gc
import json
import logging
import os
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

SHOP = b"""<!DOCTYPE html><title>shop</title><img src="/cat.png">
<ul id=list></ul>
<button id=more onclick="fetch('/api/items?page=2').then(r => r.json()).then(d => {
  for (const name of d.items) document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`)
})">more</button>
"""

PAGES = {
    "/": ("text/html", SHOP),
    "/other": ("text/html", b"<title>other</title>"),
    "/cat.png": ("image/png", b"not really a png"),
    "/api/items?page=2": ("application/json", json.dumps({"items": ["tea", "cake"]}).encode()),
    "/robots.txt": ("text/plain", b"User-agent: *\nDisallow: /private\n"),
    "/private": ("text/html", b"<title>private</title>"),
    "/ok": ("text/html", b"<title>ok</title>"),
    # A frame from another site, which Chrome runs in a process of its own:
    # 127.0.0.1 and localhost are different sites.
    "/framed": (
        "text/html",
        b"""<!DOCTYPE html><script>
  document.write(`<iframe name=child src="http://localhost:${location.port}/child"></iframe>`);
</script>""",
    ),
    "/child": (
        "text/html",
        b"""<!DOCTYPE html><title>child</title>
<img src="/secret.png" onload="window.pic = true" onerror="window.pic = true">""",
    ),
    "/secret.png": ("image/png", b"not really a png"),
    "/secret-api": ("application/json", b'{"word": "secret"}'),
    "/word": ("application/json", b'{"word": "tea"}'),
    # Asks Chrome to fetch ahead every way it can, all of it where
    # robots.txt refuses, but for a plain prefetch to show it was read.
    "/speculating": (
        "text/html",
        b"""<!DOCTYPE html><title>speculating</title>
<script type="speculationrules">{"prerender": [{"source": "list", "urls": ["/private"]}],
 "prefetch": [{"source": "list", "urls": ["/private-pf"]}]}</script>
<link rel="prerender" href="/private-link"><link rel="prefetch" href="/word">
<a id="go" href="/private">go</a>""",
    ),
}


class Handler(BaseHTTPRequestHandler):
    # Each request's path and headers; the server answers on many threads.
    seen: list[tuple[str, dict[str, str]]] = []
    lock = threading.Lock()

    def log_message(self, *args):
        pass

    def do_GET(self):
        with Handler.lock:
            Handler.seen.append((self.path, dict(self.headers.items())))
        if self.path == "/go":
            # A fixed target: robots.txt rules it out.
            self.send_response(302)
            self.send_header("Location", "/private")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        kind, body = PAGES.get(self.path, ("text/html", b"<p>missing</p>"))
        self.send_response(200 if self.path in PAGES else 404)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def paths():
    with Handler.lock:
        return [p for p, _ in Handler.seen]


def headers_for(path):
    with Handler.lock:
        return [h for p, h in Handler.seen if p == path]


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


@pytest.fixture
async def browser():
    with Handler.lock:
        Handler.seen.clear()
    try:
        b = await netweir.browser(timeout=30)
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    yield b
    await b.close()


async def test_a_click_s_xhr_json_is_read_with_expect_response(browser, base):
    page = await browser.new_page()
    document = await page.goto(base + "/")
    assert (document.method, document.resource_type) == ("GET", "document")
    assert await document.body() == SHOP
    async with page.expect_response("**/api/items*") as info:
        await page.click("#more")
    response = await info.value
    assert (response.status, response.method, response.resource_type) == (200, "GET", "fetch")
    assert await response.json() == {"items": ["tea", "cake"]}
    assert json.loads(await response.text()) == await response.json()
    assert await response.body() == PAGES["/api/items?page=2"][1]


async def test_on_response_hears_subresources_until_removed(browser, base):
    page = await browser.new_page()
    heard = []
    requests = []
    cat_heard = asyncio.Event()

    async def later(response):
        if response.url.endswith("/cat.png"):
            cat_heard.set()

    def hear(response):
        heard.append((response.resource_type, response.url))

    page.on_response(hear)
    page.on_response(later)
    page.on_request(lambda r: requests.append((r.method, r.url)))
    async with page.expect_response("**/cat.png"):
        await page.goto(base + "/")
    await asyncio.wait_for(cat_heard.wait(), 30)
    assert ("document", base + "/") in heard
    assert ("image", base + "/cat.png") in heard
    assert ("GET", base + "/cat.png") in requests

    page.remove_response_listener(hear)
    count = len(heard)
    async with page.expect_response("**/other") as info:
        await page.goto(base + "/other")
    await info.value
    assert len(heard) == count, "a removed listener hears nothing more"


async def test_globs_regexes_and_predicates_pick_the_response(browser, base):
    page = await browser.new_page()
    waits = [
        page.wait_for_response(re.compile(r"/cat\.png$")),
        page.wait_for_response(lambda r: r.resource_type == "document"),
        page.wait_for_response(base + "/*.png"),
        page.wait_for_request("**/cat.{png,jpg}"),
    ]
    tasks = [asyncio.ensure_future(w) for w in waits]
    # One turn of the loop: each wait registers before its first await.
    await asyncio.sleep(0)
    await page.goto(base + "/")
    cat, document, glob, request = await asyncio.gather(*tasks)
    assert cat.url == glob.url == request.url == base + "/cat.png"
    assert document.url == base + "/"
    with pytest.raises(netweir.BrowserTimeout, match="never/here"):
        await page.wait_for_response("**/never/here", timeout=0.3)
    with pytest.raises(TypeError, match="pattern"):
        await page.wait_for_response(42)


async def test_the_body_of_a_page_navigated_away_from_is_an_error(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/")
    async with page.expect_response("**/api/items*") as info:
        await page.click("#more")
    response = await info.value
    await page.goto(base + "/other")
    with pytest.raises(netweir.BrowserError, match="isn't available any more"):
        await asyncio.wait_for(response.body(), 30)


async def test_routes_abort_change_and_fulfill(browser, base):
    page = await browser.new_page()
    twice = []

    async def handle(route):
        if route.request.url.endswith("/cat.png"):
            await route.abort()
        elif "/api/" in route.request.url:
            await route.fulfill(json={"items": ["made", "up"]})
            assert route.handled
            try:
                await route.abort()
            except netweir.BrowserError as e:
                twice.append(str(e))
        else:
            headers = dict(route.request.headers) | {"X-Routed": "yes"}
            await route.continue_(headers=headers)

    await page.route("**/*", handle)
    await page.goto(base + "/")
    await page.click("#more")
    await page.wait_for("li:nth-child(2)")
    assert (await page.parse()).css("li::text").getall() == ["made", "up"]
    assert headers_for("/")[0].get("X-Routed") == "yes"
    assert "/cat.png" not in paths() and "/api/items?page=2" not in paths()

    await page.unroute("**/*")
    await page.goto(base + "/other")
    assert "X-Routed" not in headers_for("/other")[0]
    assert len(twice) == 1 and "already decided" in twice[0]


async def test_a_contexts_route_covers_its_pages_and_a_pages_own_comes_first(browser, base):
    context = await browser.new_context()
    page = await context.new_page()
    await context.route("**/other", lambda route: route.fulfill(body="<title>context</title>"))
    await page.goto(base + "/other")
    assert await page.title() == "context"
    second = await context.new_page()
    await second.route("**/other", lambda route: route.fulfill(body="<title>page</title>"))
    await second.goto(base + "/other")
    assert await second.title() == "page"
    # A page's routes that don't match leave the request to the context's.
    await second.goto(base + "/")
    await context.unroute("**/other")
    await page.goto(base + "/other")
    assert await page.title() == "other"


async def test_a_handler_that_raises_or_never_decides_lets_the_request_go_on(browser, base, caplog):
    page = await browser.new_page()

    def broken(route):
        raise RuntimeError("handler broke")

    async def undecided(route):
        pass

    await page.route("**/cat.png", broken)
    await page.route("**/other", undecided)
    with caplog.at_level(logging.ERROR, logger="netweir"):
        await page.goto(base + "/", timeout=20)
        await page.goto(base + "/other", timeout=20)
    assert await page.title() == "other"
    assert "/cat.png" in paths()
    assert "handler broke" in caplog.text


async def test_block_stops_images_before_they_leave_chrome(browser, base):
    page = await browser.new_page()
    await page.block()
    with pytest.raises(ValueError, match="unknown resource type"):
        await page.block(resource_types=("pictures",))
    await page.goto(base + "/")
    assert "/" in paths() and "/cat.png" not in paths()
    await page.block(resource_types=(), urls=["**/api/*"])
    assert (
        await page.evaluate("fetch('/api/items?page=2').then(() => 'ok', () => 'failed')")
        == "failed"
    )
    assert "/api/items?page=2" not in paths()


async def test_nothing_sent_is_something_a_page_can_see(browser, base):
    page = await browser.new_page()
    page.on_response(lambda r: None)
    await page.route("**/*", lambda route: route.continue_())
    await page.block(resource_types=("font",))
    async with page.expect_response("**/api/items*") as info:
        await page.goto(base + "/")
        await page.click("#more")
    await (await info.value).body()
    sent = browser.methods_sent()
    assert not {"Runtime.enable", "Console.enable", "Log.enable"} & set(sent), sent


async def _child(page):
    """The frame from another site, once its image has loaded or failed."""
    frame = await page.frame("child")
    assert frame is not None and frame.url.startswith("http://localhost:")
    await frame.evaluate(
        "new Promise(r => { const t = () => window.pic ? r() : setTimeout(t, 20); t() })"
    )
    return frame


FETCH = "fetch('{}').then(() => 'loaded', () => 'failed')"


async def test_a_frame_from_another_site_is_routed_heard_and_read(browser, base):
    page = await browser.new_page()
    requests, responses = [], []
    page.on_request(lambda r: requests.append(r.url))
    page.on_response(lambda r: responses.append(r.url))
    await page.route("**/secret*", lambda route: route.abort())
    await page.goto(base + "/framed")
    frame = await _child(page)
    assert await frame.evaluate(FETCH.format("/secret-api")) == "failed"
    assert "/secret.png" not in paths() and "/secret-api" not in paths()

    async with page.expect_response("**/word") as info:
        await frame.evaluate("fetch('/word')")
    response = await info.value
    assert response.url.startswith("http://localhost:")
    assert await response.json() == {"word": "tea"}
    assert response.url in requests and response.url in responses

    # Without the route, the frame's requests go straight through again.
    await page.unroute("**/secret*")
    assert await frame.evaluate(FETCH.format("/secret-api")) == "loaded"
    assert paths().count("/secret-api") == 1
    sent = browser.methods_sent()
    assert not {"Runtime.enable", "Console.enable", "Log.enable"} & set(sent), sent


async def test_block_stops_a_frame_from_another_site_s_images(browser, base):
    page = await browser.new_page()
    await page.block()
    await page.goto(base + "/framed")
    frame = await _child(page)
    assert "/child" in paths() and "/secret.png" not in paths()
    # Blocking more once the frame is there reaches it too.
    await page.block(resource_types=(), urls=["**/secret-api"])
    assert await frame.evaluate(FETCH.format("/secret-api")) == "failed"
    assert "/secret-api" not in paths()


def _settings(**kw):
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_tdmrep=False,
        retries=1,
        backoff_base=0.01,
        backoff_max=0.05,
        **kw,
    )


def test_a_crawl_s_chrome_pages_skip_what_browser_block_names(base):
    if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
        try:
            asyncio.run(_probe())
        except netweir.BrowserError as e:
            pytest.skip(str(e))
    with Handler.lock:
        Handler.seen.clear()
    titles = []

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/", browser=True)

        def parse(self, page):
            titles.append(page.css("title::text").get())

    spider = Shop()
    spider.settings = _settings(obey_robots=False, browser_block=("image", "font"))
    assert spider.run()["browser_fetches"] == 1
    assert titles == ["shop"]
    assert "/" in paths() and "/cat.png" not in paths()


def test_a_redirect_robots_txt_refuses_stays_refused_with_blocking_on(base):
    """Blocking puts more of the page's requests through the Fetch domain;
    the robots.txt guard still sees each document first."""
    if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
        try:
            asyncio.run(_probe())
        except netweir.BrowserError as e:
            pytest.skip(str(e))
    with Handler.lock:
        Handler.seen.clear()
    parsed = []

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/go", browser=True)

        def parse(self, page):
            parsed.append(page.url)

    spider = Shop()
    spider.settings = _settings(obey_robots=True, browser_block=("image",))
    stats = spider.run()
    assert parsed == []
    assert stats["dropped_robots"] == 1
    assert "/go" in paths() and "/private" not in paths()


def test_browser_block_is_checked():
    with pytest.raises(ValueError, match="unknown resource type 'images'"):
        netweir.Settings(browser_block=("images",))
    with pytest.raises(ValueError, match="document"):
        netweir.Settings(browser_block=("document",))
    with pytest.raises(ValueError, match="tuple"):
        netweir.Settings(browser_block="image")
    assert netweir.Settings(browser_block=["image"]).browser_block == ("image",)


def _chrome_or_skip():
    if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
        try:
            asyncio.run(_probe())
        except netweir.BrowserError as e:
            pytest.skip(str(e))


def test_a_route_can_t_send_a_crawl_s_page_where_robots_txt_refuses(base):
    _chrome_or_skip()
    with Handler.lock:
        Handler.seen.clear()
    outcome = []

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/other", browser=True)

        async def parse(self, page):
            live = page.browser
            await live.route("**/ok", lambda route: route.continue_(url=base + "/private"))
            try:
                await live.goto(base + "/ok")
                outcome.append(await live.title())
            except netweir.FetchError as e:
                outcome.append(str(e))

    spider = Shop()
    spider.settings = _settings(obey_robots=True)
    spider.run()
    assert len(outcome) == 1 and "BLOCKED_BY_CLIENT" in outcome[0], outcome
    assert "/private" not in paths()


def test_a_crawl_s_page_fetches_nothing_ahead_where_robots_txt_refuses(base):
    """Speculation rules and <link rel=prerender> would have Chrome fetch,
    and show, a page without the robots.txt guard seeing it."""
    _chrome_or_skip()
    with Handler.lock:
        Handler.seen.clear()
    outcome = []

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/speculating", browser=True)

        async def parse(self, page):
            live = page.browser
            for _ in range(1500):
                if "/word" in paths():
                    break
                await asyncio.sleep(0.02)
            # About as long again as Chrome takes to fetch ahead.
            await asyncio.sleep(2)
            outcome.append([p for p in paths() if p.startswith("/private")])
            await live.click("#go")
            for _ in range(1500):
                if live.url != base + "/speculating":
                    break
                await asyncio.sleep(0.02)
            outcome.append(live.url)

    spider = Shop()
    spider.settings = _settings(obey_robots=True)
    spider.run()
    assert outcome[0] == [], outcome
    assert not outcome[1].endswith("/private"), outcome
    assert not any(p.startswith("/private") for p in paths()), paths()


def test_listeners_and_routes_go_with_the_event_loop_that_added_them(base):
    """A page outlives an asyncio.run; what the first loop added must not
    hold the next loop's requests, and the next loop can add its own."""
    _chrome_or_skip()

    async def first():
        browser = await netweir.browser(timeout=30)
        page = await browser.new_page()
        await page.route("**/nomatch", lambda route: route.abort())
        page.on_response(lambda r: None)
        return browser, page

    browser, page = asyncio.run(first())

    async def second():
        try:
            heard = []
            page.on_response(lambda r: heard.append(r.url))
            # A request held for the first loop's routes would wait out the
            # browser's 30 seconds.
            async with page.expect_response("**/other") as info:
                await page.goto(base + "/other", timeout=10)
            await info.value
            assert base + "/other" in heard
            with Handler.lock:
                Handler.seen.clear()
            await page.route("**/cat.png", lambda route: route.abort())
            await page.goto(base + "/", timeout=10)
            assert "/" in paths() and "/cat.png" not in paths()
        finally:
            await browser.close()

    asyncio.run(second())


async def test_an_expectation_never_awaited_leaves_no_unretrieved_error(browser, base):
    page = await browser.new_page()
    loop = asyncio.get_running_loop()
    reported = []
    loop.set_exception_handler(lambda loop, context: reported.append(context))
    try:
        async with page.expect_response("**/never", timeout=0.2) as info:
            pass
        await asyncio.wait([info.value])
        del info
        gc.collect()
    finally:
        loop.set_exception_handler(None)
    assert reported == []


async def test_a_slow_predicate_doesn_t_hold_up_listeners(browser, base):
    page = await browser.new_page()
    release = asyncio.Event()

    async def slow(response):
        await release.wait()
        return False

    waiting = asyncio.ensure_future(page.wait_for_response(slow, timeout=30))
    await asyncio.sleep(0)
    heard = asyncio.Event()
    page.on_response(lambda r: heard.set() if r.url.endswith("/other") else None)
    try:
        await page.goto(base + "/other")
        await asyncio.wait_for(heard.wait(), 10)
    finally:
        release.set()
        waiting.cancel()


def test_a_wait_and_a_listener_from_another_threads_loop_run_on_that_loop(base):
    """One page, used from two threads, each with its own event loop: what
    the second loop asked for is answered on the second loop, promptly."""
    _chrome_or_skip()
    loops = []
    for _ in range(2):
        loop = asyncio.new_event_loop()
        threading.Thread(target=loop.run_forever, daemon=True).start()
        loops.append(loop)
    first, second = loops

    def on(loop, coro, timeout=60):
        return asyncio.run_coroutine_threadsafe(coro, loop).result(timeout)

    async def open_page():
        browser = await netweir.browser(timeout=30)
        page = await browser.new_page()
        # The first loop's listener is what reads the page's events.
        page.on_response(lambda r: None)
        return browser, page

    browser, page = on(first, open_page())
    heard_on = []

    async def start_waiting():
        page.on_response(
            lambda r: heard_on.append(threading.get_ident()) if r.url.endswith("/other") else None
        )
        waiting = asyncio.ensure_future(page.wait_for_response("**/other", timeout=8))
        await asyncio.sleep(0)
        return waiting, threading.get_ident()

    async def result(waiting):
        return await waiting

    try:
        waiting, second_thread = on(second, start_waiting())
        on(first, _later(page.goto, base + "/other"))
        started = time.monotonic()
        response = on(second, result(waiting), timeout=20)
        assert response.url == base + "/other"
        assert time.monotonic() - started < 4, "the second loop only woke at its timeout"

        async def heard():
            while not heard_on:
                await asyncio.sleep(0.01)

        on(second, asyncio.wait_for(heard(), 10))
        assert heard_on == [second_thread]
    finally:
        on(first, _later(browser.close))
        for loop in loops:
            loop.call_soon_threadsafe(loop.stop)


async def _later(method, *args):
    """Calls ``method`` on the loop running this, as its awaitable needs."""
    return await method(*args)


async def _probe():
    async with netweir.browser():
        pass
