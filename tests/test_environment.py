"""A page's environment from Python: contexts and pages given one, device
presets, settings changed later, and a crawl's Chrome given one. Every
surface is read back from page script, a worker and a frame from another
site. Skipped without Chrome, unless NETWEIR_REQUIRE_CHROME is set."""

import os
from http.server import BaseHTTPRequestHandler

import pytest
from local_server import serve

import netweir

READ = """
function read() {
  return {
    language: navigator.language,
    intl: Intl.DateTimeFormat().resolvedOptions().locale,
    zone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    agent: navigator.userAgent,
    mobile: navigator.userAgentData.mobile,
    platform: navigator.userAgentData.platform,
    init: self.initRan || 0,
    screen: typeof screen === 'undefined' ? null : [screen.width, screen.height, devicePixelRatio],
    dark: typeof matchMedia === 'undefined' ? null : matchMedia('(prefers-color-scheme: dark)').matches,
  };
}
"""

PAGE = (
    "<!doctype html><script>"
    + READ
    + """
window.surfaces = (async () => {
  await new Promise((ok) => document.readyState === 'complete' ? ok() : addEventListener('load', ok));
  const worker = await new Promise((ok) => { const w = new Worker('/worker.js'); w.onmessage = (m) => ok(m.data); });
  const frame = await new Promise((ok) => {
    addEventListener('message', (m) => ok(m.data));
    const f = document.createElement('iframe');
    f.src = location.origin.replace('127.0.0.1', 'localhost') + '/frame';
    document.documentElement.appendChild(f);
  });
  return { main: read(), worker, frame };
})();
</script>"""
)

FRAME = (
    "<!doctype html><script>"
    + READ
    + "addEventListener('load', () => parent.postMessage(read(), '*'));</script>"
)

WORKER = READ + "fetch('/from-worker').finally(() => postMessage(read()));"


class Handler(BaseHTTPRequestHandler):
    seen: dict[str, dict[str, str]] = {}

    def log_message(self, *args):
        pass

    def do_GET(self):
        path = self.path.split("?")[0]
        Handler.seen[path] = {k.lower(): v for k, v in self.headers.items()}
        kind, body = {
            "/": ("text/html", PAGE),
            "/frame": ("text/html", FRAME),
            "/worker.js": ("application/javascript", WORKER),
        }.get(path, ("text/plain", "x"))
        body = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    servers = serve(Handler)
    yield f"http://127.0.0.1:{servers[0].server_port}"
    for server in servers:
        server.shutdown()
        server.server_close()


async def _launch(**kw):
    try:
        return await netweir.browser(timeout=30, **kw)
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise


@pytest.fixture
async def browser():
    b = await _launch()
    yield b
    await b.close()


async def _surfaces(page, base):
    await page.goto(base + "/")
    return await page.evaluate("surfaces")


def _everywhere(s, key):
    return {s[w][key] for w in ("main", "worker", "frame") if s[w].get(key) is not None}


async def test_a_context_s_environment_is_the_same_on_every_surface(browser, base):
    context = await browser.new_context(
        locale="de-DE",
        timezone_id="Asia/Tokyo",
        screen={"width": 1440, "height": 900},
        device_scale_factor=2,
        viewport={"width": 1200, "height": 700},
        color_scheme="dark",
        extra_http_headers={"X-Extra": "yes"},
        init_scripts=["self.initRan = 1"],
    )
    page = await context.new_page()
    s = await _surfaces(page, base)
    assert _everywhere(s, "language") == {"de-DE"}
    assert _everywhere(s, "intl") == {"de-DE"}
    assert _everywhere(s, "zone") == {"Asia/Tokyo"}
    assert s["main"]["screen"] == s["frame"]["screen"] == [1440, 900, 2]
    assert s["main"]["dark"] is s["frame"]["dark"] is True
    assert (s["main"]["init"], s["frame"]["init"]) == (1, 1)
    assert page.viewport_size == {"width": 1200, "height": 700}
    for path in ("/", "/worker.js", "/from-worker", "/frame"):
        assert Handler.seen[path]["accept-language"] == "de-DE,de;q=0.9", path
        assert Handler.seen[path]["x-extra"] == "yes", path
    await context.close()


