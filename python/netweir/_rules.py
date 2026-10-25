"""Declarative items and link rules. Both compile to Rust when defined, so a
spider made of them crawls without running Python per page."""

from __future__ import annotations

import logging
import re
from collections.abc import Callable, Iterable
from typing import Any

from netweir import _track
from netweir._native import ItemSpec, Node

#: `into` values Rust converts itself; anything else runs in Python.
_BUILTIN = {str: "text", int: "int", float: "float", bool: "bool"}


class Field:
    """One field of an Item. Made by ``netweir.css`` or ``netweir.xpath``."""

    __slots__ = ("all", "convert", "default", "into", "kind", "query", "re", "strip", "track")

    def __init__(self, kind, query, re, into, all, strip, default, track=None, convert=None):
        if into is not None and not callable(into):
            raise TypeError(f"into= takes a type or a function, not {into!r}")
        self.kind = kind
        self.query = query
        self.re = re
        self.into = into
        self.all = all
        self.strip = strip
        self.default = default
        self.track = track
        self.convert = convert
        # Compiling now reports a bad query or pattern where it was written.
        ItemSpec("check", [self._spec("check")])

    def _spec(self, name: str) -> tuple:
        convert = self.convert or _BUILTIN.get(self.into, "text")
        return (
            name,
            self.kind,
            self.query,
            self.re,
            self.all,
            self.strip,
            convert,
            self.default,
            self.track,
        )

    def _python_into(self) -> Callable[[Any], Any] | None:
        return None if self.into is None or self.into in _BUILTIN else self.into


def css(
    query: str,
    *,
    re: str | None = None,
    into: Callable[[Any], Any] | None = None,
    all: bool = False,
    strip: bool = False,
    default: Any = None,
    track: str | None = None,
) -> Field:
    """A field found by a CSS query (with ``::text`` and ``::attr()``).

    The value is the first result, as ``get()`` gives it, or every result with
    ``all=True`` (an empty list when nothing matches; ``default`` is then
    not used). ``re`` keeps what the pattern matches (its first group if it
    has one), in Rust's regex syntax. ``strip`` trims whitespace. ``into`` of
    ``int``, ``float``, ``bool`` or ``str`` converts in Rust; a value that
    won't convert becomes None, with a warning. Any other callable is applied
    to the finished value in Python. Nothing found gives ``default``.
    ``track`` follows the element through redesigns under that name (see
    ``Page.css``).
    """
    return Field("css", query, re, into, all, strip, default, track)


def xpath(
    query: str,
    *,
    re: str | None = None,
    into: Callable[[Any], Any] | None = None,
    all: bool = False,
    strip: bool = False,
    default: Any = None,
    track: str | None = None,
) -> Field:
    """A field found by an XPath 1.0 query; see ``css`` for the options."""
    return Field("xpath", query, re, into, all, strip, default, track)


def markdown(
    query: str | None = None,
    *,
    main_only: bool = False,
    all: bool = False,
    default: Any = None,
) -> Field:
    """A field holding an element's Markdown, as ``Node.markdown`` gives
    it, with links made absolute against the page: the first element the
    CSS ``query`` finds, or the whole page without one. ``main_only`` keeps
    only the main content, without its boilerplate; a page without main
    content gives ``default``. ``all`` gives one per element found."""
    convert = "markdown_main" if main_only else "markdown"
    return Field("css", query or ":root", None, None, all, False, default, None, convert)


def clean_text(
    query: str | None = None,
    *,
    main_only: bool = False,
    all: bool = False,
    default: Any = None,
) -> Field:
    """A field holding an element's text as a reader sees it, as
    ``Node.clean_text`` gives it; the options are as for ``markdown``."""
    convert = "clean_text_main" if main_only else "clean_text"
    return Field("css", query or ":root", None, None, all, False, default, None, convert)


class Item:
    """Subclass this and give it fields::

        class Book(netweir.Item):
            title = netweir.css("h1::text")
            price = netweir.css(".price_color::text", re=r"[\\d.]+", into=float)

    Use it in a rule (``netweir.Follow(..., extract=Book)``) or call
    ``Book.extract(page)`` in a callback. Items come out as dicts, with
    keys in the order the fields are written.
    """

    _spec: ItemSpec
    _fields: dict[str, Field]
    _python_into: dict[str, Callable[[Any], Any]]

    def __init_subclass__(cls, **kwargs):
        super().__init_subclass__(**kwargs)
        fields: dict[str, Field] = {}
        for klass in reversed(cls.__mro__):
            for name, value in vars(klass).items():
                if isinstance(value, Field):
                    if hasattr(Item, name):
                        raise TypeError(f"{cls.__qualname__}.{name}: Item uses {name!r} itself")
                    fields[name] = value
        # Warnings name the field in full, so two classes called Book don't
        # silence each other.
        cls._fields = fields
        qualified = f"{cls.__module__}.{cls.__qualname__}"
        cls._spec = ItemSpec(qualified, [f._spec(name) for name, f in fields.items()])
        cls._python_into = {
            name: into for name, f in fields.items() if (into := f._python_into()) is not None
        }

    @classmethod
    def extract(cls, node: Any, *, url: str | None = None) -> dict[str, Any]:
        """The item found in a Page or below a Node. ``url`` is what a
        Node's relative links are relative to; a Page knows its own."""
        root = getattr(node, "root", node)
        if not isinstance(root, Node):
            raise TypeError(f"extract() takes a Page or a Node, not {type(node).__name__}")
        store, threshold = _track.context()
        page_url = getattr(node, "url", None)
        site = _track.site_of(page_url)
        base = url if url is not None else _page_base(node)
        item, invalid, notes = cls._spec.extract(root, store, site, threshold, base)
        _warn_invalid(invalid)
        for field, what, score in notes:
            _track.report(field, site, what, score)
        return cls._finish(item)

    @classmethod
    def _finish(cls, item: dict[str, Any], warned: set[str] | None = None) -> dict[str, Any]:
        for name, into in cls._python_into.items():
            value = item[name]
            if value is None:
                continue
            try:
                item[name] = [into(v) for v in value] if isinstance(value, list) else into(value)
            except Exception as e:  # noqa: BLE001 - reported, value stored as None
                field = f"{cls.__module__}.{cls.__qualname__}.{name}"
                _warn_once(field, f"{into!r} failed on {value!r}: {e}", warned)
                item[name] = None
        return item


