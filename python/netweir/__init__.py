"""Fast web scraping with a Rust engine."""

from netweir import export, loader, repair, signals
from netweir._browser import browser
from netweir._crawl import Settings, Spider
from netweir._errors import (
    Blocked,
    BrowserError,
    BrowserTimeout,
    CloseSpider,
    DontCloseSpider,
    DropItem,
    FetchError,
    IgnoreRequest,
    NotConfigured,
    QueueError,
)
from netweir._fetch import Client, Page, get, post, request, submit
from netweir._files import files, images
from netweir._form import File, Form
from netweir._helpers import Article, Price, parse_date, parse_price
from netweir._jmespath import jmespath
from netweir._monitor import Checks, validate
from netweir._native import (
    Browser,
    BrowserContext,
    BrowserFrame,
    BrowserPage,
    BrowserRequest,
    BrowserResponse,
    Node,
    ParseTimeout,
    Queue,
    Route,
    Selection,
    SelectorError,
    XPathError,
    __version__,
    js_data,
    parse,
)
from netweir._request import Request
from netweir._rules import Follow, Item, css, xpath
from netweir._stats import Stats

__all__ = [
    "Article",
    "Blocked",
    "Browser",
    "BrowserContext",
    "BrowserError",
    "BrowserFrame",
    "BrowserPage",
    "BrowserRequest",
    "BrowserResponse",
    "BrowserTimeout",
    "Checks",
    "Client",
    "CloseSpider",
    "DontCloseSpider",
    "DropItem",
    "Follow",
    "File",
    "Form",
    "IgnoreRequest",
    "Item",
    "Request",
    "Settings",
    "Spider",
    "Stats",
    "FetchError",
    "Node",
    "NotConfigured",
    "Page",
    "ParseTimeout",
    "Price",
    "Queue",
    "QueueError",
    "Route",
    "Selection",
    "SelectorError",
    "XPathError",
    "__version__",
    "browser",
    "css",
    "export",
    "files",
    "get",
    "images",
    "jmespath",
    "loader",
    "js_data",
    "parse",
    "parse_date",
    "parse_price",
    "post",
    "repair",
    "request",
    "signals",
    "submit",
    "validate",
    "xpath",
]
