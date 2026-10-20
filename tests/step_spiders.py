"""Spiders for the steps tests that run with workers, in a module of their
own so that worker processes can import them."""

import os

import netweir
from netweir import steps


class Counted(netweir.Spider):
    """Opens a page in Chrome, loads more on it with its steps, and counts
    the items; the plain pages it links to go to the workers."""

    start_urls: list[str] = []

    async def start(self):
        for url in self.start_urls:
            yield netweir.Request(
                url,
                steps=[
                    steps.click("#accept"),
                    steps.click("#more"),
                    steps.wait_for("li:nth-child(3)"),
                    steps.call("count"),
                ],
            )

    async def count(self, page, request):
        return await page.evaluate("document.querySelectorAll('li').length")

    def parse(self, page):
        yield {
            "items": page.css("li::text").getall(),
            "steps": [(s["action"], s["status"], s["result"]) for s in page.steps],
            "pid": os.getpid(),
        }
        for href in page.css("a.plain::attr(href)").getall():
            yield page.follow(href, callback="plain")

    def plain(self, page):
        yield {"plain": page.url, "pid": os.getpid()}


class RuledSteps(netweir.Spider):
    """A rules spider whose rule fetches its pages in Chrome and loads
    more on them first; the rule's callback runs in a worker."""

    start_urls: list[str] = []
    rules = (
        netweir.Follow(
            "a.shop",
            callback="shop",
            steps=[
                steps.click("#accept"),
                steps.click("#more"),
                steps.wait_for("li:nth-child(3)"),
                steps.evaluate("document.querySelectorAll('li').length"),
            ],
        ),
    )

    def shop(self, page):
        yield {
            "items": page.css("li::text").getall(),
            "results": [s["result"] for s in page.steps],
            "pid": os.getpid(),
        }
