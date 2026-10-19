"""Spiders for feeds: XML read node by node, and CSV read row by row, as
Scrapy's XMLFeedSpider and CSVFeedSpider read them."""

from __future__ import annotations

import csv
import inspect
import io
import logging
from collections.abc import AsyncIterator, Iterable, Sequence
from typing import Any

from netweir._crawl import Spider
from netweir._errors import NotConfigured
from netweir._fetch import Page
from netweir._hooks import _NOT_MANY
from netweir._native import Selection, _xml_nodes

log = logging.getLogger("netweir")

#: Seconds iternodes may take to read one node: a crawl's budget for a
#: page.
_NODE_BUDGET = 10.0


async def _results(value: Any) -> list[Any]:
    """What a ``parse_node`` or ``parse_row`` gave, as a list: one item or
    request, any iterable of them (async ones too), or None for nothing."""
    if inspect.isawaitable(value):
        value = await value
    if value is None:
        return []
    if hasattr(value, "__aiter__"):
        return [out async for out in value]
    if isinstance(value, Iterable) and not isinstance(value, _NOT_MANY):
        return list(value)
    return [value]


async def _adapted(spider: Any, page: Page) -> Page:
    adapted = spider.adapt_response(page)
    if inspect.isawaitable(adapted):
        adapted = await adapted
    if not isinstance(adapted, Page):
        raise TypeError(
            f"adapt_response must return a Page, not {type(adapted).__name__}"
            " (page.replace(body=...) makes one with a new body)"
        )
    return adapted


async def _processed(spider: Any, page: Page, results: list[Any]) -> list[Any]:
    processed = spider.process_results(page, results)
    if inspect.isawaitable(processed):
        processed = await processed
    return list(processed or [])


class XMLFeedSpider(Spider):
    """A spider for XML feeds: each element named ``itertag`` goes to
    ``parse_node(page, node)``, which you write, and which returns (or
    yields) items and Requests. Scrapy's XMLFeedSpider.

    ``iterator`` says how the feed is read:

    - ``"iternodes"`` (the default) reads one node at a time, so a large
      feed never has to be in memory whole. Each node is a document of its
      own: ``node.xpath("/..")`` finds nothing above it.
    - ``"xml"`` parses the whole feed as XML first.
    - ``"html"`` parses it as HTML, for markup too broken to be XML. HTML
      parsing loses what's inside a ``<link>``, which RSS uses for URLs.

    ``namespaces`` lists ``(prefix, uri)`` pairs, registered on every node
    so its queries can use the prefixes, and usable in ``itertag``
    (``"n:url"``). A prefix in ``itertag`` that isn't listed means what the
    feed itself declares it to mean. As in XPath, a tag without a prefix is
    in no namespace: an Atom feed's entries need one.

    Each node goes to ``parse_node`` as a Selection of one, as Scrapy hands
    over a Selector: ``node.xpath()``, ``node.css()``, ``node.get()``,
    ``node.attrib``, ``node.re()`` and ``node.register_namespace()`` work.

    ``recover = True`` reads broken XML in recover mode (see
    ``netweir.parse``); ``None``, the default, follows
    ``Settings.xml_recover``.

    ``adapt_response(page)`` can change the page before it's read and must
    return a Page (``page.replace(body=...)`` makes one).
    ``process_results(page, results)`` sees what each node gave, as a
    list, and returns the list to keep. Any of the three can be async.
    """

    #: "iternodes", "xml" or "html".
    iterator: str = "iternodes"
    #: The name of the elements to read: ``"item"`` for RSS.
    itertag: str = "item"
    #: (prefix, uri) pairs.
    namespaces: Sequence[tuple[str, str]] = ()
    #: Read broken XML in recover mode; None follows Settings.xml_recover.
    recover: bool | None = None

    def adapt_response(self, page: Page) -> Page:
        """The page to read, given the page fetched. Unchanged by default."""
        return page

    def process_results(self, page: Page, results: list[Any]) -> list[Any]:
        """The results to keep of what ``parse_node`` gave for one node.
        All of them by default."""
        return results

    async def parse(self, page: Page) -> AsyncIterator[Any]:
        if not callable(getattr(self, "parse_node", None)):
            raise NotConfigured(
                f"{type(self).__name__} needs a parse_node(self, page, node) method"
                " to read this XML feed"
            )
        page = await _adapted(self, page)
        for node in self._nodes(page):
            results = await _results(self.parse_node(page, node))
            for out in await _processed(self, page, results):
                yield out

    def _nodes(self, page: Page) -> Iterable[Selection]:
        namespaces = [(str(prefix), str(uri)) for prefix, uri in self.namespaces]
        recover = self.settings.xml_recover if self.recover is None else bool(self.recover)
        if self.iterator == "iternodes":
            nodes = _xml_nodes(
                page.text,
                self.itertag,
                namespaces,
                recover=recover,
                timeout=_NODE_BUDGET,
                place=page.url,
            )
            for node in nodes:
                for prefix, uri in namespaces:
                    node.register_namespace(prefix, uri)
                yield node.xpath(".")
            return
        if self.iterator not in ("xml", "html"):
            raise ValueError(
                f"unknown iterator {self.iterator!r}: use 'iternodes', 'xml' or 'html'"
            )
        # The page's own document when it's the right kind, so it isn't
        # parsed twice, through a handle of this spider's own ("/"), so
        # the namespaces go on these nodes and not on page.root.
        parsed = page._root
        kind = parsed.type if parsed is not None else page._response.markup
        timeout, page_recover = page._parse
        if kind == self.iterator and (parsed is not None or recover == page_recover):
            root = page.root
        else:
            root = page._response.parse(type=self.iterator, recover=recover, timeout=timeout)
        top = root.xpath("/")[0]
        for prefix, uri in namespaces:
            top.register_namespace(prefix, uri)
        prefix = self.itertag.partition(":")[0] if ":" in self.itertag else None
        if prefix is not None and prefix not in {p for p, _ in namespaces}:
            # As iternodes reads it: what the feed declares the prefix as.
            uri = top.xpath("namespace-uri((//*[name()=$tag])[1])", tag=self.itertag).get()
            if not uri:
                return
            top.register_namespace(prefix, uri)
        nodes = top.xpath(f"//{self.itertag}")
        for i in range(len(nodes)):
            yield nodes[i : i + 1]


