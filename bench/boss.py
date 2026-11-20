"""The final boss: 1,000 pages as messy as a crawl brings back, parsed as
bytes by each library in a process of its own, timing every page and
taking the process's peak memory.

    uv run --group bench python bench/boss.py --parsec ../parsec
    uv run --group bench python bench/boss.py --smoke --check   # CI
    uv run --group bench python bench/boss.py --answer-key FILE

--smoke is 300 pages with the giants cut to 2 MB, every library but
html5lib, once and without waiting for a quiet machine; --check fails if
Netweir crashes, hangs or raises on a page, leaves one without a
document, or is more than --tolerance slower than another library over
the batch. --answer-key writes test.netweir.com's final boss answer key
(each page's encoding, title and element count), checked against Parsec
and html5lib.

The pages are Parsec's final boss batch, byte for byte: seed 20261119,
900 tiny broken fragments, 46 real pages as served, 44 real pages
re-encoded (Shift_JIS, GBK, EUC-KR, windows-1251, UTF-16 with a BOM, a
<meta> that lies, a charset only the HTTP header gives, UTF-8 with invalid
bytes) and 10 giants of 10 to 30 MB. Parsec's bench builds them
(`parsec-html-bench boss-page N`), so the first run needs a Parsec
checkout (https://github.com/parsec-html/parsec, `--parsec DIR`, with
`bench/fetch-pages.sh` run in it for the real pages) and writes them to
bench/.cache/boss/. The fingerprint of every byte in order must come out
de2480edafa2baa9, as in Parsec's own run, or nothing is timed. Without
Parsec's bench/corpus the real pages are generated shop listings and the
fingerprint is different, which --smoke allows.

Each library gets the bytes as its users have them, and the HTTP charset
for the one quirk that has one:

- netweir: the Page a spider gets for the response, read through
  `page.root`. Netweir has no public call that takes bytes, so this builds
  the response the way `netweir parse FILE` does. It finds the encoding
  as a browser does: BOM, Content-Type, <meta>, then a guess.
- selectolax: `LexborHTMLParser(body, encoding=True)`, its own BOM and
  <meta> detection. Undeclared bytes are UTF-8 to it, and it has nowhere
  to put an HTTP charset. Its default, `encoding=False`, reads every
  page as UTF-8.
- lxml: `lxml.html.fromstring(body)`, libxml2's own detection, with an
  HTMLParser(encoding=...) for the HTTP charset.
- beautifulsoup (lxml) and (html.parser): `BeautifulSoup(body, parser,
  from_encoding=...)`, which runs UnicodeDammit first.
- parsel: as Scrapy feeds it, `w3lib.encoding.html_to_unicode` on the
  Content-Type and body, then `Selector(text=...)`.
- html5lib: `html5lib.parse(body, transport_encoding=...)`.

A page's time is from bytes to a built document. Counting its elements,
freeing it and a gc.collect() (Beautiful Soup's tree is full of cycles)
come after, untimed. A page still parsing after 30 seconds is a timeout:
the worker is killed and a new one carries on from the next page, as
after a crash. Peak memory is getrusage(RUSAGE_CHILDREN)'s ru_maxrss in a
process whose only children are that library's workers; "none" is a
worker that reads the pages and parses nothing. Before each library's
run the bench waits for the one-minute load average to drop below 3.
Figures are medians of the runs. The element count, in tree order
without template contents, is compared with the HTML Standard's tree
as Parsec builds it.
"""

import argparse
import contextlib
import gc
import hashlib
import json
import os
import re
import resource
import select
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

SEED = 20261119
PAGES = 1000
FINGERPRINT = "de2480edafa2baa9"
# The Parsec commit CI builds the batch with, and the answer key names.
PARSEC_SHA = "4d6fe823b25974f3aab19750060231b97df06445"
LIMIT = 30.0
QUIET = 3.0
MAX_WAIT = 2 * 60 * 60
SMOKE_PAGES = 300
SMOKE_GIANT = 2_000_000