async def test_a_device_preset_is_a_phone_everywhere(browser, base):
    assert "Galaxy S23" in netweir.devices
    preset = netweir.devices["Galaxy S23"]
    assert preset["is_mobile"] and preset["has_touch"]
    page = await browser.new_page(**preset)
    s = await _surfaces(page, base)
    assert _everywhere(s, "mobile") == {True}
    assert _everywhere(s, "platform") == {"Android"}
    assert len(_everywhere(s, "agent")) == 1
    assert "Android" in s["main"]["agent"]
    assert s["main"]["screen"] == s["frame"]["screen"] == [360, 780, 3]
    assert Handler.seen["/"]["sec-ch-ua-mobile"] == "?1"
    assert Handler.seen["/frame"]["sec-ch-ua-platform"] == '"Android"'
    assert page.viewport_size == {"width": 360, "height": 704}
    # A phone has no mouse to hover with.
    with pytest.raises(ValueError, match="phone"):
        await page.hover("body")


async def test_settings_changed_later_reach_the_page(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/plain")
    assert page.viewport_size == {"width": 1280, "height": 720}
    await page.set_viewport_size({"width": 1000, "height": 600})
    assert await page.evaluate("[innerWidth, innerHeight]") == [1000, 600]
    await page.emulate_media(color_scheme="dark")
    assert await page.evaluate("matchMedia('(prefers-color-scheme: dark)').matches") is True
    script = tmp_path / "init.js"
    script.write_text("window.fromFile = 'yes'")
    await page.add_init_script(path=script)
    await page.set_extra_http_headers({"X-Later": "1"})
    await page.goto(base + "/later")
    assert await page.evaluate("window.fromFile") == "yes"
    assert Handler.seen["/later"]["x-later"] == "1"

    fetch = (
        "fetch('/plain').then(() => [navigator.onLine, 'ok'], () => [navigator.onLine, 'failed'])"
    )
    await page.set_offline(True)
    assert await page.evaluate(fetch) == [False, "failed"]
    await page.set_offline(False)
    assert await page.evaluate(fetch) == [True, "ok"]

    where = (
        "new Promise((ok) => navigator.geolocation.getCurrentPosition("
        "(p) => ok([p.coords.latitude, p.coords.longitude]), (e) => ok('error ' + e.code)))"
    )
    await page.set_geolocation({"latitude": 51.5, "longitude": -0.12})
    await page.grant_permissions(["geolocation"], origin=base)
    assert await page.evaluate(where) == [51.5, -0.12]
    states = "Promise.all(['geolocation', 'notifications'].map((n) => navigator.permissions.query({name: n}).then((s) => s.state)))"
    assert await page.evaluate(states) == ["granted", "prompt"]
    await page.clear_permissions()
    assert await page.evaluate(states) == ["prompt", "prompt"]


async def test_a_context_s_settings_change_for_all_its_pages(browser, base):
    context = await browser.new_context()
    first = await context.new_page()
    await first.goto(base + "/plain")
    await context.set_offline(True)
    await context.add_init_script("window.everyPage = 1")
    await context.set_extra_http_headers({"X-All": "1"})
    await context.set_geolocation({"latitude": 1, "longitude": 2, "accuracy": 3})
    await context.grant_permissions(["geolocation"])
    second = await context.new_page()
    for page in (first, second):
        assert await page.evaluate("navigator.onLine") is False
    await context.set_offline(False)
    await second.goto(base + "/second")
    assert await second.evaluate("window.everyPage") == 1
    assert Handler.seen["/second"]["x-all"] == "1"
    assert (
        await second.evaluate(
            "new Promise((ok) => navigator.geolocation.getCurrentPosition((p) => ok(p.coords.accuracy)))"
        )
        == 3
    )
    await context.clear_permissions()
    await context.close()


async def test_what_contradicts_itself_or_chrome_is_refused(browser):
    with pytest.raises(TypeError, match="unknown environment setting"):
        await browser.new_page(timezone="Asia/Tokyo")
    with pytest.raises(ValueError, match="mobile"):
        await browser.new_page(is_mobile=True)
    with pytest.raises(ValueError, match="no device preset"):
        await browser.new_page(device="Pixel 7")
    with pytest.raises(ValueError, match="timezone"):
        await browser.new_page(timezone_id="Mars/Olympus_Mons")
    with pytest.raises(ValueError, match="launch"):
        await browser.new_page(device_scale_factor=1.25)
    with pytest.raises(TypeError, match="list"):
        await browser.new_page(permissions="geolocation")
    with pytest.raises(ValueError, match="color_scheme"):
        await browser.new_page(color_scheme="purple")
    context = await browser.new_context(timezone_id="Europe/Paris")
    with pytest.raises(ValueError, match="context"):
        await context.new_page(timezone_id="Asia/Tokyo")
    await context.close()
    page = await browser.new_page()
    with pytest.raises(TypeError, match="script or a path"):
        await page.add_init_script()


async def test_a_browser_launched_with_an_environment_gives_it_every_page(base):
    browser = await _launch(
        environment={"locale": "fr-FR", "screen": {"width": 1366, "height": 768}}
    )
    try:
        page = await browser.new_page()
        s = await _surfaces(page, base)
        assert _everywhere(s, "language") == {"fr-FR"}
        assert s["main"]["screen"] == s["frame"]["screen"] == [1366, 768, 1]
        # Chrome's own list for a context it makes, which adds its own
        # language, sent the same by the page, its worker and its frame.
        for path in ("/", "/worker.js", "/from-worker", "/frame"):
            assert Handler.seen[path]["accept-language"] == "fr-FR,fr;q=0.9,en-US;q=0.8,en;q=0.7", (
                path
            )
    finally:
        await browser.close()


def test_a_crawl_s_chrome_pages_have_its_environment(base):
    seen = {}

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/crawled", browser=True)

        async def parse(self, page):
            seen["said"] = await page.browser.evaluate(
                "[navigator.language, Intl.DateTimeFormat().resolvedOptions().timeZone, navigator.userAgentData.mobile]"
            )

    spider = Shop()
    spider.settings = netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        browser_environment={
            "device": "Galaxy S22",
            "locale": "it-IT",
            "timezone_id": "Europe/Rome",
        },
    )
    try:
        spider.run()
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    if "said" not in seen and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
        pytest.skip("Chrome couldn't be started")
    assert seen["said"] == ["it-IT", "Europe/Rome", True]
    assert Handler.seen["/crawled"]["accept-language"] == "it-IT,it;q=0.9,en-US;q=0.8,en;q=0.7"
    assert Handler.seen["/crawled"]["sec-ch-ua-mobile"] == "?1"


