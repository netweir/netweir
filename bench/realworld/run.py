"""Fetches production pages and parses each with Netweir's parser and with
html5ever, reporting panics, hangs and trees that differ.

    uv run python bench/realworld/run.py                 # the whole list
    uv run python bench/realworld/run.py --sample 300    # as CI runs it
    uv run python bench/realworld/run.py --parse-only    # cached pages only

The pages are fetched politely: robots.txt obeyed (for every redirect
too), one request at a time to a host with a second between them (or the
Crawl-delay, up to 10 s), a User-Agent naming the project and where to
reach us, and a 20 s timeout. Bodies are cached in bench/.cache/realworld/
under the SHA-256 of their URL, with what the server said about them, and
fetched again once they're older than --max-age days. The pages belong to
their sites, so the repository holds only the URL list.

Each page is decoded the way Netweir's spider decodes a response (BOM,
Content-Type, <meta>, then a guess) and parsed by bench/realworld's Rust
worker: once as Netweir parses a page, with a 10 s budget, and once with
the parse errors kept, its tree dumped in html5lib's test format and
compared with html5ever's. A worker still on one page after 90 s is
killed and counted as a hang in whichever parser it was running.

The report goes to bench/.cache/realworld/report.{json,md}, and to the
GitHub job summary when there is one. --check exits 1 if any page made
Netweir's parser panic, crash or hang, or built a tree that differs from
html5ever's. bench/realworld/allowlist.json lists html5ever's known
gaps, each a rewrite of both dumps into a form they share, with the
reason; the rest of a page's tree is still compared.
"""

import argparse
import datetime
import gzip
import hashlib
import json
import os
import re
import select
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import urllib.robotparser
import zlib
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
CACHE = HERE.parent / ".cache" / "realworld"
UA = "netweir-realworld/1.0 (+https://github.com/netweir/netweir)"
TIMEOUT = 20
MAX_BODY = 32 * 1024 * 1024
GAP = 1.0
MAX_DELAY = 10.0
PAGE_LIMIT = 90
HTML_TYPES = ("text/html", "application/xhtml+xml")


def read_urls(path: Path) -> list[tuple[str, str]]:
    out = []
    for line in path.read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            url, _, source = line.partition("\t")
            out.append((url.strip(), source.strip() or "-"))
    return out


def sample(urls: list, n: int, week: int) -> list:
    """Every k-th URL, starting a little further on each ISO week, so the
    sample keeps its spread and covers the whole list over the weeks."""
    if n >= len(urls):
        return urls
    step = len(urls) / n
    offset = week % max(1, int(step))
    return [urls[min(len(urls) - 1, int(i * step) + offset)] for i in range(n)]


def key(url: str) -> str:
    return hashlib.sha256(url.encode()).hexdigest()[:32]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        return None


OPENER = urllib.request.build_opener(NoRedirect)


def to_uri(url: str) -> str:
    """url as HTTP takes it: an IDNA host, and the rest percent-encoded
    where it isn't ASCII, as a browser sends it."""
    parts = urllib.parse.urlsplit(url)
    host = parts.hostname.encode("idna").decode() if parts.hostname else ""
    netloc = host + (f":{parts.port}" if parts.port else "")
    safe = "/:@!$&'()*+,;=%~-._?"
    path, query = (urllib.parse.quote(x, safe=safe) for x in (parts.path, parts.query))
    return urllib.parse.urlunsplit((parts.scheme, netloc, path, query, ""))


def get(url: str, accept: str) -> tuple[int, dict, bytes]:
    """One request, with no redirect followed: (status, headers, body)."""
    request = urllib.request.Request(
        to_uri(url),
        headers={"User-Agent": UA, "Accept": accept, "Accept-Encoding": "gzip, deflate"},
    )
    try:
        response = OPENER.open(request, timeout=TIMEOUT)
    except urllib.error.HTTPError as e:
        response = e
    with response:
        body = response.read(MAX_BODY + 1)
        headers = {k.lower(): v for k, v in response.headers.items()}
        status = response.status if hasattr(response, "status") else response.code
    if len(body) > MAX_BODY:
        raise ValueError(f"body over {MAX_BODY} bytes")
    coding = headers.get("content-encoding", "").lower()
    if coding == "gzip":
        body = gzip.decompress(body)
    elif coding == "deflate":
        try:
            body = zlib.decompress(body)
        except zlib.error:
            body = zlib.decompress(body, -zlib.MAX_WBITS)
    return status, headers, body