CATEGORIES = ["fragments", "real pages", "encodings", "giants"]
QUIRKS = [
    "Shift_JIS",
    "windows-1251",
    "GBK",
    "EUC-KR",
    "UTF-16LE, BOM",
    "UTF-16BE, BOM",
    "meta says UTF-8, bytes windows-1251",
    "UTF-8 BOM, meta says windows-1251",
    "EUC-KR from the transport, meta says ISO-8859-1",
    "UTF-8, undeclared",
    "UTF-8 with invalid bytes",
]
GIANTS = ["real pages run together", "one huge table", "one text node of references"]
HERE = Path(__file__).resolve().parent
M64 = (1 << 64) - 1


def mix(seed, i):
    """Parsec's splitmix64 of the seed and a page number."""
    z = (seed ^ (i * 0x9E3779B97F4A7C15)) & M64
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
    return (z ^ (z >> 31)) | 1


def plan(pages, seed):
    """Each page's category, in batch order, as Parsec's `Batch::plan`
    shuffles them, so the bench knows which page is which kind and which
    one has an HTTP charset."""
    giants = (pages + 50) // 100
    encodings = (pages * 44 + 500) // 1000
    real = (pages * 46 + 500) // 1000
    out = [0] * (pages - giants - encodings - real) + [1] * real + [2] * encodings + [3] * giants
    x = mix(seed, M64)
    for i in range(pages - 1, 0, -1):
        x ^= (x << 13) & M64
        x ^= x >> 7
        x ^= (x << 17) & M64
        j = x % (i + 1)
        out[i], out[j] = out[j], out[i]
    return out


def describe(pages, seed):
    """Category, kind and HTTP charset of every page."""
    seen = [0, 0, 0, 0]
    out = []
    for c in plan(pages, seed):
        nth = seen[c]
        seen[c] += 1
        kind = {0: "fragment", 1: "real page", 2: QUIRKS[nth % 11], 3: GIANTS[nth % 3]}[c]
        transport = "euc-kr" if c == 2 and nth % 11 == 8 else None
        out.append({"category": CATEGORIES[c], "kind": kind, "transport": transport})
    return out


def fnv(data, h=0xCBF29CE484222325):
    """FNV-1a over `data`, Parsec's fingerprint of the batch."""
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & M64
    return h


def parsec_bench(parsec):
    """Builds Parsec's bench and returns the binary."""
    manifest = Path(parsec) / "bench" / "Cargo.toml"
    if not manifest.exists():
        sys.exit(
            f"no Parsec at {parsec}: git clone --recurse-submodules"
            " https://github.com/parsec-html/parsec, and pass --parsec"
        )
    subprocess.run(["cargo", "build", "--release", "-q", "--manifest-path", manifest], check=True)
    return manifest.parent / "target" / "release" / "parsec-html-bench"


def parsec_elements(binary, folder, pages):
    """Each page's element count from Parsec's boss worker: the HTML
    Standard's tree, from the bytes and the HTTP charset."""
    worker = subprocess.Popen(
        [binary, "boss-worker", "0"], stdin=subprocess.PIPE, stdout=subprocess.PIPE
    )
    counts = []
    for i, page in enumerate(pages):
        body = (folder / f"{i:04}.html").read_bytes()
        worker.stdin.write(f"{i} {len(body)} {page['transport'] or '-'}\n".encode() + body)
        worker.stdin.flush()
        counts.append(int(worker.stdout.readline().split()[2]))
    worker.stdin.close()
    worker.wait()
    return counts


