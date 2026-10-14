"""Item loaders on a shop listing, one item per product, as a Scrapy
callback fills them.

    uv run --group bench python bench/loaders.py

Each run parses the page and loads every product on it through a loader
with input and output processors, a regex and a value from the page. The
same callback runs four ways:

- netweir: netweir.loader on Netweir's selectors;
- itemloaders on netweir: Scrapy's itemloaders package on the same
  selectors, so the difference from the first is the loader alone;
- itemloaders + parsel: what a Scrapy project runs today;
- no loader: the same fields read straight off Netweir's selectors, for
  what a loader costs at all.

itemloaders and parsel come with Scrapy (in the dev group). The figure is
the best of N runs, in milliseconds.
"""

import argparse
import time

from pages import page

import netweir
from netweir import loader as nl

URL = "https://shop.example/catalogue/page-1.html"


def callback(root, loader_class, mapc, take_first, join):
    class Product(loader_class):
        default_output_processor = take_first()
        name_in = mapc(str.strip)
        description_out = join(" ")

    items = []
    for card in root.css("article.product_pod"):
        loader = Product(selector=card, page=URL)
        loader.add_css("name", "h3 a::attr(title)")
        loader.add_css("price", ".price_color::text", mapc(str.strip), re=r"[\d.]+")
        loader.add_xpath("url", ".//h3/a/@href")
        loader.add_css("stock", ".availability::text", mapc(str.strip))
        loader.add_xpath("rating", ".//p[contains(@class, 'star-rating')]/@class", re=r"\w+$")
        loader.add_css("description", ".description::text")
        loader.add_value("source", URL)
        items.append(loader.load_item())
    return items


def with_netweir(html):
    return callback(netweir.parse(html), nl.ItemLoader, nl.MapCompose, nl.TakeFirst, nl.Join)


def itemloaders_on_netweir(html):
    from itemloaders import ItemLoader
    from itemloaders.processors import Join, MapCompose, TakeFirst

    return callback(netweir.parse(html), ItemLoader, MapCompose, TakeFirst, Join)


def itemloaders_parsel(html):
    import parsel
    from itemloaders import ItemLoader
    from itemloaders.processors import Join, MapCompose, TakeFirst

    return callback(parsel.Selector(text=html), ItemLoader, MapCompose, TakeFirst, Join)


def no_loader(html):
    items = []
    for card in netweir.parse(html).css("article.product_pod"):
        price = card.css(".price_color::text").re_first(r"[\d.]+")
        rating = card.xpath(".//p[contains(@class, 'star-rating')]/@class").re_first(r"\w+$")
        items.append(
            {
                "name": card.css("h3 a::attr(title)").get().strip(),
                "price": price.strip(),
                "url": card.xpath(".//h3/a/@href").get(),
                "stock": card.css(".availability::text").getall()[-1].strip(),
                "rating": rating,
                "description": " ".join(card.css(".description::text").getall()),
                "source": URL,
            }
        )
    return items


def best_ms(fn, html, runs, budget=3.0):
    best = float("inf")
    spent = 0.0
    for _ in range(runs):
        start = time.perf_counter()
        fn(html)
        took = time.perf_counter() - start
        best = min(best, took)
        spent += took
        if spent > budget:
            break
    return best * 1000


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=30)
    args = ap.parse_args()

    runners = {
        "netweir": with_netweir,
        "itemloaders on netweir": itemloaders_on_netweir,
        "itemloaders + parsel": itemloaders_parsel,
        "no loader": no_loader,
    }
    for label, size in [("100 KB", 100_000), ("1 MB", 1_000_000)]:
        html = page(size)
        expected = with_netweir(html)
        for name, fn in runners.items():
            if fn(html) != expected:
                raise SystemExit(f"{name} gave different items from netweir")
        print(f"\n{label} listing, {len(expected)} items", flush=True)
        times = {name: best_ms(fn, html, args.runs) for name, fn in runners.items()}
        for name, ms in sorted(times.items(), key=lambda kv: kv[1]):
            each = ms * 1000 / len(expected)
            print(
                f"  {name:24} {ms:8.2f} ms  {each:6.1f} µs an item  {ms / times['netweir']:5.2f}x"
            )


if __name__ == "__main__":
    main()