class Hosts:
    """robots.txt for each host, and one request at a time to each."""

    def __init__(self):
        self.lock = threading.Lock()
        self.locks: dict[str, threading.Lock] = defaultdict(threading.Lock)
        self.robots: dict[str, urllib.robotparser.RobotFileParser] = {}
        self.last: dict[str, float] = {}

    def request(self, url: str, accept: str):
        host = urllib.parse.urlsplit(url).netloc.lower()
        with self.lock:
            host_lock = self.locks[host]
        with host_lock:
            rules = self.robots.get(host)
            if rules is None:
                rules = self.robots[host] = self._robots(url)
            if unreachable := getattr(rules, "unreachable", None):
                why = f"robots.txt: {type(unreachable).__name__}: {unreachable}"
                raise ConnectionError(why) from unreachable
            if not rules.can_fetch(UA, url):
                return None
            delay = min(MAX_DELAY, float(rules.crawl_delay(UA) or GAP))
            wait = self.last.get(host, 0) + delay - time.monotonic()
            if wait > 0:
                time.sleep(wait)
            try:
                return get(url, accept)
            finally:
                self.last[host] = time.monotonic()

    def _robots(self, url: str) -> urllib.robotparser.RobotFileParser:
        """As RFC 9309 has it: a 4xx allows everything, and no answer or a
        5xx allows nothing."""
        parts = urllib.parse.urlsplit(url)
        rules = urllib.robotparser.RobotFileParser()
        location = f"{parts.scheme}://{parts.netloc}/robots.txt"
        try:
            for _ in range(5):
                status, headers, body = get(location, "text/plain")
                if status in (301, 302, 303, 307, 308) and "location" in headers:
                    location = urllib.parse.urljoin(location, headers["location"])
                    continue
                break
        except Exception as e:
            rules.unreachable = e
            return rules
        self.last[parts.netloc.lower()] = time.monotonic()
        if status >= 500 or status in (429,):
            rules.disallow_all = True
        elif status >= 400 or status in (301, 302, 303, 307, 308):
            rules.allow_all = True
        else:
            rules.parse(body.decode("utf-8", "replace").splitlines())
        return rules


def no_such_host(e: BaseException | None) -> bool:
    while e is not None:
        if isinstance(e, socket.gaierror):
            return True
        e = getattr(e, "reason", None) if isinstance(e, urllib.error.URLError) else e.__cause__
    return False


def follow(url: str, hosts: Hosts):
    """The page at the end of url's redirects, as (url, status, headers,
    body), or why it wasn't fetched."""
    location = url
    for _ in range(6):
        got = hosts.request(location, "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5")
        if got is None:
            return f"robots.txt disallows {location}"
        status, headers, body = got
        if status in (301, 302, 303, 307, 308) and "location" in headers:
            location = urllib.parse.urljoin(location, headers["location"])
            continue
        return location, status, headers, body
    return "more than 5 redirects"


def fetch(url: str, hosts: Hosts) -> dict:
    meta = {
        "url": url,
        "fetched": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
    }
    try:
        try:
            got = follow(url, hosts)
        except Exception as e:
            # Tranco lists domains, and some answer only at www.
            parts = urllib.parse.urlsplit(url)
            if not no_such_host(e) or parts.netloc.startswith("www.") or parts.path != "/":
                raise
            got = follow(f"{parts.scheme}://www.{parts.netloc}/", hosts)
    except Exception as e:
        meta["error"] = f"{type(e).__name__}: {e}"[:300]
        return meta
    if isinstance(got, str):
        meta["error"] = got
        meta["robots"] = got.startswith("robots.txt")
        return meta
    location, status, headers, body = got
    meta.update(final_url=location, status=status, content_type=headers.get("content-type"))
    if status == 200:
        meta["sha256"] = hashlib.sha256(body).hexdigest()
        meta["bytes"] = len(body)
        (CACHE / f"{key(url)}.html").write_bytes(body)
    return meta


def cached(url: str, max_age: float) -> dict | None:
    path = CACHE / f"{key(url)}.json"
    if not path.exists():
        return None
    if is_old(path, max_age):
        return None
    meta = json.loads(path.read_text())
    if meta.get("sha256") and not (CACHE / f"{key(url)}.html").exists():
        return None
    return meta