def build_corpus(folder, pages, seed, parsec, giant_cap):
    """Writes the batch to `folder`, a file a page and pages.json, and
    returns the fingerprint."""
    binary = parsec_bench(parsec)
    folder.mkdir(parents=True, exist_ok=True)
    about = describe(pages, seed)

    def one(i):
        body = subprocess.run(
            [binary, "boss-page", str(i), str(pages), str(seed)], capture_output=True, check=True
        ).stdout
        if giant_cap and about[i]["category"] == "giants":
            # shortcut: cut, where Parsec's --smoke builds a smaller giant;
            # both stay broken HTML, which is all the smoke run checks.
            body = body[:giant_cap]
        (folder / f"{i:04}.html").write_bytes(body)
        return body

    print(f"Building {pages} pages with {binary}", flush=True)
    h = 0xCBF29CE484222325
    with ThreadPoolExecutor(os.cpu_count()) as pool:
        for i, body in enumerate(pool.map(one, range(pages))):
            about[i]["bytes"] = len(body)
            about[i]["sha256"] = hashlib.sha256(body).hexdigest()
            # Parsec's count has a declarative shadow root as its <template>
            # element; Netweir, as a browser does, keeps it out of the tree.
            about[i]["shadow"] = any(
                "shadowrootmode".encode(e) in body.lower()
                for e in ("utf-8", "utf-16-le", "utf-16-be")
            )
            h = fnv(body, h)
            # The plan above must be Parsec's: a giant is 10 MB or more.
            assert (len(body) >= 10_000_000) == (about[i]["category"] == "giants" and not giant_cap)
    for page, n in zip(about, parsec_elements(binary, folder, about), strict=True):
        page["elements"] = n
    fingerprint = f"{h:016x}"
    meta = {"pages": pages, "seed": seed, "giant_cap": giant_cap, "fingerprint": fingerprint}
    (folder / "pages.json").write_text(json.dumps({**meta, "batch": about}, indent=0))
    return fingerprint


def load_corpus(folder):
    meta = json.loads((folder / "pages.json").read_text())
    for i, page in enumerate(meta["batch"]):
        body = (folder / f"{i:04}.html").read_bytes()
        if hashlib.sha256(body).hexdigest() != page["sha256"]:
            sys.exit(f"{folder}/{i:04}.html has changed since it was built; delete {folder}")
    return meta


def tool(name):
    """`(parse, count)` for a library: parse turns bytes and the HTTP
    charset into a document, count gives its elements."""
    if name == "netweir":
        from netweir import Page
        from netweir._native import _response

        def parse(body, transport):
            ctype = f"text/html; charset={transport}" if transport else "text/html"
            headers = [("content-type", ctype)]
            return Page(_response("https://boss.test/", 200, "HTTP/1.1", headers, body)).root

        # Without template contents, which Parsec's count leaves out too.
        query = "count(//*[not(ancestor::template)])"
        return parse, lambda root: int(float(root.xpath(query).get()))
    if name == "selectolax":
        from selectolax.lexbor import LexborHTMLParser

        def count(tree):
            if tree.root is None:
                return 0
            return sum(1 for n in tree.root.traverse() if n.is_element_node)

        return lambda body, transport: LexborHTMLParser(body, encoding=True), count
    if name in ("lxml", "parsel"):
        from lxml import etree
        from lxml import html as lxml_html

        def count_lxml(tree):
            return sum(1 for _ in tree.getroottree().getroot().iter(etree.Element))

        if name == "lxml":

            def parse(body, transport):
                parser = lxml_html.HTMLParser(encoding=transport) if transport else None
                return lxml_html.fromstring(body, parser=parser)

            return parse, count_lxml
        import parsel
        from w3lib.encoding import html_to_unicode

        def parse(body, transport):
            ctype = f"text/html; charset={transport}" if transport else "text/html"
            return parsel.Selector(text=html_to_unicode(ctype, body)[1])

        return parse, lambda sel: count_lxml(sel.root)
    if name.startswith("beautifulsoup"):
        from bs4 import BeautifulSoup

        parser = name.split()[1].strip("()")
        return (
            lambda body, transport: BeautifulSoup(body, parser, from_encoding=transport),
            lambda soup: len(soup.find_all(True)),
        )
    if name == "html5lib":
        import html5lib

        return (
            lambda body, transport: html5lib.parse(body, transport_encoding=transport),
            lambda doc: sum(1 for e in doc.iter() if isinstance(e.tag, str)),
        )
    assert name == "none"
    return lambda body, transport: body, lambda body: 0


