import os
import re
from collections.abc import Awaitable, Callable, Iterator
from typing import Any, Literal, TypeVar, overload

from netweir._errors import FetchError

_D = TypeVar("_D")

__version__: str

class SelectorError(ValueError): ...
class XPathError(ValueError): ...
class ParseTimeout(TimeoutError): ...

Filter = (
    str
    | list[str]
    | tuple[str, ...]
    | set[str]
    | bool
    | re.Pattern[str]
    | Callable[..., object]
    | None
)

class Node:
    # parsel-style queries
    def css(self, query: str, track: str | None = None) -> Selection: ...
    def xpath(
        self, query: str, track: str | None = None, **variables: str | int | float | bool
    ) -> Selection: ...
    def tracked(
        self,
        kind: Literal["css", "xpath"],
        query: str,
        name: str,
        store: TrackStore,
        site: str,
        threshold: float,
    ) -> Selection: ...
    # Beautiful Soup style search
    def find(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        recursive: bool = True,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        recursive: bool = True,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_parent(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_parents(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_next_sibling(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_next_siblings(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_previous_sibling(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_previous_siblings(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_next(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all_next(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_previous(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all_previous(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def select(self, css: str) -> list[Node]: ...
    def select_one(self, css: str) -> Node | None: ...
    # what the node is
    @property
    def kind(self) -> Literal["element", "text", "comment", "document", "doctype", "other"]: ...
    @property
    def tag(self) -> str | None: ...
    @property
    def name(self) -> str | None: ...
    @property
    def text(self) -> str: ...
    @property
    def html(self) -> str: ...
    @property
    def string(self) -> str | None: ...
    def metadata(self, base_url: str | None = None) -> dict[str, Any]: ...
    def get_text(self, separator: str = "", strip: bool = False) -> str: ...
    # attributes
    @property
    def attrs(self) -> dict[str, str]: ...
    def attr(self, name: str) -> str | None: ...
    @overload
    def get(self, name: str) -> str | None: ...
    @overload
    def get(self, name: str, default: object) -> object: ...
    def __getitem__(self, name: str) -> str: ...
    # navigation
    @property
    def parent(self) -> Node | None: ...
    @property
    def parents(self) -> list[Node]: ...
    @property
    def children(self) -> list[Node]: ...
    @property
    def contents(self) -> list[Node]: ...
    @property
    def descendants(self) -> list[Node]: ...
    @property
    def next_sibling(self) -> Node | None: ...
    @property
    def previous_sibling(self) -> Node | None: ...
    @property
    def next_siblings(self) -> list[Node]: ...
    @property
    def previous_siblings(self) -> list[Node]: ...
    @property
    def next_element(self) -> Node | None: ...
    @property
    def previous_element(self) -> Node | None: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

class Selection:
    @property
    def relocated(self) -> bool: ...
    @property
    def score(self) -> float | None: ...
    @overload
    def get(self) -> str | None: ...
    @overload
    def get(self, default: _D) -> str | _D: ...
    def getall(self) -> list[str]: ...
    def extract(self) -> list[str]: ...
    @overload
    def extract_first(self) -> str | None: ...
    @overload
    def extract_first(self, default: _D) -> str | _D: ...
    def css(self, query: str) -> Selection: ...
    def xpath(self, query: str, **variables: str | int | float | bool) -> Selection: ...
    def re(self, pattern: str | re.Pattern[str], replace_entities: bool = True) -> list[str]: ...
    @overload
    def re_first(
        self, pattern: str | re.Pattern[str], *, replace_entities: bool = True
    ) -> str | None: ...
    @overload
    def re_first(
        self, pattern: str | re.Pattern[str], default: _D, replace_entities: bool = True
    ) -> str | _D: ...
    @property
    def attrib(self) -> dict[str, str]: ...
    def __len__(self) -> int: ...
    @overload
    def __getitem__(self, index: int) -> Node | str: ...
    @overload
    def __getitem__(self, index: slice) -> Selection: ...
    def __iter__(self) -> Iterator[Node | str]: ...

def _response(
    url: str, status: int, version: str, headers: list[tuple[str, str]], body: bytes
) -> Response: ...

class Response:
    @property
    def url(self) -> str: ...
    @property
    def status(self) -> int: ...
    @property
    def version(self) -> str: ...
    @property
    def headers(self) -> list[tuple[str, str]]: ...
    @property
    def body(self) -> bytes: ...
    def text(self) -> str: ...
    def parse(self, timeout: float | None = None) -> Node: ...
    def classify(
        self,
    ) -> tuple[
        Literal["ok", "blocked", "throttled", "payment_required", "http_error"],
        str | None,
        Literal["challenge", "captcha", "block", "rate_limit"] | None,
        float | None,
        str | None,
    ]: ...

class Fetcher:
    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        verify: bool = True,
        max_size: int | None = None,
        cookies: bool = True,
        system_proxy: bool = True,
    ) -> None: ...
    async def get(self, url: str, headers: list[tuple[str, str]] | None = None) -> Response: ...
    def get_blocking(self, url: str, headers: list[tuple[str, str]] | None = None) -> Response: ...
    async def send(
        self,
        url: str,
        method: str = "GET",
        kind: Literal["navigate", "form", "fetch"] = "navigate",
        body: bytes | None = None,
        content_type: str | None = None,
        referer: str | None = None,
        headers: list[tuple[str, str]] | None = None,
        follow: bool = True,
        chain: list[tuple[int, str]] | None = None,
    ) -> Response: ...
    def send_blocking(
        self,
        url: str,
        method: str = "GET",
        kind: Literal["navigate", "form", "fetch"] = "navigate",
        body: bytes | None = None,
        content_type: str | None = None,
        referer: str | None = None,
        headers: list[tuple[str, str]] | None = None,
    ) -> Response: ...

class Crawler:
    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        verify: bool = True,
        concurrency: int = 64,
        per_domain: int = 8,
        obey_robots: bool = True,
        robots_agent: str = "netweir",
        obey_tdmrep: bool = True,
        throttle: bool = True,
        start_delay: float = 1.0,
        min_delay: float = 0.0,
        max_delay: float = 60.0,
        target_concurrency: float = 1.0,
        max_depth: int | None = None,
        max_pages_per_domain: int | None = None,
        max_pages: int | None = None,
        retries: int = 3,
        backoff_base: float = 1.0,
        backoff_max: float = 60.0,
        proxies: list[str] | None = None,
        proxy_rotation: Literal["on_block", "per_request", "per_host"] = "on_block",
        proxy_failures: int = 3,
        proxy_cooldown: float = 60.0,
        breaker_window: int = 50,
        breaker_ratio: float = 0.3,
        breaker_pause: float = 300.0,
        checkpoint: str | None = None,
        browser: str = "off",
        browser_pages: int = 4,
        max_response_size: int | None = None,
        allowed_domains: list[str] | None = None,
        cache: str | None = None,
        cache_expiry: float | None = None,
        deltafetch: str | None = None,
        deltafetch_reset: bool = False,
        warc: str | None = None,
        har: str | None = None,
    ) -> None: ...
    def add_rule(
        self,
        kind: Literal["css", "xpath"],
        query: str,
        item: ItemSpec | None = None,
        follow: bool = True,
        to_python: bool = False,
        priority: int = 0,
        allow: list[str] | None = None,
        deny: list[str] | None = None,
        allow_domains: list[str] | None = None,
        deny_domains: list[str] | None = None,
    ) -> int: ...
    def submit(
        self,
        id: int,
        url: str,
        priority: int = 0,
        headers: list[tuple[str, str]] | None = None,
        dont_filter: bool = False,
        apply_rules: bool = False,
        to_python: bool = True,
        depth: int = 0,
        payload: str = "{}",
        row: int | None = None,
        browser: bool = False,
        method: str = "GET",
        kind: Literal["navigate", "form", "fetch"] = "navigate",
        body: bytes | None = None,
        content_type: str | None = None,
        referer: str | None = None,
        retry_post: bool = False,
        proxy: str | None = None,
        delta_key: bytes | None = None,
        delta: bool = True,
    ) -> Literal[
        "queued",
        "duplicate",
        "invalid",
        "too_deep",
        "trap",
        "domain_full",
        "offsite",
        "bad_proxy",
        "delta",
    ]: ...
    def resume(self) -> dict[str, Any] | None: ...
    def ack(self) -> None: ...
    def set_tracks(self, store: TrackStore, threshold: float) -> None: ...
    def item_done(self, id: str) -> None: ...
    def settle(self, items: list[str], state: str) -> None: ...
    @staticmethod
    def delta_key(
        method: str, url: str, body: bytes | None = None, delta_key: bytes | None = None
    ) -> bytes | None: ...
    def produced(
        self, keys: list[bytes], files: list[tuple[str, int, str | None, bool]]
    ) -> None: ...
    def delta_files(self) -> dict[str, tuple[int, str | None, bool]] | None: ...
    def delta_was_empty(self) -> bool | None: ...
    def flush(self) -> None: ...
    def save_counters(self, json: str) -> None: ...
    def close(self) -> None: ...
    def close_browser(self) -> Awaitable[None]: ...
    async def next(
        self, max: int = 256
    ) -> list[
        tuple[Literal["fetched"], int, Response, Node | None, BrowserPage | None]
        | tuple[Literal["failed"], int, FetchError]
        | tuple[Literal["dropped"], int, Literal["robots", "tdm"]]
        | tuple[Literal["handled"], int]
        | tuple[
            Literal["item"],
            int,
            dict[str, Any],
            list[tuple[str, str, str]],
            str,
            list[tuple[str, str, float | None]],
            str | None,
        ]
        | tuple[Literal["ruled"], int, str, Response, Node | None, int]
        | tuple[Literal["rule_failed"], int, str, FetchError]
        | tuple[Literal["rule_dropped"], int, str, Literal["robots", "tdm"]]
        | tuple[Literal["page_error"], str, str]
        | tuple[Literal["ignored"], str, str]
        | tuple[Literal["blocked"], int, int | None, str, str, str, Response]
        | tuple[Literal["paused"], str, float]
        | tuple[Literal["warning"], str]
    ]: ...
    def stats(self) -> dict[str, int]: ...
    def proxy_stats(self) -> dict[str, dict[str, int]]: ...
    def start_archive(self) -> None: ...

class ItemSpec:
    def __init__(
        self,
        name: str,
        fields: list[
            tuple[
                str,
                Literal["css", "xpath"],
                str,
                str | None,
                bool,
                bool,
                Literal["text", "int", "float", "bool"],
                object,
                str | None,
            ]
        ],
    ) -> None: ...
    def extract(
        self,
        node: Node,
        store: TrackStore | None = None,
        site: str = "",
        threshold: float = 0.75,
    ) -> tuple[dict[str, Any], list[tuple[str, str, str]], list[tuple[str, str, float | None]]]: ...

class TrackStore:
    def __init__(self, path: str) -> None: ...
    def get(self, site: str, name: str) -> str | None: ...
    def close(self) -> None: ...

def similarity(node: Node, kind: str, query: str, fingerprint: str) -> float | None: ...

class JsonlWriter:
    def __init__(self, path: str, append: bool = False) -> None: ...
    def write(self, item: Any) -> None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class JsonWriter:
    @staticmethod
    def resume_length(path: str) -> int: ...
    def __init__(self, path: str, append: bool = False) -> None: ...
    def write(self, item: Any) -> None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class XmlWriter:
    @staticmethod
    def resume_length(path: str, root: str = "items") -> int: ...
    def __init__(
        self, path: str, root: str = "items", item: str = "item", append: bool = False
    ) -> None: ...
    def write(self, item: Any) -> None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class CsvWriter:
    def __init__(
        self, path: str, fields: list[str] | None = None, append: bool = False
    ) -> None: ...
    def write(self, item: Any) -> list[str] | None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class ParquetWriter:
    def __init__(self, path: str) -> None: ...
    def write(self, item: Any) -> None: ...
    def close(self) -> list[tuple[str, int, int]]: ...

def parse(html: str, timeout: float | None = None) -> Node: ...
def fingerprint(url: str) -> str | None: ...
def launch(
    executable: str | os.PathLike[str] | None = None,
    headless: bool = True,
    args: list[str] = ...,
    timeout: float = 30.0,
    proxy: str | None = None,
    connect: str | None = None,
) -> Awaitable[Browser]: ...
def find_chrome() -> str: ...
def install_chrome(
    headless_shell: bool = False, index: str = ..., home: str | os.PathLike[str] | None = None
) -> tuple[str, str, bool]: ...

class Browser:
    @property
    def version(self) -> str: ...
    @property
    def closed(self) -> bool: ...
    def new_page(self) -> Awaitable[BrowserPage]: ...
    def new_context(self) -> Awaitable[BrowserContext]: ...
    def methods_sent(self) -> list[str]: ...
    def close(self) -> Awaitable[None]: ...
    def __aenter__(self) -> Awaitable[Browser]: ...
    def __aexit__(self, *exc: object) -> Awaitable[None]: ...

class BrowserContext:
    def new_page(self) -> Awaitable[BrowserPage]: ...
    def cookies(self) -> Awaitable[list[dict[str, Any]]]: ...
    def set_cookies(self, cookies: list[dict[str, Any]]) -> Awaitable[None]: ...
    def close(self) -> Awaitable[None]: ...
    def __aenter__(self) -> Awaitable[BrowserContext]: ...
    def __aexit__(self, *exc: object) -> Awaitable[None]: ...

class BrowserPage:
    @property
    def url(self) -> str: ...
    @property
    def closed(self) -> bool: ...
    def goto(
        self, url: str, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse]: ...
    def wait_for_navigation(
        self, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse]: ...
    def content(self) -> Awaitable[str]: ...
    def parse(self) -> Awaitable[Node]: ...
    def title(self) -> Awaitable[str]: ...
    def evaluate(self, js: str) -> Awaitable[Any]: ...
    def click(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def fill(self, selector: str, text: str, timeout: float | None = None) -> Awaitable[None]: ...
    def press(self, key: str) -> Awaitable[None]: ...
    def wait_for(
        self, selector: str, state: str = "visible", timeout: float | None = None
    ) -> Awaitable[None]: ...
    def screenshot(
        self, path: str | os.PathLike[str] | None = None, full_page: bool = False
    ) -> Awaitable[bytes]: ...
    def cookies(self) -> Awaitable[list[dict[str, Any]]]: ...
    def set_cookies(self, cookies: list[dict[str, Any]]) -> Awaitable[None]: ...
    def close(self) -> Awaitable[None]: ...
    def __aenter__(self) -> Awaitable[BrowserPage]: ...
    def __aexit__(self, *exc: object) -> Awaitable[None]: ...

class BrowserResponse:
    @property
    def url(self) -> str: ...
    @property
    def status(self) -> int: ...
    @property
    def headers(self) -> list[tuple[str, str]]: ...

def _sitemap(body: bytes, max: int) -> tuple[Literal["urlset", "index"], list[dict[str, Any]]]: ...
def _robots_sitemaps(body: str) -> list[str]: ...
def _offsite(url: str, allowed: list[str]) -> bool: ...