def prune(max_age: float):
    """Drops pages older than max_age, which would be fetched again
    anyway, so the cache (and CI's copy of it) holds about a week."""
    for meta in CACHE.glob("*.json"):
        if meta.name != "report.json" and is_old(meta, max_age):
            meta.unlink()
            meta.with_suffix(".html").unlink(missing_ok=True)


def is_old(path: Path, max_age: float) -> bool:
    fetched = datetime.datetime.fromisoformat(json.loads(path.read_text())["fetched"])
    return (datetime.datetime.now(datetime.UTC) - fetched).total_seconds() > max_age * 86400


def fetch_all(urls: list, hosts_at_once: int, max_age: float, minutes: float) -> dict[str, dict]:
    metas = {}
    by_host = defaultdict(list)
    for url, _ in urls:
        meta = cached(url, max_age)
        if meta is not None:
            metas[url] = meta
        else:
            by_host[urllib.parse.urlsplit(url).netloc.lower()].append(url)
    if not by_host:
        return metas
    hosts = Hosts()
    stop = time.monotonic() + minutes * 60
    done = [0]
    total = sum(len(v) for v in by_host.values())

    def one_host(urls):
        for url in urls:
            if time.monotonic() > stop:
                return
            meta = fetch(url, hosts)
            (CACHE / f"{key(url)}.json").write_text(json.dumps(meta))
            metas[url] = meta
            done[0] += 1
            if done[0] % 100 == 0:
                print(f"  fetched {done[0]}/{total}", file=sys.stderr)

    with ThreadPoolExecutor(hosts_at_once) as pool:
        list(pool.map(one_host, by_host.values()))
    return metas


