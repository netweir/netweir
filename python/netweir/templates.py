"""Ready-made crawls: a shop's products, a site's articles or a board's
jobs from a URL, without selectors. They're declarative spiders, so
Netweir runs them in Rust, and you can subclass them like any Spider.

``strategy`` decides how far the crawl goes:

- ``"navigation"``: from the start pages, follow sub-categories and next
  pages, and take every item they list.
- ``"pagination_only"``: only the start listing and its next pages.
- ``"full"``: navigation, plus every link on the site; any page that is
  an item gives one, so items no listing shows are found too.
- ``"direct_item"``: the start URLs are items; nothing is followed.
- ``"automatic"`` (the default): ``"full"`` when a start URL is a site's
  home page, ``"navigation"`` otherwise.

Crawls stay on the start URLs' sites. Limit them with the usual settings
(``max_pages``, ``max_items``, ``max_depth``)."""

from __future__ import annotations

from typing import Any, ClassVar
from urllib.parse import urlsplit

from netweir import auto
from netweir._crawl import Spider
from netweir._rules import Extract, Follow, Item

STRATEGIES = ("automatic", "full", "navigation", "pagination_only", "direct_item")


class _Template(Spider):
    #: The item each item page gives; a subclass of the auto type pins
    #: fields a site gets wrong.
    item: ClassVar[type[Item]]
    #: The navigation type that lists items and leads on.
    navigation: ClassVar[type[Item]]
    #: The page type of an item page, for the full strategy.
    item_type: ClassVar[str]
    strategy: str = "automatic"
    #: The start URL (or several, separated by spaces), as ``-a url=`` or
    #: ``--url`` sets it; ``start_urls`` works too.
    url: str | None = None
    _allowed: list[str] | None = None

    @property  # type: ignore[override]
    def start_urls(self) -> list[str]:
        return self.url.split() if self.url else []

    @property  # type: ignore[override]
    def allowed_domains(self) -> list[str]:
        if self._allowed is not None:
            return self._allowed
        return sorted({urlsplit(u).hostname or "" for u in self.start_urls} - {""})

    @allowed_domains.setter
    def allowed_domains(self, domains: list[str]) -> None:
        self._allowed = list(domains)

    def chosen_strategy(self) -> str:
        """The strategy the crawl runs: ``strategy``, with ``"automatic"``
        decided from the start URLs."""
        if self.strategy not in STRATEGIES:
            raise ValueError(f"strategy is one of {', '.join(STRATEGIES)}, not {self.strategy!r}")
        if self.strategy != "automatic":
            return self.strategy
        home = any(urlsplit(u).path in ("", "/") for u in self.start_urls)
        return "full" if home else "navigation"

    @property  # type: ignore[override]
    def rules(self) -> list[Any]:
        strategy = self.chosen_strategy()
        if strategy == "direct_item":
            return [Extract(self.item)]
        nav = self.navigation
        items = Follow(navigation=nav, part="items", extract=self.item, follow=strategy == "full")
        rules = [items, Follow(navigation=nav, part="next_page")]
        if strategy == "pagination_only":
            return rules
        rules.append(Follow(navigation=nav, part="sub_categories"))
        if strategy == "full":
            rules.append(Follow("a[href]"))
        # An item page reached as a category (its listing wasn't
        # recognised) still gives its item.
        rules.append(Extract(self.item, when=self.item_type))
        return rules


class Ecommerce(_Template):
    """A shop's products: ``netweir.auto.Product`` items."""

    item = auto.Product
    navigation = auto.ProductNavigation
    item_type = "product"


class Article(_Template):
    """A site's articles: ``netweir.auto.Article`` items."""

    item = auto.Article
    navigation = auto.ArticleNavigation
    item_type = "article"


class Job(_Template):
    """A jobs board's adverts: ``netweir.auto.JobPosting`` items."""

    item = auto.JobPosting
    navigation = auto.JobPostingNavigation
    item_type = "job-posting"


#: The templates by the name ``netweir crawl`` takes.
TEMPLATES: dict[str, type[_Template]] = {"ecommerce": Ecommerce, "article": Article, "job": Job}