def _page_base(page: Any) -> str | None:
    urljoin = getattr(page, "urljoin", None)
    return urljoin("") if urljoin is not None else None


#: Fields already warned about outside a crawl (once per field for the life
#: of the process); a crawl keeps its own set.
_warned: set[str] = set()


def _warn_once(field: str, detail: str, warned: set[str] | None = None) -> None:
    warned = _warned if warned is None else warned
    if field not in warned:
        warned.add(field)
        logging.getLogger("netweir").warning("%s: %s; stored None", field, detail)


def _warn_invalid(invalid: list[tuple[str, str, str]], warned: set[str] | None = None) -> None:
    for field, text, kind in invalid:
        detail = f"query failed: {text}" if kind == "query" else f"couldn't convert {text!r}"
        _warn_once(field, detail, warned)


class Follow:
    """A link rule for a Spider's ``rules``.

    Takes every link matched by ``css`` (or ``xpath``): a matched element's
    ``href``, or the strings themselves if the query asks for them
    (``::attr(href)``, ``@href``). Links are resolved as a browser would.

    The pages they lead to get ``extract``'s item taken from them, in Rust,
    and are passed to ``callback`` if one is given. ``follow`` says whether
    the rules apply again on those pages; it defaults to True for a rule with
    neither ``extract`` nor ``callback``, and False otherwise, as in Scrapy.

    ``allow`` and ``deny`` are regexes matched against each absolute link:
    a link is taken if it matches one of ``allow`` (or ``allow`` is empty)
    and none of ``deny``. ``allow_domains`` and ``deny_domains`` do the same
    by site, a domain covering its subdomains.

    ``browser=True`` fetches the pages it leads to in Chrome, so their item
    and links come from the page as its scripts built it. ``steps`` is
    what to do on each first (see ``netweir.steps``), in place of the
    crawl's ``browser_steps``, and implies ``browser``. A call step among
    them names a spider method, or is one: the rules run in Rust, which
    holds the name.
    """

    __slots__ = (
        "allow",
        "allow_domains",
        "browser",
        "callback",
        "deny",
        "deny_domains",
        "extract",
        "follow",
        "kind",
        "priority",
        "query",
        "steps",
    )

    def __init__(
        self,
        css: str | None = None,
        *,
        xpath: str | None = None,
        extract: type[Item] | None = None,
        callback: Callable[..., Any] | str | None = None,
        follow: bool | None = None,
        priority: int = 0,
        allow: Iterable[str] = (),
        deny: Iterable[str] = (),
        allow_domains: Iterable[str] = (),
        deny_domains: Iterable[str] = (),
        browser: bool = False,
        steps: Iterable[Any] | None = None,
    ):
        if (css is None) == (xpath is None):
            raise TypeError("Follow takes a css query or an xpath query, not both or neither")
        if extract is not None and not (isinstance(extract, type) and issubclass(extract, Item)):
            raise TypeError("extract= takes an Item subclass")
        if not isinstance(priority, int):
            raise TypeError(f"priority takes an int, not {priority!r}")
        self.kind, self.query = ("css", css) if css is not None else ("xpath", xpath)
        self.extract = extract
        self.callback = callback
        self.follow = (extract is None and callback is None) if follow is None else follow
        self.priority = priority
        self.allow, self.deny = _patterns("allow", allow), _patterns("deny", deny)
        self.allow_domains = _domains(allow_domains)
        self.deny_domains = _domains(deny_domains)
        from netweir._request import _steps

        self.steps = None if steps is None else _steps(steps, "Follow's steps")
        self.browser = bool(browser or self.steps)


def _patterns(what: str, patterns: Iterable[str]) -> list[str]:
    """``patterns`` as a list, each checked to compile, so a typo fails
    when the rule is written rather than when the crawl starts."""
    if isinstance(patterns, str):
        patterns = [patterns]
    out = list(patterns)
    for p in out:
        try:
            re.compile(p)
        except re.error as e:
            raise ValueError(f"{what} pattern {p!r}: {e}") from None
    return out


def _domains(domains: Iterable[str]) -> list[str]:
    return [domains] if isinstance(domains, str) else list(domains)