class CSVFeedSpider(Spider):
    """A spider for CSV feeds: each row goes to ``parse_row(page, row)``,
    which you write, as a dict keyed by the headers. Scrapy's
    CSVFeedSpider.

    ``delimiter`` and ``quotechar`` default to ``,`` and ``"``. Without
    ``headers``, the first row names the columns. A row with more or fewer
    fields than there are headers is skipped, with a warning.
    ``adapt_response`` and ``process_results`` work as on XMLFeedSpider.
    """

    #: The field separator; "," when None.
    delimiter: str | None = None
    #: The quote character; '"' when None.
    quotechar: str | None = None
    #: The column names; the first row's when None.
    headers: Sequence[str] | None = None

    def adapt_response(self, page: Page) -> Page:
        """The page to read, given the page fetched. Unchanged by default."""
        return page

    def process_results(self, page: Page, results: list[Any]) -> list[Any]:
        """The results to keep of what ``parse_row`` gave for one row. All
        of them by default."""
        return results

    async def parse(self, page: Page) -> AsyncIterator[Any]:
        if not callable(getattr(self, "parse_row", None)):
            raise NotConfigured(
                f"{type(self).__name__} needs a parse_row(self, page, row) method"
                " to read this CSV feed"
            )
        page = await _adapted(self, page)
        for row in self._rows(page):
            results = await _results(self.parse_row(page, row))
            for out in await _processed(self, page, results):
                yield out

    def _rows(self, page: Page) -> Iterable[dict[str, str]]:
        options = {}
        if self.delimiter:
            options["delimiter"] = self.delimiter
        if self.quotechar:
            options["quotechar"] = self.quotechar
        # newline="" so a quoted field may hold a line break.
        reader = csv.reader(io.StringIO(page.text, newline=""), **options)
        headers = list(self.headers) if self.headers else next(reader, None)
        if not headers:
            return
        for fields in reader:
            if len(fields) != len(headers):
                log.warning(
                    "%s: ignoring row %d (length: %d, should be: %d)",
                    page.url,
                    reader.line_num,
                    len(fields),
                    len(headers),
                )
                continue
            yield dict(zip(headers, fields, strict=True))
