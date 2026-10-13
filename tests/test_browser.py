"""The browser driver from Python. Skipped without Chrome, unless
NETWEIR_REQUIRE_CHROME is set."""

import asyncio
import json
import os
import re
import socket
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

PAGES = {
    "/": b"""<!DOCTYPE html><title>shop</title>
<ul id=list></ul>
<script>
  for (const name of ['tea', 'cake']) {
    document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
  }
</script>
<button id=more style="display:none" onclick="document.querySelector('#list').insertAdjacentHTML('beforeend', '<li>jam</li>')">more</button>
<script>setTimeout(() => document.querySelector('#more').style.display = '', 200)</script>
<a id=away href="/missing">away</a>
<form onsubmit="event.preventDefault(); window.searched = document.querySelector('#q').value">
  <input id=q>
</form>
""",
    # An endless feed: the next page of items is fetched as you near the end.
    "/feed": b"""<!DOCTYPE html>
<style>li { height: 150px; }</style>
<ul id=list></ul>
<script>
  let next = 1, loading = false, done = false;
  async function more() {
    if (loading || done) return;
    loading = true;
    const r = await fetch('/api/items?page=' + next);
    if (r.ok) {
      const items = (await r.json()).items;
      for (const name of items) {
        document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
      }
      next++;
    } else {
      done = true;
    }
    loading = false;
  }
  addEventListener('scroll', () => {
    if (scrollY + innerHeight > document.documentElement.scrollHeight - 300) more();
  });
  more();
</script>
""",
    "/form": b"""<!DOCTYPE html>
<style>#tip { display: none; position: absolute } #help:hover + #tip { display: block }</style>
<form method=post action=/upload enctype=multipart/form-data>
  <select name=size id=size onchange="window.changed = this.value">
    <option value=s>Small</option><option value=m>Medium</option><option value=l>Large</option>
  </select>
  <select name=extras id=extras multiple>
    <option value=milk>Milk</option><option value=sugar>Sugar</option><option value=lemon>Lemon</option>
  </select>
  <input type=checkbox name=gift id=gift value=yes>
  <input type=file name=doc id=doc>
  <span id=help>?</span><span id=tip>a tip</span>
  <button id=send>send</button>
</form>
""",
    # Grows whenever you near the end, forever.
    "/endless": b"""<!DOCTYPE html><div id=feed style="height: 3000px"></div>
<script>
  addEventListener('scroll', () => {
    const s = document.scrollingElement;
    if (scrollY > s.scrollHeight - s.clientHeight - 100) {
      setTimeout(() => feed.style.height = (feed.offsetHeight + 2000) + 'px', 50);
    }
  });
</script>
""",
    "/inner": b"""<!DOCTYPE html><title>inner</title>
<script>window.clicks = 0</script>
<button id=b onclick="clicks++">b</button>
<input id=t>
<select id=s><option value=a>A</option><option value=b>B</option></select>
<input id=c type=checkbox>
""",
    # A frame from this site, and one from another, which Chrome runs in a
    # process of its own.
    "/outer": b"""<!DOCTYPE html><title>outer</title>
<iframe name=same src="/inner"></iframe>
<script>
  document.write(`<iframe name=other src="http://localhost:${location.port}/inner"></iframe>`);
</script>
""",
}

