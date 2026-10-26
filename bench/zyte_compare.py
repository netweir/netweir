"""Netweir's auto-extraction against Zyte API's on the same pages.

    ZYTE_API_KEY=... uv run python bench/zyte_compare.py URLS --save DIR
    uv run python bench/zyte_compare.py --from DIR

URLS is a text file with one URL per line (and optionally a type after
it: `product`, `product-list` or `job-posting`; `product` when left out).
For each, Zyte API fetches the page once and extracts from that same
response (`extractFrom: httpResponseBody`), and Netweir reads the body
Zyte fetched, so both read identical HTML. With `--save`, each page's
HTML and Zyte's item are kept, so `--from` can compare again later
without paying for the requests twice. Keep DIR out of the repo
(plans/ is never committed).

The report gives, per field, how often the two agree, how often only one
of them found a value, and how often both found different ones, then
how long Netweir took per page.
"""

import argparse
import base64
import json
import os
import sys
import time
import urllib.request
from pathlib import Path

from auto_eval import same

import netweir

TYPES = {
    "product": ("product", netweir.auto.Product),
    "product-list": ("productList", netweir.auto.ProductList),
    "job-posting": ("jobPosting", netweir.auto.JobPosting),
}

FIELDS = {
    "product": [
        "name",
        "price",
        "currency",
        "regular_price",
        "availability",
        "sku",
        "brand",
        "main_image",
        "breadcrumbs",
        "description",
    ],
    "product-list": ["category_name", "products", "pagination_next"],
    "job-posting": [
        "job_title",
        "hiring_organization",
        "job_location",
        "base_salary",
        "employment_type",
        "date_published",
        "valid_through",
    ],
}


def snake(name: str) -> str:
    return "".join("_" + c.lower() if c.isupper() else c for c in name)


def snake_keys(v):
    if isinstance(v, dict):
        return {snake(k): snake_keys(x) for k, x in v.items()}
    if isinstance(v, list):
        return [snake_keys(x) for x in v]
    return v


def ask_zyte(key: str, url: str, kind: str) -> tuple[str, dict]:
    field = TYPES[kind][0]
    body = json.dumps(
        {
            "url": url,
            "httpResponseBody": True,
            field: True,
            f"{field}Options": {"extractFrom": "httpResponseBody"},
        }
    ).encode()
    request = urllib.request.Request(
        "https://api.zyte.com/v1/extract",
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": "Basic " + base64.b64encode(f"{key}:".encode()).decode(),
        },
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        answer = json.load(response)
    html = base64.b64decode(answer["httpResponseBody"]).decode("utf-8", "replace")
    return html, answer.get(field) or {}


def agree(field: str, zyte, ours) -> bool:
    if field == "products":
        names = lambda items: {" ".join(str(p.get("name", "")).split()).lower() for p in items}  # noqa: E731
        z, o = names(zyte), names(ours)
        return bool(z) and len(z & o) >= 0.8 * len(z)
    if field == "pagination_next":
        return zyte.get("url") == ours.get("url")
    if field == "description":
        words = lambda s: set(str(s).lower().split())  # noqa: E731
        z, o = words(zyte), words(ours)
        return bool(z) and len(z & o) >= 0.8 * len(z)
    return same(field, zyte, ours)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("urls", nargs="?", type=Path)
    parser.add_argument("--save", type=Path, help="keep each page and Zyte's item here")
    parser.add_argument("--from", dest="saved", type=Path, help="compare pages kept before")
    parser.add_argument("--show", action="store_true", help="print every disagreement")
    args = parser.parse_args()
    pages: list[tuple[str, str, str, dict]] = []
    if args.saved:
        for meta in sorted(args.saved.glob("*.json")):
            kept = json.loads(meta.read_text())
            html = meta.with_suffix(".html").read_text(encoding="utf-8")
            pages.append((kept["url"], kept["type"], html, kept["zyte"]))
    elif args.urls:
        key = os.environ.get("ZYTE_API_KEY")
        if not key:
            print("set ZYTE_API_KEY first", file=sys.stderr)
            return 2
        for i, line in enumerate(args.urls.read_text().split("\n")):
            parts = line.split()
            if not parts or parts[0].startswith("#"):
                continue
            url, kind = parts[0], parts[1] if len(parts) > 1 else "product"
            try:
                html, item = ask_zyte(key, url, kind)
            except Exception as e:  # noqa: BLE001
                print(f"{url}: {e}", file=sys.stderr)
                continue
            pages.append((url, kind, html, item))
            if args.save:
                args.save.mkdir(parents=True, exist_ok=True)
                (args.save / f"{i:04}.html").write_text(html, encoding="utf-8")
                (args.save / f"{i:04}.json").write_text(
                    json.dumps({"url": url, "type": kind, "zyte": item}, indent=1)
                )
    else:
        parser.error("give a URL file or --from")
    tally: dict[str, dict[str, dict[str, int]]] = {}
    took = []
    for url, kind, html, zyte in pages:
        start = time.perf_counter()
        ours = TYPES[kind][1].extract(netweir.parse(html), url=url)
        took.append(time.perf_counter() - start)
        zyte = snake_keys(zyte)
        for f in FIELDS[kind]:
            t = tally.setdefault(kind, {}).setdefault(
                f, {"agree": 0, "only zyte": 0, "only netweir": 0, "differ": 0}
            )
            z, o = zyte.get(f), ours.get(f)
            if z in (None, [], "") and o in (None, [], ""):
                continue
            if o in (None, [], ""):
                t["only zyte"] += 1
            elif z in (None, [], ""):
                t["only netweir"] += 1
            elif agree(f, z, o):
                t["agree"] += 1
            else:
                t["differ"] += 1
                if args.show:
                    print(f"{url} {f}: zyte {z!r}, netweir {o!r}")
    for kind, fields in tally.items():
        print(f"\n{kind}: {sum(1 for p in pages if p[1] == kind)} pages")
        print(f"{'field':20} {'agree':>7} {'only zyte':>10} {'only netweir':>13} {'differ':>7}")
        for f, t in fields.items():
            print(
                f"{f:20} {t['agree']:7} {t['only zyte']:10} {t['only netweir']:13} {t['differ']:7}"
            )
    if took:
        took.sort()
        print(
            f"\nNetweir per page: median {1000 * took[len(took) // 2]:.1f} ms, "
            f"slowest {1000 * took[-1]:.1f} ms (parse and extract)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