def test_a_crawl_s_environment_is_checked_when_the_settings_are_made():
    with pytest.raises(TypeError, match="unknown environment setting"):
        netweir.Settings(browser_environment={"language": "de"})
    with pytest.raises(ValueError, match="dict"):
        netweir.Settings(browser_environment="Galaxy S23")
    settings = netweir.Settings(browser_environment={"device": "Galaxy S23"})
    assert settings.browser_environment == {"device": "Galaxy S23"}


async def test_media_and_no_viewport_are_as_playwright_has_them(browser, base):
    page = await browser.new_page(viewport=None, forced_colors="active")
    assert page.viewport_size is None
    await page.goto(base + "/plain")
    queries = "['print', '(forced-colors: active)', '(prefers-contrast: more)'].map((q) => matchMedia(q).matches)"
    assert await page.evaluate(queries) == [False, True, False]
    await page.emulate_media(media="print", contrast="more")
    assert await page.evaluate(queries) == [True, True, True]
    await page.emulate_media(media="null", forced_colors="null")
    assert await page.evaluate(queries) == [False, False, True]
    with pytest.raises(ValueError, match="media"):
        await page.emulate_media(media="paper")


async def test_a_phone_without_touch_or_a_desktop_with_it_is_refused(browser):
    with pytest.raises(ValueError, match="touch"):
        await browser.new_page(device="Galaxy S23", has_touch=False)
    with pytest.raises(ValueError, match="desktop"):
        await browser.new_page(has_touch=True)


async def test_worker_header_order_puts_a_worker_scripts_headers_back(base):
    want = ["host", "connection", "accept", "sec-fetch-site", "sec-fetch-mode", "sec-fetch-dest"]
    for tidy in (False, True):
        browser = await _launch(worker_header_order=tidy)
        try:
            page = await browser.new_page()
            await page.goto(base + "/")
            await page.evaluate("surfaces")
            names = list(Handler.seen["/worker.js"])
            assert names[: len(want)] == want if tidy else names[2:4] == ["accept", "user-agent"], (
                names
            )
        finally:
            await browser.close()