TOOLS = [
    "netweir",
    "selectolax",
    "lxml",
    "beautifulsoup (lxml)",
    "beautifulsoup (html.parser)",
    "parsel",
    "html5lib",
    "none",
]


def worker(name, folder, start):
    """Parses the pages from `start` on, printing `<page> <seconds>
    <elements>`, or `<page> <seconds> ! <exception>` when it raised."""
    parse, count = tool(name)
    batch = json.loads((folder / "pages.json").read_text())["batch"]
    for i in range(start, len(batch)):
        body = (folder / f"{i:04}.html").read_bytes()
        t = time.perf_counter()
        try:
            doc = parse(body, batch[i]["transport"])
            secs = time.perf_counter() - t
            line = f"{i} {secs} {count(doc)}"
        except Exception as e:
            secs = time.perf_counter() - t
            line = f"{i} {secs} ! {type(e).__name__}"
        doc = body = None
        gc.collect()
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def peak_rss():
    """The largest resident set of any child waited for, in bytes: macOS
    gives ru_maxrss in bytes, Linux in kilobytes."""
    rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    return rss if sys.platform == "darwin" else rss * 1024


def runner(name, folder):
    """Runs workers for one library over the batch, a new one after a
    crash or a timeout, and prints the outcome of every page and the
    workers' peak memory as JSON. Its only children are those workers."""
    total = len(json.loads((folder / "pages.json").read_text())["batch"])
    pages = [None] * total
    nxt = 0
    while nxt < total:
        child = subprocess.Popen(
            [sys.executable, __file__, "--worker", name, "--cache", folder, "--from", str(nxt)],
            stdout=subprocess.PIPE,
            text=True,
        )
        while True:
            ready, _, _ = select.select([child.stdout], [], [], LIMIT)
            if not ready:
                child.kill()
                child.wait()
                pages[nxt] = ["timeout", LIMIT, None]
                nxt += 1
                break
            line = child.stdout.readline()
            if not line:
                if child.wait() != 0 and nxt < total:
                    pages[nxt] = ["crash", 0.0, f"exit {child.returncode}"]
                    nxt += 1
                break
            i, secs, rest = line.split(" ", 2)
            rest = rest.strip()
            if rest.startswith("!"):
                pages[int(i)] = ["exception", float(secs), rest[2:]]
            else:
                pages[int(i)] = ["parsed", float(secs), int(rest)]
            nxt = int(i) + 1
    print(json.dumps({"pages": pages, "rss": peak_rss()}))


XHTML = "{http://www.w3.org/1999/xhtml}"


def collapse(text):
    """`text` with ASCII whitespace stripped and collapsed, as
    document.title has it."""
    return re.sub(r"[\t\n\f\r ]+", " ", text).strip(" ")


def html5lib_view(body, transport):
    """html5lib's encoding, element count (template contents left out,
    shadow-root templates in) and title, to check Netweir's against."""
    import html5lib

    parser = html5lib.HTMLParser()
    doc = parser.parse(body, transport_encoding=transport)
    elements, title, stack = 0, None, [doc]
    while stack:
        el = stack.pop()
        if not isinstance(el.tag, str):
            continue
        elements += 1
        if title is None and el.tag == XHTML + "title":
            title = (el.text or "") + "".join(c.tail or "" for c in el)
        if el.tag != XHTML + "template":
            stack.extend(reversed(el))
    return parser.documentEncoding, elements, collapse(title or "")


# The elements whose content is HTML again inside SVG or MathML.
INTEGRATION = {"foreignobject", "desc", "title", "annotation-xml", "mi", "mo", "mn", "ms", "mtext"}


