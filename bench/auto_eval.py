"""How well auto-extraction reads pages from their layout, measured on
pages that also carry structured data.

    uv run python bench/auto_eval.py PAGES_DIR [--type product]

A page with a complete schema.org Product (or JobPosting) is its own
answer key: the item read from its JSON-LD and microdata is what the
page says it holds. The same page with its structured data, Open Graph
tags and app state taken out is what a shop without them looks like, and
reading it again shows what the layout readers recover. The report gives
each field's share right, wrong and missing.

PAGES_DIR holds `.html` files, each with a `.url` file beside it giving
the page's URL (or a `pages.json` mapping file names to URLs). Pages are
never committed: keep them under plans/ or somewhere outside the repo.
"""

import argparse
import json
import re
import sys
from pathlib import Path

import netweir

FIELDS = {
    "product": [
        "name",
        "price",
        "currency",
        "regular_price",
        "availability",
        "sku",
        "main_image",
        "breadcrumbs",
    ],
    "job-posting": [
        "job_title",
        "base_salary",
        "employment_type",
        "job_location",
        "hiring_organization",
        "valid_through",
    ],
}

TYPES = {"product": netweir.auto.Product, "job-posting": netweir.auto.JobPosting}

# Every inline script goes: JSON-LD, and the app state js_state reads
# (whatever name it's under). Scripts loaded from a file stay.
INLINE_SCRIPT = re.compile(r"<script\b(?![^>]*\bsrc=)[^>]*>.*?</script>", re.I | re.S)
META = re.compile(r"<meta[^>]+(?:property|name)=[\"']?(?:og|product|twitter):[^>]*>", re.I)
TAG = re.compile(r"<[a-zA-Z][^>]*>")
ATTRIBUTE = re.compile(r"""\s+([^\s=>/]+)(\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"'>]+))?""")
MICRODATA = {"itemprop", "itemscope", "itemtype", "itemid", "itemref"}


def _no_microdata(tag: re.Match) -> str:
    text = tag.group(0)
    name_end = re.match(r"<[a-zA-Z][^\s>/]*", text).end()
    attrs = ATTRIBUTE.sub(
        lambda a: "" if a.group(1).lower() in MICRODATA else a.group(0), text[name_end:]
    )
    return text[:name_end] + attrs


def without_structured_data(html: str) -> str:
    """The page as a shop without structured data would send it: no
    inline scripts, no Open Graph or product meta tags, and no microdata
    attributes (taken out of tags only, never out of text or values)."""
    html = INLINE_SCRIPT.sub("", html)
    html = META.sub("", html)
    return TAG.sub(_no_microdata, html)


def same(field: str, want, got) -> bool:
    """Whether two values say the same thing, allowing for how pages
    write them: amounts compared as numbers, text without case or extra
    spaces, a breadcrumb trail by its names, an image by its URL's path."""
    if want is None or got is None:
        return want == got
    if field in ("price", "regular_price"):
        try:
            return float(want) == float(got)
        except ValueError:
            return False
    if field == "breadcrumbs":
        names = lambda trail: [" ".join(c.get("name", "").split()).lower() for c in trail]  # noqa: E731
        w, g = names(want), names(got)
        # A trail often ends with the product itself in one and not the
        # other; a trail of one crumb has to match whole.
        if not w or not g:
            return w == g
        return w == g or (len(w) > 1 and w[:-1] == g) or (len(g) > 1 and w == g[:-1])
    if field == "main_image":
        path = lambda u: re.sub(r"^https?://[^/]+", "", u["url"]).split("?")[0]  # noqa: E731
        return path(want) == path(got)
    if field == "base_salary":
        given = [k for k in ("value_min", "value_max") if want.get(k) is not None]
        if not given:
            return same("job_title", want.get("raw"), got.get("raw"))
        amounts = all(same("price", want[k], got.get(k) or "x") for k in given)
        return amounts and (want.get("currency") in (None, got.get("currency")))
    if isinstance(want, dict):
        want, got = want.get("name") or want.get("raw"), got.get("name") or got.get("raw")
    norm = lambda s: " ".join(str(s).split()).lower()  # noqa: E731
    return norm(want) == norm(got)


def pages(folder: Path):
    mapping = {}
    if (folder / "pages.json").exists():
        mapping = json.loads((folder / "pages.json").read_text())
    for html in sorted(folder.glob("*.html")):
        url_file = html.with_suffix(".url")
        url = mapping.get(html.name) or (
            url_file.read_text().strip() if url_file.exists() else None
        )
        if url:
            yield html, url


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("pages", type=Path)
    parser.add_argument("--type", default="product", choices=sorted(TYPES))
    parser.add_argument("--show", action="store_true", help="print every wrong value")
    args = parser.parse_args()
    kind, fields = TYPES[args.type], FIELDS[args.type]
    tally = {f: {"right": 0, "wrong": 0, "missing": 0} for f in fields}
    used = skipped = 0
    for path, url in pages(args.pages):
        html = path.read_text(encoding="utf-8", errors="replace")
        answer = kind.extract(netweir.parse(html), url=url)
        sources = answer["metadata"]["sources"]
        structured = {
            f
            for f in fields
            if answer.get(f) is not None
            and sources.get(f, "").split(":")[0] in ("json-ld", "microdata")
        }
        if answer["metadata"]["probability"] < 0.9 or not structured:
            skipped += 1
            continue
        used += 1
        guess = kind.extract(netweir.parse(without_structured_data(html)), url=url)
        for f in structured:
            if guess.get(f) is None:
                tally[f]["missing"] += 1
            elif same(f, answer[f], guess[f]):
                tally[f]["right"] += 1
            else:
                tally[f]["wrong"] += 1
                if args.show:
                    print(f"{path.name} {f}: {answer[f]!r} read as {guess[f]!r}")
    print(f"{used} pages with structured data, {skipped} without or incomplete")
    print(f"{'field':20} {'pages':>6} {'right':>7} {'wrong':>7} {'missing':>8}")
    for f, t in tally.items():
        n = sum(t.values())
        if n:
            right, wrong, missing = (f"{100 * t[k] / n:6.0f}%" for k in t)
            print(f"{f:20} {n:6} {right} {wrong} {missing:>8}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