class Worker:
    """The Rust worker, restarted after a page that kills or hangs it."""

    def __init__(self, binary: Path):
        self.binary = binary
        self.proc = None

    def run(self, path: Path, content_type: str | None) -> dict:
        if self.proc is None or self.proc.poll() is not None:
            self.proc = subprocess.Popen(
                [self.binary, HERE / "allowlist.json"],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            self.buf = b""
        ct = (content_type or "-").replace("\t", " ").replace("\n", " ")
        stage = "start"
        try:
            self.proc.stdin.write(f"{path}\t{ct}\n".encode())
            self.proc.stdin.flush()
        except BrokenPipeError:
            pass  # It died before reading the page; the read below says how.
        deadline = time.monotonic() + PAGE_LIMIT
        while True:
            while b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                msg = json.loads(line)
                if "stage" in msg:
                    stage = msg["stage"]
                else:
                    return msg["result"]
            left = deadline - time.monotonic()
            ready = select.select([self.proc.stdout], [], [], max(0, left))[0] if left > 0 else []
            if not ready:
                self.proc.kill()
                self.proc.wait()
                detail = f"still running after {PAGE_LIMIT} s"
                return {"failure": {"kind": "hang", "parser": parser_of(stage), "detail": detail}}
            chunk = os.read(self.proc.stdout.fileno(), 1 << 16)
            if not chunk:
                code = self.proc.wait()
                err = self.proc.stderr.read().decode("utf-8", "replace")[-2000:]
                detail = f"worker exited with {code}: {err.strip()}"
                return {"failure": {"kind": "crash", "parser": parser_of(stage), "detail": detail}}
            self.buf += chunk

    def close(self):
        if self.proc and self.proc.poll() is None:
            self.proc.stdin.close()
            self.proc.wait()


def parser_of(stage: str) -> str:
    return "html5ever" if stage == "html5ever" else "netweir"


def build() -> Path:
    target = Path(os.environ.get("CARGO_TARGET_DIR", HERE / "target"))
    subprocess.run(
        ["cargo", "build", "--release", "--quiet", "--manifest-path", HERE / "Cargo.toml"],
        check=True,
        env={**os.environ, "CARGO_TARGET_DIR": str(target)},
    )
    return target / "release" / "bench-realworld"


def parse_all(urls: list, metas: dict, workers: int) -> list[dict]:
    binary = build()
    pages = []
    for url, source in urls:
        meta = metas.get(url)
        if meta is None:
            continue
        rec = {"url": url, "source": source, **meta}
        ct = (meta.get("content_type") or "text/html").split(";")[0].strip().lower()
        if meta.get("status") == 200 and ct not in HTML_TYPES:
            rec["skipped"] = f"not HTML: {ct}"
        pages.append(rec)
    todo = [p for p in pages if p.get("sha256") and "skipped" not in p]
    lock = threading.Lock()
    queue = list(reversed(todo))

    def drain():
        worker = Worker(binary)
        while True:
            with lock:
                if not queue:
                    break
                page = queue.pop()
            page.update(worker.run(CACHE / f"{key(page['url'])}.html", page.get("content_type")))
        worker.close()

    threads = [threading.Thread(target=drain) for _ in range(workers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return pages


def size_bucket(n: int) -> str:
    for limit, name in [
        (10_000, "under 10 KB"),
        (100_000, "10-100 KB"),
        (1_000_000, "100 KB-1 MB"),
    ]:
        if n < limit:
            return name
    return "1-10 MB" if n < 10_000_000 else "over 10 MB"


def failed(p: dict) -> bool:
    return "failure" in p or "difference" in p


# parsec-reduce's --check names; a hang is a parse past src/main.rs's BUDGET.
REDUCE = {"panic": "panic", "crash": "panic", "hang": "hang:10000"}


def reduce_command(p: dict) -> str:
    check = REDUCE.get(p.get("failure", {}).get("kind"), "differs-from-html5ever")
    page = CACHE / (key(p["url"]) + ".html")
    return f"cargo run --release -p parsec-reduce -- {page} --check {check}"


def report(pages: list, listed: int, started: str) -> tuple[dict, str]:
    parsed = [p for p in pages if "netweir_ms" in p or "failure" in p]
    fetch_errors = [p for p in pages if "error" in p and not p.get("robots")]
    kinds = Counter(p["failure"]["kind"] for p in parsed if "failure" in p)
    differs = [p for p in parsed if "difference" in p]
    allowed = Counter(name for p in parsed for name in p.get("allowed", []))
    headline = {
        "started": started,
        "listed": listed,
        "robots_disallowed": sum(1 for p in pages if p.get("robots")),
        "fetch_errors": len(fetch_errors),
        "not_200": sum(1 for p in pages if p.get("status") not in (None, 200)),
        "not_html": sum(1 for p in pages if "skipped" in p),
        "parsed": len(parsed),
        "bytes": sum(p.get("bytes", 0) for p in parsed),
        "panics": kinds["panic"],
        "crashes": kinds["crash"],
        "hangs": kinds["hang"],
        "trees_differ": len(differs),
        "trees_differ_allowlisted": dict(allowed),
        "html5ever_panics": sum(1 for p in parsed if "html5ever_panic" in p),
        "parse_errors": sum(p.get("errors", 0) for p in parsed),
    }

    def table(title, by):
        groups = defaultdict(list)
        for p in parsed:
            groups[by(p)].append(p)
        rows = sorted(groups.items(), key=lambda kv: -len(kv[1]))
        lines = [
            f"### {title}",
            "",
            "| | Pages | Failures | Parse errors (median) |",
            "|---|--:|--:|--:|",
        ]
        for name, ps in rows[:30]:
            errs = sorted(p.get("errors", 0) for p in ps)
            lines.append(
                f"| {name} | {len(ps)} | {sum(map(failed, ps))} | {errs[len(errs) // 2]} |"
            )
        if len(rows) > 30:
            lines.append(f"| {len(rows) - 30} more | {sum(len(ps) for _, ps in rows[30:])} | | |")
        return lines + [""]

    def lang(p):
        return (p.get("lang") or "none").split("-")[0].split("_")[0].lower().strip() or "none"

    def source(p):
        return p["source"].split(":")[0] if p["source"].startswith("tranco") else p["source"]

    h = headline
    md = [
        "## Netweir's parser on the real web",
        "",
        f"{h['listed']} URLs listed; {h['parsed']} pages parsed ({h['bytes'] / 1e6:.0f} MB), "
        f"{h['robots_disallowed']} left alone for robots.txt, {h['fetch_errors']} unreachable, "
        f"{h['not_200']} not 200 and {h['not_html']} not HTML.",
        "",
        "| Panics | Crashes | Hangs | Trees differ from html5ever | Allowlisted | html5ever panics |",
        "|--:|--:|--:|--:|--:|--:|",
        f"| {h['panics']} | {h['crashes']} | {h['hangs']} | {h['trees_differ']} | "
        f"{sum(1 for p in parsed if p.get('allowed'))} | {h['html5ever_panics']} |",
        "",
    ]
    if allowed:
        md += ["Pages that differ only as bench/realworld/allowlist.json expects:", ""]
        md += [f"- `{name}`: {n}" for name, n in allowed.most_common()] + [""]
    md += table("By encoding", lambda p: p.get("encoding", "?"))
    md += table("By language (`<html lang>`)", lang)
    md += table("By size", lambda p: size_bucket(p.get("bytes", 0)))
    md += table("By source", source)
    timed = sorted((p for p in parsed if "netweir_ms" in p), key=lambda p: -p["netweir_ms"])
    md += [
        "### Slowest pages",
        "",
        "| Page | Size | Netweir | html5ever |",
        "|---|--:|--:|--:|",
    ]
    for p in timed[:20]:
        theirs = f"{p['html5ever_ms']:.1f} ms" if "html5ever_ms" in p else "-"
        md.append(
            f"| {p['url']} | {p['bytes'] / 1e3:.0f} KB | {p['netweir_ms']:.1f} ms | {theirs} |"
        )
    md.append("")
    bad = [p for p in parsed if failed(p)]
    md += ["### Failures", ""]
    if bad:
        md += [
            "Each one shrinks to a fixture with Parsec's reducer, parsec-html/parsec#203, "
            "once it lands; the command is under each failure.",
            "",
        ]
    else:
        md += ["None.", ""]
    for p in bad:
        if "failure" in p:
            f = p["failure"]
            md += [
                f"#### {f['kind']} in {f.get('parser', '?')}: {p['url']}",
                "",
                f"```\n{f['detail']}\n```",
            ]
        else:
            d = p["difference"]
            md += [
                f"#### Tree differs from html5ever's: {p['url']}",
                "",
                f"First at line {d['line']} of the dump.",
                "",
                f"Netweir:\n```\n{d['netweir']}\n```",
                f"html5ever:\n```\n{d['html5ever']}\n```",
            ]
        md += [
            f"SHA-256 `{p['sha256']}`, encoding {p.get('encoding')}.",
            "",
            f"`{reduce_command(p)}`",
            "",
        ]
    theirs = [p for p in parsed if "html5ever_panic" in p]
    if theirs:
        md += ["### html5ever panics", "", "Not Netweir's, but its tree couldn't be compared:", ""]
        md += [f"- {p['url']}: `{p['html5ever_panic']}`" for p in theirs] + [""]
    if fetch_errors:
        reasons = Counter(re.sub(r"[\d.]+", "N", p["error"].split(":")[0]) for p in fetch_errors)
        md += ["### Fetch errors", ""] + [f"- {r}: {n}" for r, n in reasons.most_common(10)] + [""]
    data = {"headline": headline, "pages": pages}
    return data, "\n".join(md)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--urls", type=Path, default=HERE / "urls.txt")
    ap.add_argument("--sample", type=int, help="this many URLs, spread over the list")
    ap.add_argument("--hosts", type=int, default=16, help="hosts fetched from at once")
    ap.add_argument("--workers", type=int, default=max(1, (os.cpu_count() or 2) // 2))
    ap.add_argument(
        "--max-age", type=float, default=7, help="days before a cached page is fetched again"
    )
    ap.add_argument(
        "--minutes", type=float, default=60, help="stop starting new fetches after this"
    )
    ap.add_argument("--parse-only", action="store_true", help="no fetching, cached pages only")
    ap.add_argument("--check", action="store_true", help="exit 1 on any failure")
    args = ap.parse_args()
    started = datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds")
    CACHE.mkdir(parents=True, exist_ok=True)
    urls = read_urls(args.urls)
    if args.sample:
        urls = sample(urls, args.sample, datetime.date.today().isocalendar().week)
    if args.parse_only:
        metas = {u: m for u, _ in urls if (m := cached(u, float("inf")))}
    else:
        prune(args.max_age)
        print(f"fetching {len(urls)} URLs", file=sys.stderr)
        metas = fetch_all(urls, args.hosts, args.max_age, args.minutes)
    print(f"parsing {sum(1 for m in metas.values() if m.get('sha256'))} pages", file=sys.stderr)
    pages = parse_all(urls, metas, args.workers)
    data, md = report(pages, len(urls), started)
    (CACHE / "report.json").write_text(json.dumps(data, indent=1))
    (CACHE / "report.md").write_text(md)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write(md + "\n")
    h = data["headline"]
    print(md.split("### By encoding")[0])
    print(f"report: {CACHE / 'report.md'}", file=sys.stderr)
    bad = h["panics"] + h["crashes"] + h["hangs"] + h["trees_differ"]
    if args.check and bad:
        sys.exit(f"{bad} pages failed")


if __name__ == "__main__":
    main()
