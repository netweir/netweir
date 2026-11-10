"""Rendering pages in a browser, timed: netweir's Chrome and Firefox
against Playwright and Puppeteer.

    uv run python bench/browser.py                 # every tool it can reach
    uv run python bench/browser.py --only playwright-chrome

The pages are test.netweir.com's client-rendered shop (/js/spa/item/<id>/):
each arrives as an empty shell, and its script fetches the item and draws
it. Each tool renders all 30 items three times over, four pages at a time
in one headless browser, waits for the item to appear, and reads its name
and price. The names are checked against the site's answer key.

The same browser binaries for everyone where a tool can take them: the
Chrome for Testing and the Firefox that `netweir install` put in
~/.netweir. Playwright's Firefox is its own patched build (it can't drive
a stock one), so that row uses it.

A browser is many processes, so CPU and memory are the whole tree's:
psutil samples the tool's process and every descendant five times a
second. CPU is each process's last-seen total, summed, so the last fifth
of a second of a process that exits is missed; memory is the peak of the
summed RSS, which counts memory shared between processes more than once.
Both are a fair comparison between tools, not exact figures.

Install what you want to compare against:

    uv pip install playwright psutil && playwright install firefox
    (cd bench/others/puppeteer && PUPPETEER_SKIP_DOWNLOAD=1 npm install puppeteer)
"""

from __future__ import annotations

import argparse
import asyncio
import glob
import json
import os
import subprocess
import sys
import time
import urllib.request

SITE = "https://test.netweir.com"
ROUNDS = 3
PAGES_AT_ONCE = 4
HOME = os.environ.get("NETWEIR_HOME", os.path.expanduser("~/.netweir"))


def newest(pattern: str) -> str | None:
    found = sorted(glob.glob(os.path.join(HOME, pattern)))
    return found[-1] if found else None


CHROME = newest(
    "chrome/*/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
) or newest("chrome/*/chrome-linux64/chrome")
FIREFOX = newest("firefox/*/Firefox.app/Contents/MacOS/firefox") or newest(
    "firefox/*/firefox/firefox"
)


def answer_key() -> list[dict]:
    # Cloudflare turns away urllib's own User-Agent.
    request = urllib.request.Request(
        f"{SITE}/js/spa-data/items.json", headers={"User-Agent": "netweir-bench/1"}
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)["items"]


def urls() -> list[str]:
    return [SITE + item["url"] for _ in range(ROUNDS) for item in answer_key()]


# --- the tools ------------------------------------------------------------------
# Each renders the URLs on stdin and prints a JSON line per page, then the
# count.


async def pool(pages: list, render, todo: list[str]) -> list[dict]:
    found: list[dict] = []

    async def worker(page) -> None:
        while todo:
            found.append(await render(page, todo.pop()))

    await asyncio.gather(*map(worker, pages))
    return found


def run_netweir(todo: list[str], engine: str, stealth: bool, pause: bool = True) -> list[dict]:
    import netweir

    async def render(page, url: str) -> dict:
        await page.goto(url)
        await page.wait_for("article.item h2.name")
        root = await page.parse()
        return {
            "url": url,
            "name": root.css("article.item h2.name::text").get(),
            "price": root.css("article.item p.price::text").get(),
        }

    async def main() -> list[dict]:
        executable = CHROME if engine == "chrome" else FIREFOX
        # dialog_pause is also the moment a person takes to look at a tab
        # brought to the front, which every switch between pages costs.
        timing = {} if pause else {"dialog_pause": (0, 0)}
        async with netweir.browser(executable, engine=engine, stealth=stealth, **timing) as browser:
            pages = [await browser.new_page() for _ in range(PAGES_AT_ONCE)]
            return await pool(pages, render, todo)

    return asyncio.run(main())


def run_playwright(todo: list[str], engine: str) -> list[dict]:
    from playwright.async_api import async_playwright

    async def render(page, url: str) -> dict:
        await page.goto(url, wait_until="load")
        item = page.locator("article.item")
        await item.locator("h2.name").wait_for()
        return {
            "url": url,
            "name": await item.locator("h2.name").text_content(),
            "price": await item.locator("p.price").text_content(),
        }

    async def main() -> list[dict]:
        async with async_playwright() as p:
            if engine == "chrome":
                browser = await p.chromium.launch(executable_path=CHROME, headless=True)
            else:
                browser = await p.firefox.launch(headless=True)
            context = await browser.new_context()
            pages = [await context.new_page() for _ in range(PAGES_AT_ONCE)]
            found = await pool(pages, render, todo)
            await browser.close()
            return found

    return asyncio.run(main())