def document_title(root):
    """document.title from a Netweir document. Its XPath has no
    namespaces, so a title is HTML unless its nearest <svg> or <math> is
    nearer than the nearest point where content turns back into HTML."""
    for title in root.xpath("//title[not(ancestor::template)]"):
        for up in reversed(title.xpath("ancestor::*")):
            name = up.xpath("name(.)").get()
            if name in INTEGRATION or name in ("svg", "math"):
                break
        else:
            name = None
        if name not in ("svg", "math"):
            return collapse("".join(title.xpath("text()").getall()))
    return ""


def answer_key(folder, meta, path):
    """Writes test.netweir.com's answer key for the batch: for every page,
    Netweir's encoding, title and element count, with Parsec's count and
    html5lib's three to check them by."""
    parse, count = tool("netweir")
    from netweir import Page
    from netweir._native import _response

    records, differ = [], []
    for i, b in enumerate(meta["batch"]):
        body = (folder / f"{i:04}.html").read_bytes()
        ctype = f"text/html; charset={b['transport']}" if b["transport"] else "text/html"
        page = Page(
            _response("https://boss.test/", 200, "HTTP/1.1", [("content-type", ctype)], body)
        )
        elements = count(page.root)
        title = document_title(page.root)
        shadow = b["elements"] - elements
        assert shadow == 0 or (b["shadow"] and shadow > 0), f"page {i}: {shadow} shadow roots"
        if b["category"] == "giants":
            # html5lib grows a text node by concatenation, so a 13 MB one
            # of references takes it hours.
            checks = []
        else:
            enc, h_elements, h_title = html5lib_view(body, b["transport"])
            checks = [("elements", b["elements"], h_elements), ("title", title, h_title)]
            # For ASCII, UTF-8 and windows-1252 give the same text.
            if not body.isascii():
                checks.append(("encoding", page.encoding.lower(), (enc or "").lower()))
        for what, mine, theirs in checks:
            if mine != theirs:
                differ.append(
                    f"page {i} ({b['category']}, {b['kind']}): {what} {mine!r}, html5lib {theirs!r}"
                )
        records.append(
            {
                "page": i,
                "file": f"{i:04}.html",
                "category": b["category"],
                "kind": b["kind"],
                "bytes": b["bytes"],
                "sha256": b["sha256"],
                "http_charset": b["transport"],
                "encoding": page.encoding,
                "title": title,
                "elements": elements,
                "shadow_roots": shadow,
            }
        )
        print(f"\r  {i + 1} pages", end="", flush=True)
    print()
    for d in differ:
        print(d)
    key = {
        "pages": meta["pages"],
        "seed": meta["seed"],
        "fingerprint": meta["fingerprint"],
        "parsec": PARSEC_SHA,
        "checked": (
            "elements + shadow_roots is Parsec's count on every page. Against html5lib 1.1"
            f" on the {sum(b['category'] != 'giants' for b in meta['batch'])} pages that"
            f" aren't giants: {len(differ)} differences in encoding, element count or title"
        ),
        "batch": records,
    }
    Path(path).write_text(json.dumps(key, ensure_ascii=False, indent=1) + "\n")
    return differ


def wait_for_quiet(deadline):
    while True:
        load = os.getloadavg()[0]
        if load < QUIET or time.time() >= deadline:
            return load
        time.sleep(20)


