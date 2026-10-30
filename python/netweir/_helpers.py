"""Prices, dates and articles as pages write them. The reading is done in
Rust; this gives the results their Python types."""

from __future__ import annotations

import datetime
from dataclasses import dataclass
from decimal import Decimal
from typing import Any

from netweir._native import _currency_code, _parse_date, _parse_price


@dataclass(frozen=True)
class Price:
    """A price: ``amount`` as a Decimal and ``currency`` as written next to
    it (``"£"``, ``"USD"``, ``"zł"``), or None. Exports as
    ``{"amount": "1234.50", "currency": "£"}``."""

    amount: Decimal
    currency: str | None = None

    @property
    def code(self) -> str | None:
        """The ISO 4217 code, when the currency names only one: ``£`` is
        GBP, ``US$`` is USD, but ``$`` and ``¥`` are None."""
        return _currency_code(self.currency) if self.currency else None


def parse_price(text: str) -> Price | None:
    """The first price in ``text``, or None when there's no number in it.

    ``1,234.50``, ``1.234,50``, ``1 234,50`` and ``1'234.50`` are all
    1234.50: the separator with one or two digits after it is the decimal
    point.
    """
    found = _parse_price(text)
    if found is None:
        return None
    amount, currency = found
    return Price(Decimal(amount), currency)


def parse_date(
    text: str,
    *,
    day_first: bool | None = None,
    now: datetime.datetime | datetime.date | None = None,
) -> datetime.datetime | None:
    """The date in ``text`` as a datetime, or None when it isn't one that's
    fully understood.

    A date with a zone gives an aware datetime, one without a naive one.
    ``day_first`` settles ``03/04/2026``: True reads it as 3 April, False
    as March 4, and None (the default) reads a date written with dots
    (``03.04.2026``) day first and one with slashes or dashes month first.
    Relative phrases (``3 days ago``, ``yesterday``) count from ``now``,
    the current time by default, and keep its zone; a ``date`` is its
    midnight. Hours, minutes and seconds ago are elapsed time, so in a zone
    with summer time ``1 hour ago`` is always an hour; days and longer are
    on the calendar, so ``1 day ago`` is the same clock time yesterday.
    """
    if now is None:
        now = datetime.datetime.now()
    elif not isinstance(now, datetime.datetime):
        if not isinstance(now, datetime.date):
            raise TypeError(f"now= takes a datetime or a date, not {type(now).__name__}")
        now = datetime.datetime(now.year, now.month, now.day)
    now_fields = (now.year, now.month, now.day, now.hour, now.minute, now.second, now.microsecond)
    found = _parse_date(text, day_first, now_fields)
    if found is None:
        return None
    fields, offset, from_now, elapsed = found
    if offset is not None:
        tz = datetime.UTC if offset == 0 else datetime.timezone(datetime.timedelta(seconds=offset))
    else:
        tz = now.tzinfo if from_now else None
    if from_now and offset is None and fields == now_fields:
        # No calendar change: start from `now` itself, which keeps its fold
        # (which of a repeated hour's two 01:30s it is).
        found = now
    else:
        found = datetime.datetime(*fields, tzinfo=tz)
    if not elapsed:
        return found
    step = datetime.timedelta(seconds=elapsed)
    if found.utcoffset() is None:
        return found + step
    # Aware datetimes add on the clock face; elapsed time is added in UTC.
    return (found.astimezone(datetime.UTC) + step).astimezone(found.tzinfo)


@dataclass(frozen=True)
class Article:
    """What a page says about the article on it, each field None when the
    page doesn't say.

    ``text`` is the main content as a reader sees it, paragraphs separated
    by blank lines, and ``html`` the same content's HTML; both leave out
    the boilerplate inside it (share buttons, related links, ads) and are
    None when the page has no main content. ``published`` is a datetime,
    read as ``parse_date`` reads it. ``image`` is absolute."""

    title: str | None = None
    byline: str | None = None
    published: datetime.datetime | None = None
    image: str | None = None
    language: str | None = None
    text: str | None = None
    html: str | None = None


@dataclass(frozen=True)
class Records:
    """A page's main repeated region: table rows, cards or list entries.

    ``rows`` holds a dict per row with every column, a string or None
    where the row lacks it. ``columns`` lists each column's ``name`` and
    the ``selector`` that finds it inside a row (``::attr()`` for links
    and images), and ``selector`` finds the rows on the page."""

    selector: str
    columns: list[dict[str, str]]
    rows: list[dict[str, str | None]]


@dataclass(frozen=True)
class Chunk:
    """A piece of a page's Markdown, sized for retrieval, from
    ``page.chunks()`` or a node's ``chunks()``.

    ``id`` is the same for the same chunk of the same page from crawl to
    crawl, so a store can skip what it already holds; a change to one
    section leaves the other sections' ids alone. ``headings`` are the
    headings the chunk sits under, outermost first. ``index`` is its place
    among the page's chunks, from 0."""

    id: str
    text: str
    headings: tuple[str, ...]
    index: int
    url: str | None

    def to_dict(self) -> dict:
        """The chunk as a dict, for JSON: ``id``, ``text``, ``headings``
        (a list), ``index`` and ``url``."""
        return {
            "id": self.id,
            "text": self.text,
            "headings": list(self.headings),
            "index": self.index,
            "url": self.url,
        }

    def _metadata(self) -> dict:
        return {"url": self.url, "headings": list(self.headings), "index": self.index}

    def to_langchain(self) -> Any:
        """The chunk as a LangChain ``Document``, with its id, and its
        URL, headings and index as metadata. Needs ``langchain-core``."""
        try:
            from langchain_core.documents import Document
        except ImportError:
            raise ImportError("to_langchain needs LangChain: pip install langchain-core") from None
        return Document(page_content=self.text, metadata=self._metadata(), id=self.id)

    def to_llamaindex(self) -> Any:
        """The chunk as a LlamaIndex ``TextNode``, with its id, and its
        URL, headings and index as metadata. Needs ``llama-index-core``."""
        try:
            from llama_index.core.schema import TextNode
        except ImportError:
            raise ImportError(
                "to_llamaindex needs LlamaIndex: pip install llama-index-core"
            ) from None
        return TextNode(text=self.text, id_=self.id, metadata=self._metadata())


def _chunk_sizes(max_chars: int, overlap: int) -> None:
    if isinstance(max_chars, bool) or not isinstance(max_chars, int) or max_chars < 1:
        raise ValueError(
            f"max_chars is a whole number of characters, at least 1, not {max_chars!r}"
        )
    if isinstance(overlap, bool) or not isinstance(overlap, int) or not 0 <= overlap < max_chars:
        raise ValueError(f"overlap is from 0 to less than max_chars ({max_chars}), not {overlap!r}")


def _records(selector: str, columns: list[tuple[str, str]], rows: list) -> Records:
    return Records(selector, [{"name": n, "selector": s} for n, s in columns], rows)


def _article(
    title: str | None,
    byline: str | None,
    published: str | None,
    image: str | None,
    language: str | None,
    text: str | None,
    html: str | None,
) -> Article:
    when = parse_date(published) if published else None
    return Article(title, byline, when, image, language, text, html)
