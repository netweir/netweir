"""Spiders for the worker tests, in a module of their own so that worker
processes can import them."""

import dataclasses
import os
import time

import netweir
from netweir import loader

BASE = os.environ.get("NETWEIR_TEST_BASE", "")


class Shop(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback="item", meta={"from": page.url})

    def item(self, page):
        # Enough work that the pages spread over the workers.
        time.sleep(0.05)
        yield {
            "name": page.css("h1::text").get(),
            "from": page.meta["from"],
            "depth": page.depth,
        }
        yield {"pid": os.getpid()}


class Breaks(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        yield {"before": page.url}
        raise ValueError(f"broke on {page.url}")


class Tracks(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        yield {"price": page.css(".price_color::text", track="price").get()}


class Awkward(Exception):
    """Pickles, but can't be rebuilt from its pickle."""

    def __init__(self, a, b):
        super().__init__(f"{a} {b}")


class Unpicklable(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback="item")

    def item(self, page):
        name = page.css("h1::text").get()
        if name == "0":
            raise Awkward("odd", "one")
        if name == "1":
            yield {"kept": name}
            yield {"fn": lambda: None}
            yield {"lost": name}
            return
        yield {"name": name}


class SlowAfterFirst(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback="item")

    def item(self, page):
        if page.url.endswith("/item/0"):
            # Long enough for the next page's callback to be running.
            time.sleep(1)
            raise ValueError("stop here")
        time.sleep(5)
        yield {}


class Dies(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback="item")

    def item(self, page):
        if page.url.endswith("/item/3"):
            os._exit(3)
        yield {"name": page.css("h1::text").get()}


class NeedsArguments(netweir.Spider):
    start_urls: list[str] = []

    def __init__(self, required):
        self.required = required


class StartFails(netweir.Spider):
    async def start(self):
        raise RuntimeError("start broke")
        yield


class Linked(netweir.Spider):
    """An item per page, following every link."""

    start_urls: list[str] = []

    def parse(self, page):
        yield {"url": page.url}
        yield from page.follow_all(page.css("a::attr(href)"))


@dataclasses.dataclass
class Listing:
    name: str | None = None
    price: netweir.Price | None = None
    url: str | None = None
    tags: list[str] = dataclasses.field(
        default_factory=list, metadata={"output_processor": loader.Identity()}
    )


class ListingLoader(loader.ItemLoader):
    default_item_class = Listing
    default_output_processor = loader.TakeFirst()
    name_in = loader.MapCompose(str.strip)
    price_in = loader.MapCompose(netweir.parse_price)


class Loads(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        yield from page.follow_all(page.css("a.item::attr(href)"), callback="product")

    def product(self, page):
        item = ListingLoader(response=page)
        item.add_css("name", "h1::text")
        item.add_css("price", ".price::text")
        item.add_value("url", page.url)
        item.add_css("tags", "li::text")
        yield item.load_item()


class Logins(netweir.Spider):
    """Yields, from a worker, a request with a login, a timeout and a
    redirect limit of its own."""

    start_urls: list[str] = []

    def parse(self, page):
        yield page.follow(
            "/whoami", callback="item", auth=("ada", "secret"), timeout=4, max_redirects=3
        )

    def item(self, page):
        yield {
            "who": page.css("h1::text").get(),
            "timeout": page.request.timeout,
            "max_redirects": page.request.max_redirects,
        }


class Counts(netweir.Spider):
    """Keeps stats of its own, in whichever worker each page lands."""

    start_urls: list[str] = []

    def parse(self, page):
        self.stats.inc_value("pages_counted")
        self.stats.max_value("deepest", page.depth)
        time.sleep(0.02)
        yield from page.follow_all(page.css("a::attr(href)"))


class Feed(netweir.Spider):
    """Reads an XML page that needs recover mode."""

    start_urls: list[str] = []

    def parse(self, page):
        yield {"title": page.xpath("//title/text()").get()}


class Shelved(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback="item", cb_kwargs={"shelf": page.url})

    def item(self, page, shelf):
        yield {"name": page.css("h1::text").get(), "shelf": shelf, "pid": os.getpid()}
