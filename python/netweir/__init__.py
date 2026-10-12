"""Fast web scraping with a Rust engine."""

from netweir import export, repair
from netweir._browser import browser
from netweir._crawl import Settings, Spider
from netweir._errors import (
    Blocked,
    BrowserError,
    BrowserTimeout,
    DropItem,
    FetchError,
    IgnoreRequest,
)
from netweir._fetch import Client, Page, get, post, request, submit
from netweir._form import Form
from netweir._helpers import Price, parse_date, parse_price
from netweir._monitor import Checks, validate
from netweir._native import (
    Browser,
    BrowserContext,
    BrowserPage,
    BrowserResponse,
    Node,
    ParseTimeout,
    Selection,
    SelectorError,
    XPathError,
    __version__,
    js_data,
    parse,
)
from netweir._request import Request
from netweir._rules import Follow, Item, css, xpath

__all__ = [
    "Blocked",
    "Browser",
    "BrowserContext",
    "BrowserError",
    "BrowserPage",
    "BrowserResponse",
    "BrowserTimeout",
    "Checks",
    "Client",
    "DropItem",
    "Follow",
    "Form",
    "IgnoreRequest",
    "Item",
    "Request",
    "Settings",
    "Spider",
    "FetchError",
    "Node",
    "Page",
    "ParseTimeout",
    "Price",
    "Selection",
    "SelectorError",
    "XPathError",
    "__version__",
    "browser",
    "css",
    "export",
    "get",
    "js_data",
    "parse",
    "parse_date",
    "parse_price",
    "post",
    "repair",
    "request",
    "submit",
    "validate",
    "xpath",
]
