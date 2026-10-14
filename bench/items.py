"""Per-item extraction: a few small queries inside each product card.

    uv run --group bench python bench/items.py
    uv run --group bench python bench/items.py --check   # exit 1 if netweir loses

The commonest scraping loop there is: find every card, then pull a handful
of fields out of each one. Each query runs on one small element, so what
it costs is mostly overhead (compiling the query, building results), not
matching. Every library parses the page outside the timing and runs the
same queries the way its documentation shows; the figure is the best of N
runs, in milliseconds.
"""

import argparse
import sys
import time

from pages import page

import netweir

CARD = "article.product_pod"
CSS = {
    "title": "h3 a::attr(title)",
    "price": ".price_color::text",
    "link": "h3 a::attr(href)",
    "image": "img::attr(alt)",
}
XPATH = {
    "title": ".//h3/a/@title",
    "price": ".//p[@class='price_color']/text()",
    "link": ".//h3/a/@href",
    "rating": ".//p[contains(@class, 'star-rating')]/@class",
}


def css_parsel_style(root):
    return [{k: card.css(q).get() for k, q in CSS.items()} for card in root.css(CARD)]


def xpath_parsel_style(root):
    xpath = ".//article[@class='product_pod']"
    return [{k: card.xpath(q).get() for k, q in XPATH.items()} for card in root.xpath(xpath)]


def css_selectolax(tree):
    out = []
    for card in tree.css(CARD):
        a = card.css_first("h3 a")
        out.append(
            {
                "title": a.attributes["title"],
                "price": card.css_first(".price_color").text(deep=False),
                "link": a.attributes["href"],
                "image": card.css_first("img").attributes["alt"],
            }
        )
    return out


def css_lxml(tree):
    out = []
    for card in tree.cssselect(CARD):
        a = card.cssselect("h3 a")[0]
        out.append(
            {
                "title": a.get("title"),
                "price": card.cssselect(".price_color")[0].text,
                "link": a.get("href"),
                "image": card.cssselect("img")[0].get("alt"),
            }
        )
    return out


def xpath_lxml(tree):
    xpath = ".//article[@class='product_pod']"
    return [{k: str(card.xpath(q)[0]) for k, q in XPATH.items()} for card in tree.xpath(xpath)]


def parsers():
    import parsel
    from lxml import html as lxml_html
    from selectolax.lexbor import LexborHTMLParser

    return {
        "netweir": netweir.parse,
        "parsel": parsel.Selector,
        "selectolax": LexborHTMLParser,
        "lxml": lxml_html.fromstring,
    }


def best_ms(fn, tree, runs, budget=3.0):
    """Best of `runs` timings, stopping early once `budget` seconds are spent."""
    best = float("inf")
    spent = 0.0
    for _ in range(runs):
        start = time.perf_counter()
        fn(tree)
        took = time.perf_counter() - start
        best = min(best, took)
        spent += took
        if spent > budget:
            break
    return best * 1000


def compare(title, runners, trees, runs):
    expected = runners["netweir"](trees["netweir"])
    times = {}
    for name, fn in runners.items():
        if fn(trees[name]) != expected:
            sys.exit(f"{title}: {name} extracted different data from netweir")
        times[name] = best_ms(fn, trees[name], runs)
    items = len(expected)
    print(f"  {title}, {items} items x {len(expected[0])} fields")
    for name, ms in sorted(times.items(), key=lambda kv: kv[1]):
        per_query = ms * 1000 / (items * len(expected[0]))
        print(
            f"    {name:12} {ms:8.2f} ms  {per_query:5.2f} us/query  {ms / times['netweir']:5.2f}x"
        )
    return times


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="fail if netweir is clearly slower")
    ap.add_argument("--tolerance", type=float, default=0.05)
    ap.add_argument("--runs", type=int, default=15)
    args = ap.parse_args()

    html = page(1_000_000)
    trees = {name: parse(html) for name, parse in parsers().items()}
    print("\n1 MB page", flush=True)
    losses = []
    for title, runners in [
        (
            "CSS",
            {
                "netweir": css_parsel_style,
                "parsel": css_parsel_style,
                "selectolax": css_selectolax,
                "lxml": css_lxml,
            },
        ),
        (
            "XPath",
            {"netweir": xpath_parsel_style, "parsel": xpath_parsel_style, "lxml": xpath_lxml},
        ),
    ]:
        times = compare(title, runners, trees, args.runs)
        rival = min(v for k, v in times.items() if k != "netweir")
        if times["netweir"] > rival * (1 + args.tolerance):
            losses.append(title)

    if args.check and losses:
        sys.exit(f"netweir is slower than the fastest alternative per item: {', '.join(losses)}")


if __name__ == "__main__":
    main()