def median(values):
    values = sorted(values)
    return values[len(values) // 2]


def versions():
    import platform
    from importlib.metadata import PackageNotFoundError, version

    out = []
    for pkg in ["netweir", "selectolax", "lxml", "beautifulsoup4", "parsel", "w3lib", "html5lib"]:
        with contextlib.suppress(PackageNotFoundError):
            out.append(f"{pkg} {version(pkg)}")

    return (
        f"{', '.join(out)}; Python {platform.python_version()},"
        f" {platform.system()} {platform.release()} {platform.machine()}, {os.cpu_count()} CPUs"
    )


def report(meta, tools, results, loads):
    batch = meta["batch"]
    n = len(batch)
    secs = lambda p: p[1]  # noqa: E731
    print(
        f"\n{'median':28} {'s/1,000 pages':>13} {'slowest':>9} {'peak RSS':>9}"
        f" {'crashes':>7} {'timeouts':>8} {'raised':>6} {'empty':>5} {'tree differs':>12}"
    )
    summary = {}
    for name in tools:
        runs = results[name]
        total = median([sum(map(secs, r["pages"])) for r in runs])
        slowest = median([max(map(secs, r["pages"])) for r in runs])
        rss = median([r["rss"] for r in runs]) / 1e6

        def worst(f, runs=runs):
            return max(sum(1 for p in r["pages"] if f(p)) for r in runs)

        crashes = worst(lambda p: p[0] == "crash")
        timeouts = worst(lambda p: p[0] == "timeout")
        raised = worst(lambda p: p[0] == "exception")
        empty = worst(lambda p: p[0] == "parsed" and p[2] == 0)
        differs = sum(
            1
            for p, b in zip(runs[0]["pages"], batch, strict=True)
            if p[0] == "parsed" and p[2] != b["elements"]
        )
        summary[name] = dict(total=total * 1000 / n, crashes=crashes + timeouts + raised + empty)
        if name == "none":
            print(f"{name:28} {'-':>13} {'-':>9} {rss:6.0f} MB")
            continue
        print(
            f"{name:28} {total * 1000 / n:13.2f} {slowest:7.3f} s {rss:6.0f} MB"
            f" {crashes:7} {timeouts:8} {raised:6} {empty:5} {differs:12}"
        )

    shown = [t for t in tools if t != "none"]
    print("\nPer category, median seconds for its pages (slowest page in brackets)")
    print(f"{'':30} {'pages':>5} {'MB':>6}" + "".join(f" {t[:20]:>20}" for t in shown))
    groups = [(c, [i for i, b in enumerate(batch) if b["category"] == c]) for c in CATEGORIES]
    groups += [
        (f"  {k}", [i for i, b in enumerate(batch) if b["kind"] == k])
        for k in GIANTS
        if any(b["kind"] == k for b in batch)
    ]
    for label, idx in groups:
        mb = sum(batch[i]["bytes"] for i in idx) / 1e6
        row = f"{label:30} {len(idx):5} {mb:6.1f}"
        for t in shown:
            runs = results[t]
            total = median([sum(r["pages"][i][1] for i in idx) for r in runs])
            slow = median([max(r["pages"][i][1] for i in idx) for r in runs])
            row += f" {f'{total:.3f} ({slow:.3f})':>20}"
        print(row)

    for t in shown:
        per_page = [median([r["pages"][i][1] for r in results[t]]) for i in range(n)]
        i = max(range(n), key=per_page.__getitem__)
        b = batch[i]
        print(
            f"{t}'s slowest page: {i} ({b['category']}, {b['kind']},"
            f" {b['bytes'] / 1e6:.1f} MB), {per_page[i]:.3f} s"
        )
    for t in shown:
        differ = [
            b
            for p, b in zip(results[t][0]["pages"], batch, strict=True)
            if p[0] == "parsed" and p[2] != b["elements"]
        ]
        if differ:
            by = ", ".join(f"{c} {sum(b['category'] == c for b in differ)}" for c in CATEGORIES)
            print(
                f"{t}'s element count differs from Parsec's on {len(differ)} pages ({by}),"
                f" {sum(b['shadow'] for b in differ)} of them with a <template shadowrootmode>"
            )
    for t in shown:
        bad = [
            f"{i} {p[0]} {p[2] or ''} ({batch[i]['category']}, {batch[i]['kind']})".replace(
                "  ", " "
            )
            for i, p in enumerate(results[t][0]["pages"])
            if p[0] != "parsed" or p[2] == 0
        ]
        if bad:
            print(f"{t}, run 1: {len(bad)} pages without a document: " + "; ".join(bad[:8]))
    print("Load at the start of each run: " + ", ".join(f"{t} {load:.2f}" for t, load in loads))
    return summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--parsec", default=os.environ.get("PARSEC", HERE.parent.parent / "parsec"))
    ap.add_argument("--cache", type=Path)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--tools", help=f"comma-separated, from: {', '.join(TOOLS)}")
    ap.add_argument("--smoke", action="store_true", help="300 pages, giants cut to 2 MB, one run")
    ap.add_argument("--now", action="store_true", help="don't wait for a quiet machine")
    ap.add_argument("--check", action="store_true", help="fail if netweir fails a page or loses")
    ap.add_argument(
        "--tolerance",
        type=float,
        default=0.15,
        help="how much slower than the fastest other library --check allows",
    )
    ap.add_argument("--worker")
    ap.add_argument("--tool")
    ap.add_argument("--answer-key", help="write test.netweir.com's answer key here, then stop")
    ap.add_argument("--from", dest="start", type=int, default=0)
    args = ap.parse_args()

    pages = SMOKE_PAGES if args.smoke else PAGES
    cap = SMOKE_GIANT if args.smoke else 0
    folder = args.cache or HERE / ".cache" / "boss" / f"{pages}-{SEED}-{cap}"
    if args.worker:
        return worker(args.worker, folder, args.start)
    if args.tool:
        return runner(args.tool, folder)

    if not (folder / "pages.json").exists():
        shutil.rmtree(folder, ignore_errors=True)
        build_corpus(folder, pages, SEED, args.parsec, cap)
    meta = load_corpus(folder)
    if not args.smoke and meta["fingerprint"] != FINGERPRINT:
        sys.exit(
            f"fingerprint {meta['fingerprint']}, not Parsec's {FINGERPRINT}: run"
            f" bench/fetch-pages.sh in Parsec, delete {folder} and run again"
        )
    if args.answer_key:
        return answer_key(folder, meta, args.answer_key)
    batch = meta["batch"]
    print(versions())
    print(f"\nThe final boss: {len(batch)} pages, seed {meta['seed']}")
    for c in CATEGORIES:
        idx = [b for b in batch if b["category"] == c]
        print(f"  {c:12} {len(idx):5} pages {sum(b['bytes'] for b in idx) / 1e6:8.1f} MB")
    print(f"  fingerprint of every byte, in order: {meta['fingerprint']}", flush=True)

    if args.tools:
        tools = [t.strip() for t in args.tools.split(",")]
    elif args.smoke:
        tools = [t for t in TOOLS if t not in ("html5lib", "none")]
    else:
        tools = TOOLS
    runs = 1 if args.smoke else args.runs
    results = {t: [] for t in tools}
    loads = []
    deadline = time.time() + MAX_WAIT
    for r in range(runs):
        # A different library goes first each run.
        for k in range(len(tools)):
            name = tools[(k + r) % len(tools)]
            quick = args.now or args.smoke
            load = os.getloadavg()[0] if quick else wait_for_quiet(deadline)
            out = subprocess.run(
                [sys.executable, __file__, "--tool", name, "--cache", folder],
                capture_output=True,
                text=True,
                check=True,
            ).stdout
            result = json.loads(out)
            results[name].append(result)
            loads.append((name, load))
            took = sum(p[1] for p in result["pages"])
            print(f"  run {r + 1} {name:28} load {load:.2f} at start, {took:.1f} s", flush=True)

    summary = report(meta, tools, results, loads)
    if args.check:
        mine = summary["netweir"]
        if mine["crashes"]:
            sys.exit("netweir crashed, hung, raised or left a page without a document")
        rivals = {t: s["total"] for t, s in summary.items() if t not in ("netweir", "none")}
        best = min(rivals, key=rivals.get)
        if mine["total"] > rivals[best] * (1 + args.tolerance):
            sys.exit(f"netweir is slower than {best} on the final boss")


if __name__ == "__main__":
    main()