PUPPETEER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "others", "puppeteer")

TOOLS = {
    "netweir-chrome": lambda todo: run_netweir(todo, "chrome", stealth=True),
    "netweir-chrome-nopause": lambda todo: run_netweir(todo, "chrome", stealth=True, pause=False),
    "netweir-firefox": lambda todo: run_netweir(todo, "firefox", stealth=True),
    "netweir-firefox-plain": lambda todo: run_netweir(todo, "firefox", stealth=False),
    "netweir-firefox-nopause": lambda todo: run_netweir(
        todo, "firefox", stealth=False, pause=False
    ),
    "playwright-chrome": lambda todo: run_playwright(todo, "chrome"),
    "playwright-firefox": lambda todo: run_playwright(todo, "firefox"),
}
PROGRAMS = {
    "puppeteer-chrome": lambda: ["node", os.path.join(PUPPETEER, "render.js"), "chrome", CHROME],
    "puppeteer-firefox": lambda: ["node", os.path.join(PUPPETEER, "render.js"), "firefox", FIREFOX],
}


def available(name: str) -> bool:
    if name.startswith("puppeteer"):
        return os.path.isdir(os.path.join(PUPPETEER, "node_modules", "puppeteer"))
    module = "netweir" if name.startswith("netweir") else "playwright"
    try:
        __import__(module)
    except ImportError:
        return False
    return True


# --- running and measuring --------------------------------------------------------


def measure(name: str, todo: list[str], names: dict[str, str]) -> dict:
    import psutil

    if name in PROGRAMS:
        command = [*PROGRAMS[name](), str(PAGES_AT_ONCE)]
    else:
        command = [sys.executable, __file__, "--tool", name]
    started = time.perf_counter()
    proc = subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    proc.stdin.write("\n".join(todo))
    proc.stdin.close()
    root = psutil.Process(proc.pid)
    cpu: dict[int, float] = {}
    peak = 0
    while proc.poll() is None:
        try:
            tree = [root, *root.children(recursive=True)]
        except psutil.NoSuchProcess:
            break
        rss = 0
        for p in tree:
            try:
                with p.oneshot():
                    times = p.cpu_times()
                    cpu[p.pid] = times.user + times.system
                    rss += p.memory_info().rss
            except psutil.Error:
                pass
        peak = max(peak, rss)
        time.sleep(0.2)
    out, err = proc.communicate()
    wall = time.perf_counter() - started
    lines = out.strip().splitlines()
    if proc.returncode != 0 or not lines:
        return {"name": name, "error": f"exit status {proc.returncode}: {err.strip()[-300:]}"}
    found = [json.loads(line) for line in lines[:-1]]
    right = sum(
        1
        for item in found
        if names.get(item["url"].rstrip("/") + "/") == (item["name"] or "").strip()
        and (item["price"] or "").strip()
    )
    return {
        "name": name,
        "pages": len(found),
        "right": right,
        "pages_per_sec": len(found) / wall,
        "cpu_s": sum(cpu.values()),
        "peak_rss_mb": peak / 1e6,
        "wall_s": wall,
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", help=argparse.SUPPRESS)
    ap.add_argument("--only", action="append", choices=[*TOOLS, *PROGRAMS])
    ap.add_argument("--json", action="store_true", help="print the results as JSON")
    args = ap.parse_args()
    if args.tool:
        todo = sys.stdin.read().split()
        found = TOOLS[args.tool](todo)
        for item in found:
            print(json.dumps(item))
        print(len(found))
        return

    names = {SITE + item["url"]: item["name"] for item in answer_key()}
    print(f"  Chrome:  {CHROME}\n  Firefox: {FIREFOX}", flush=True)
    results = []
    for name in args.only or [*TOOLS, *PROGRAMS]:
        if not available(name):
            print(f"  {name:22} not installed, skipped", flush=True)
            continue
        result = measure(name, urls(), names)
        results.append(result)
        if "error" in result:
            print(f"  {name:22} failed: {result['error']}", flush=True)
            continue
        print(
            f"  {name:22} {result['pages_per_sec']:5.1f} pages/s  "
            f"{result['cpu_s']:6.1f} s CPU  {result['peak_rss_mb']:6.0f} MB peak  "
            f"({result['right']}/{result['pages']} right)",
            flush=True,
        )
    if args.json:
        print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