# What the server was sent to /upload, and how often it served each page of
# items; the server's threads write them.
UPLOADS: list[bytes] = []
SERVED: dict[str, int] = {}
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        with LOCK:
            UPLOADS.append(body)
        reply = f"<p id=got>{len(body)} bytes</p>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def do_GET(self):
        if self.path.startswith("/api/items?page="):
            page = int(self.path.rsplit("=", 1)[1])
            with LOCK:
                SERVED[self.path] = SERVED.get(self.path, 0) + 1
            if page > 3:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            body = json.dumps({"items": [f"item {page}.{i}" for i in range(8)]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path == "/empty":
            self.send_response(204)
            self.end_headers()
            return
        body = PAGES.get(self.path)
        self.send_response(200 if body else 404)
        self.send_header("Content-Type", "text/html")
        if self.path == "/":
            self.send_header("Set-Cookie", "visit=1; Path=/")
        body = body or b"<p>missing</p>"
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Proxy(BaseHTTPRequestHandler):
    """Wants the login user:secret, and answers every request itself."""

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.headers.get("Proxy-Authorization") != "Basic dXNlcjpzZWNyZXQ=":
            self.send_response(407)
            self.send_header("Proxy-Authenticate", 'Basic realm="test"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = f"<p id=via>{self.path}</p>".encode()
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


@pytest.fixture
async def browser():
    try:
        b = await netweir.browser(timeout=30)
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    yield b
    await b.close()


async def test_a_rendered_page_reads_back_as_a_node(browser, base):
    page = await browser.new_page()
    response = await page.goto(base + "/")
    assert (response.status, response.url) == (200, base + "/")
    assert await page.title() == "shop"
    root = await page.parse()
    assert root.css("li::text").getall() == ["tea", "cake"]
    assert "<li>cake</li>" in await page.content()


async def test_actions_wait_and_evaluate_returns_python_values(browser, base):
    async with await browser.new_page() as page:
        await page.goto(base + "/")
        await page.click("#more")
        await page.wait_for("li:nth-child(3)")
        await page.fill("#q", "scones")
        await page.press("Enter")
        assert await page.evaluate("searched") == "scones"
        # Waiting starts before the click, so the navigation can't slip past.
        moved = asyncio.ensure_future(page.wait_for_navigation())
        await page.click("#away")
        moved = await moved
        assert (moved.status, moved.url) == (404, base + "/missing")
        assert await page.evaluate(
            "() => ({n: 1, f: 1.5, ok: true, none: null, list: [1, 'a']})"
        ) == {
            "n": 1,
            "f": 1.5,
            "ok": True,
            "none": None,
            "list": [1, "a"],
        }
    assert page.closed


async def test_failures_raise_what_they_are(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/")
    with pytest.raises(netweir.BrowserTimeout, match="visible") as timed_out:
        await page.wait_for("#q-missing", timeout=0.5)
    assert isinstance(timed_out.value, TimeoutError)
    with pytest.raises(netweir.BrowserTimeout, match="nothing matched"):
        await page.click("#nothing-here", timeout=0.5)
    with pytest.raises(netweir.BrowserError, match="boom"):
        await page.evaluate("(() => { throw new Error('boom') })()")
    with pytest.raises(ValueError, match="wait must be"):
        await page.goto(base + "/", wait="soon")
    closed = socket.socket()
    closed.bind(("127.0.0.1", 0))
    port = closed.getsockname()[1]
    closed.close()
    with pytest.raises(netweir.FetchError, match="REFUSED") as refused:
        await page.goto(f"http://127.0.0.1:{port}/")
    assert refused.value.kind == "connect"
    # Chrome abandons a navigation to a 204; the server was reached.
    with pytest.raises(netweir.FetchError, match="ERR_ABORTED") as aborted:
        await page.goto(base + "/empty")
    assert aborted.value.kind == "other"
    for bad in (0, -1, float("inf"), 1e300):
        with pytest.raises(ValueError, match="timeout"):
            await page.click("#q", timeout=bad)


async def test_cookies_are_dicts_and_stay_in_their_context(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/")
    cookies = await page.cookies()
    assert [(c["name"], c["value"], c["path"]) for c in cookies] == [("visit", "1", "/")]
    assert await (await browser.new_page()).cookies() == []

    async with await browser.new_context() as context:
        a, b = await context.new_page(), await context.new_page()
        await a.set_cookies([{"name": "k", "value": "v", "domain": "127.0.0.1"}])
        assert [(c["name"], c["value"], c["path"]) for c in await b.cookies()] == [("k", "v", "/")]
        with pytest.raises(ValueError, match="domain"):
            await a.set_cookies([{"name": "k", "value": "v"}])


async def test_screenshots_are_png_and_can_go_to_a_file(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/")
    png = await page.screenshot(tmp_path / "shot.png", full_page=True)
    assert png.startswith(b"\x89PNG") and (tmp_path / "shot.png").read_bytes() == png


async def test_the_browser_can_be_awaited_or_used_as_a_context_manager():
    try:
        async with netweir.browser() as b:
            assert b.version.startswith("Chrome/")
            page = await b.new_page()
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    assert b.closed
    with pytest.raises(netweir.BrowserError):
        await page.goto("about:blank")
    with pytest.raises(netweir.BrowserError):
        await b.new_page()
    assert not any(
        m in b.methods_sent() for m in ("Runtime.enable", "Console.enable", "Log.enable")
    )


async def test_a_proxy_with_a_login_carries_the_pages():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        try:
            b = await netweir.browser(proxy=f"http://user:secret@127.0.0.1:{server.server_port}")
        except netweir.BrowserError as e:
            if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
                pytest.skip(str(e))
            raise
        async with b:
            page = await b.new_page()
            # Not a loopback address, which Chrome would fetch directly.
            response = await page.goto("http://shop.test/item")
            assert response.status == 200
            assert (await page.parse()).css("#via::text").get() == "http://shop.test/item"
    finally:
        server.shutdown()


async def test_a_running_chrome_can_be_driven_and_is_left_running(base, tmp_path):
    from netweir._native import find_chrome

    try:
        executable = find_chrome()
    except netweir.BrowserError as e:
        if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    chrome = subprocess.Popen(
        [
            executable,
            "--headless",
            "--remote-debugging-port=0",
            "--no-first-run",
            f"--user-data-dir={tmp_path}",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        ws = next(
            line.split("DevTools listening on ", 1)[1].strip()
            for line in chrome.stderr
            if "DevTools listening on " in line
        )
        async with netweir.browser(connect=ws) as b:
            page = await b.new_page()
            await page.goto(base + "/")
            assert await page.title() == "shop"
        assert chrome.poll() is None, "Chrome kept running"
        with pytest.raises(ValueError, match="already running"):
            await netweir.browser(connect=ws, proxy="http://127.0.0.1:1")
        with pytest.raises(ValueError, match="already running"):
            await netweir.browser(connect=ws, headless=False)
    finally:
        chrome.kill()
        chrome.wait()


async def test_scrolling_an_endless_feed_reads_every_page_of_its_api(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/feed")
    await page.wait_for("li")
    items = []
    while True:
        async with page.expect_response("**/api/items?*") as info:
            loaded = await page.scroll(times=1)
        if not loaded:
            break
        items += (await (await info.value).json())["items"]
    assert items == [f"item {page}.{i}" for page in (2, 3) for i in range(8)]
    assert len((await page.parse()).css("li")) == 24
    assert await page.scroll() == 0, "nothing more comes"
    with LOCK:
        assert SERVED.get("/api/items?page=4") == 1, "the page stopped asking once it ran out"
    await page.evaluate("scrollTo(0, 0)")
    await page.scroll("li:last-child")
    assert await page.evaluate(
        "(() => { const r = document.querySelector('li:last-child').getBoundingClientRect(); "
        "return r.top >= 0 && r.bottom <= innerHeight; })()"
    )
    await page.scroll(y=-200)
    with pytest.raises(netweir.BrowserTimeout, match="nothing matched"):
        await page.scroll("#nothing", timeout=0.5)


async def test_a_form_is_filled_with_choices_and_a_file_and_sent(browser, base, tmp_path):
    upload = tmp_path / "notes.txt"
    upload.write_bytes(b"the bytes of the file")
    page = await browser.new_page()
    await page.goto(base + "/form")

    assert await page.select_option("#size", "m") == ["m"]
    assert await page.evaluate("changed") == "m"
    assert await page.select_option("#size", label="Large") == ["l"]
    assert await page.select_option("#size", index=0) == ["s"]
    assert await page.select_option("#extras", ["milk", "lemon"]) == ["milk", "lemon"]
    with pytest.raises(netweir.BrowserTimeout, match='no option has the value "xl"'):
        await page.select_option("#size", "xl", timeout=0.5)
    with pytest.raises(ValueError, match="one of value, label or index"):
        await page.select_option("#size", "m", label="Medium")

    await page.check("#gift")
    await page.check("#gift")
    assert await page.evaluate("document.querySelector('#gift').checked")
    await page.uncheck("#gift")
    await page.uncheck("#gift")
    assert not await page.evaluate("document.querySelector('#gift').checked")
    await page.check("#gift")

    await page.hover("#help")
    await page.wait_for("#tip")

    with pytest.raises(FileNotFoundError, match="missing.txt"):
        await page.set_input_files("#doc", tmp_path / "missing.txt")
    await page.set_input_files("#doc", upload)
    moved = asyncio.ensure_future(page.wait_for_navigation())
    await page.click("#send")
    await moved
    assert await page.title() == "" and "bytes" in await page.content()
    with LOCK:
        sent = UPLOADS[-1]
    assert b"the bytes of the file" in sent and b'filename="notes.txt"' in sent
    assert b'name="size"\r\n\r\ns' in sent and b'name="gift"\r\n\r\nyes' in sent
    assert b"milk" in sent and b"lemon" in sent and b"sugar" not in sent
    assert not any(
        m in browser.methods_sent()
        for m in ("Runtime.enable", "Console.enable", "Log.enable", "DOM.enable")
    )


async def test_frames_from_this_site_and_others_take_actions(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/outer")
    frames = await page.frames()
    assert [f.name for f in frames] == ["", "same", "other"]
    assert frames[0].url == base + "/outer"
    other = await page.frame(url="http://localhost:*/inner")
    assert other is not None and other.name == "other"
    assert await page.frame(url=re.compile(r"127\.0\.0\.1:\d+/inner$")) is not None
    assert await page.frame("nowhere") is None
    for name in ("same", "other"):
        frame = await page.frame(name)
        assert frame is not None and await frame.title() == "inner"
        await frame.click("#b")
        assert await frame.evaluate("clicks") == 1
        await frame.fill("#t", "hello")
        assert (await frame.parse()).css("#t").get() is not None
        assert await frame.evaluate("document.querySelector('#t').value") == "hello"
        assert await frame.select_option("#s", "b") == ["b"]
        await frame.check("#c")
        await frame.hover("#b")
        with pytest.raises(netweir.BrowserTimeout, match="#missing"):
            await frame.click("#missing", timeout=0.5)
    assert await page.evaluate("typeof clicks") == "undefined"


async def test_a_profile_given_to_the_browser_keeps_its_cookies_and_stays(base, tmp_path):
    profile = tmp_path / "profile"
    for visit in range(2):
        try:
            b = await netweir.browser(user_data_dir=profile)
        except netweir.BrowserError as e:
            if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
                pytest.skip(str(e))
            raise
        async with b:
            page = await b.new_page()
            if visit == 0:
                await page.goto(base + "/")
                await page.evaluate("document.cookie = 'kept=yes; max-age=3600; path=/'")
            else:
                cookies = {c["name"]: c["value"] for c in await page.cookies()}
                assert cookies.get("kept") == "yes", cookies
        assert profile.is_dir()
    with pytest.raises(ValueError, match="already running"):
        await netweir.browser(connect="ws://127.0.0.1:1/", user_data_dir=profile)


async def test_a_profile_under_home_is_found_there(tmp_path, monkeypatch):
    from netweir._native import find_chrome

    # The Chrome is found before home moves: netweir's own install is
    # found through home, and this is about the profile, not the Chrome.
    try:
        executable = find_chrome()
    except netweir.BrowserError as e:
        if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    monkeypatch.chdir(tmp_path)
    b = await netweir.browser(executable, user_data_dir="~/kept-profile")
    await b.close()
    assert (tmp_path / "kept-profile").is_dir()
    assert not (tmp_path / "~").exists()


async def test_a_directory_isnt_a_file_to_upload(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/form")
    with pytest.raises(IsADirectoryError, match="directory"):
        await page.set_input_files("#doc", tmp_path)


async def test_a_feed_still_growing_when_time_runs_out_says_how_far_it_got(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/endless")
    # A turn of the wheel from the bottom, so the first round is done in
    # moments however slow the machine; the next is a long way down.
    await page.evaluate(
        "scrollTo(0, document.scrollingElement.scrollHeight - document.scrollingElement.clientHeight - 200)"
    )
    with pytest.raises(netweir.BrowserTimeout, match="scrolling to the bottom") as stopped:
        await page.scroll(timeout=2)
    assert stopped.value.rounds >= 1
    with pytest.raises(netweir.BrowserTimeout) as other:
        await page.wait_for("#nothing", timeout=0.2)
    assert other.value.rounds is None
