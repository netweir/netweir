from __future__ import annotations

import asyncio
import json as _json
from collections.abc import Iterable, Mapping
from typing import Any
from urllib.parse import urlencode, urljoin

from netweir import _track
from netweir._errors import Blocked
from netweir._form import Files, Form, read_form
from netweir._native import BrowserPage, Fetcher, Node, Response, Selection
from netweir._request import Request

Headers = Mapping[str, str] | Iterable[tuple[str, str]] | None


def _pairs(headers: Headers) -> list[tuple[str, str]]:
    if headers is None:
        return []
    if isinstance(headers, Mapping):
        return list(headers.items())
    return list(headers)


def payload(
    method: str, form: Any = None, json: Any = None, body: bytes | str | None = None
) -> dict[str, Any]:
    """How to send a request with one of ``form``, ``json`` or ``body``:
    a form is a form submission, URL-encoded; JSON and a raw body are what
    a page's script sends; none of them, a navigation."""
    given = [form is not None, json is not None, body is not None]
    if sum(given) > 1:
        raise ValueError("pass one of form=, json= and body=, not several")
    method = method.upper()
    if any(given) and method in ("GET", "HEAD"):
        raise ValueError(f"a {method} carries no body: use POST, or put the data in the URL")
    if form is not None:
        pairs = form.items() if isinstance(form, Mapping) else form
        encoded = urlencode(list(pairs), doseq=True).encode()
        return {"method": method, "kind": "form", "body": encoded}
    if json is not None:
        # As a script's JSON.stringify writes it: no spaces, text as is.
        encoded = _json.dumps(json, separators=(",", ":"), ensure_ascii=False).encode()
        return {
            "method": method,
            "kind": "fetch",
            "body": encoded,
            "content_type": "application/json",
        }
    if body is not None:
        encoded = body.encode() if isinstance(body, str) else bytes(body)
        return {"method": method, "kind": "fetch", "body": encoded}
    if method == "GET":
        return {"method": method, "kind": "navigate"}
    return {"method": method, "kind": "fetch"}


def _from_form(form: Form) -> tuple[str, dict[str, Any]]:
    url, body, content_type = form.encoded()
    if body is None:
        return url, {"method": "GET", "kind": "navigate"}
    return url, {"method": form.method, "kind": "form", "body": body, "content_type": content_type}


class Page:
    """A fetched page: the response, and the document parsed from it."""

    __slots__ = ("_base", "_outcome", "_response", "_root", "browser", "request")

    def __init__(
        self, response: Response, request: Request | None = None, root: Node | None = None
    ):
        self._response = response
        #: Parsed on first use, unless the crawl engine already parsed it.
        self._root: Node | None = root
        self._base: str | None = None
        self._outcome: tuple | None = None
        #: In a crawl, the Request this page answers.
        self.request = request
        #: For a page fetched in Chrome, the BrowserPage, open until the
        #: callback returns; otherwise None.
        self.browser: BrowserPage | None = None

    @property
    def url(self) -> str:
        """The final URL, after redirects."""
        return self._response.url

    @property
    def status(self) -> int:
        return self._response.status

    def _classified(self) -> tuple:
        if self._outcome is None:
            self._outcome = self._response.classify()
        return self._outcome

    @property
    def outcome(self) -> str:
        """What the response means: ``"ok"``, ``"blocked"`` (by bot
        protection), ``"throttled"`` (429, or 503 with Retry-After),
        ``"payment_required"`` (402) or ``"http_error"``."""
        return self._classified()[0]

    @property
    def blocked(self) -> str | None:
        """The bot-protection vendor that blocked this request, or None."""
        return self._classified()[1]

    @property
    def version(self) -> str:
        return self._response.version

    @property
    def raw_headers(self) -> list[tuple[str, str]]:
        """Every header as (name, value), in the order received, repeats
        included. Names are lowercase."""
        return self._response.headers

    @property
    def headers(self) -> dict[str, str]:
        """Lowercase names. A header sent more than once has its values
        joined with ", ", except set-cookie, which keeps the last; use
        raw_headers for all of them."""
        out: dict[str, str] = {}
        for name, value in self._response.headers:
            if name in out and name != "set-cookie":
                out[name] = f"{out[name]}, {value}"
            else:
                out[name] = value
        return out

    @property
    def body(self) -> bytes:
        return self._response.body

    @property
    def text(self) -> str:
        """The body decoded the way a browser would choose the encoding."""
        return self._response.text()

    @property
    def root(self) -> Node:
        """The parsed document. Parsed on first use."""
        if self._root is None:
            self._root = self._response.parse()
        return self._root

    def css(self, query: str, track: str | None = None) -> Selection:
        """A CSS query. ``track`` names the element so it's found again
        after a redesign breaks the query (see ``Selection.relocated``)."""
        if track is not None:
            return _track.tracked(
                self.root, "css", query, track, _track.site_of(self.url), self.url
            )
        return self.root.css(query)

    def xpath(
        self, query: str, track: str | None = None, **variables: str | int | float | bool
    ) -> Selection:
        """An XPath 1.0 query; keyword arguments bind ``$variables``.
        ``track`` works as in ``css``."""
        if track is not None:
            if variables:
                raise TypeError("track= can't be combined with $variables")
            return _track.tracked(
                self.root, "xpath", query, track, _track.site_of(self.url), self.url
            )
        return self.root.xpath(query, **variables)

    @property
    def depth(self) -> int:
        """Links followed from a start page to get here; 0 outside a crawl."""
        return self.request.depth if self.request is not None else 0

    @property
    def meta(self) -> dict:
        """The meta of the Request this page answers; empty outside a crawl."""
        return self.request.meta if self.request is not None else {}

    def urljoin(self, url: str) -> str:
        """``url`` made absolute, against the page's ``<base href>`` if it
        has one and its URL otherwise, as a browser resolves links."""
        if self._base is None:
            href = self.root.css("base[href]::attr(href)").get()
            self._base = urljoin(self.url, href) if href else self.url
        return urljoin(self._base, str(url))

    def follow(self, url: str, callback=None, **kwargs) -> Request:
        """A Request for ``url``, resolved as ``urljoin`` does, sent as
        following a link on this page."""
        kwargs.setdefault("referer", self.url)
        return Request(self.urljoin(url), callback=callback, **kwargs)

    def follow_all(self, urls, callback=None, **kwargs) -> list[Request]:
        """``follow`` for every URL in ``urls`` (a list or a Selection)."""
        return [self.follow(u, callback, **kwargs) for u in urls]

    def metadata(self) -> dict:
        """The structured data the page carries about itself: a dict with
        ``json_ld`` (a list of the JSON-LD blocks), ``microdata`` (a list
        of items, each with ``type``, ``id`` and ``properties``),
        ``opengraph``, ``twitter`` and ``dublin_core`` (dicts of their meta
        tags; a key given more than once has a list). Relative URLs are
        resolved against the page."""
        return self.root.metadata(self.urljoin(""))

    def js_data(self, name: str | None = None) -> Any:
        """Data the page keeps in its ``<script>`` elements, read as
        JavaScript (unquoted keys, single quotes, trailing commas and
        comments are all fine).

        With ``name``, the value assigned to it (``window.__STATE__ =
        {...}``, ``var data = [...]``, ``"name": {...}``, or a JSON script
        with that id, like Next.js's ``__NEXT_DATA__``), or None. Without,
        a list of every object and array in every script, in order."""
        return self.root.js_data(name)

    def form(
        self,
        query: str | None = None,
        *,
        data: Mapping[str, Any] | Iterable[tuple[str, Any]] | None = None,
        files: Files | None = None,
        click: str | bool | None = None,
        formid: str | None = None,
        formname: str | None = None,
        formnumber: int = 0,
    ) -> Form:
        """A form on this page, ready to submit as a browser would.

        Pick it with ``query`` (a CSS selector, or an XPath), ``formid``,
        ``formname`` or ``formnumber``; the first form otherwise. Its fields
        are what the browser would send: named, enabled controls, checked
        boxes and radios, selected options. ``data`` replaces or adds
        fields, and ``files`` does the same for file inputs: a path, a
        ``(filename, bytes)`` or ``(filename, bytes, content_type)`` tuple,
        or a list of them. ``click`` names the submit button pressed (the first, by
        default; ``False`` for none)."""
        return read_form(
            self.root,
            self.url,
            self.urljoin(""),
            query,
            data=data,
            files=files,
            click=click,
            formid=formid,
            formname=formname,
            formnumber=formnumber,
        )

    def __getattr__(self, name: str):
        # Everything else a Node offers (find_all, select, get_text ...)
        # works on the page's document.
        if name.startswith("_"):
            raise AttributeError(name)
        return getattr(self.root, name)

    def __repr__(self) -> str:
        return f"<Page {self.status} {self.url}>"


class Client:
    """Fetches pages as one browser, reusing connections and cookies.

    ``profile`` names the browser to look like (``"chrome"`` is the newest
    Chrome). ``timeout`` covers a whole request, in seconds.
    """

    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        max_size: int | None = None,
    ):
        self._fetcher = Fetcher(profile, proxy, timeout, max_size=max_size)

    async def get(
        self,
        url: str,
        headers: Headers = None,
        raise_on_block: bool = True,
        referer: str | None = None,
    ) -> Page:
        """Fetches one page. A block page raises Blocked unless
        ``raise_on_block=False``. With ``referer``, it's sent as following a
        link on that page."""
        return await self.request(
            "GET", url, headers=headers, raise_on_block=raise_on_block, referer=referer
        )

    async def request(
        self,
        method: str,
        url: str,
        *,
        form: Any = None,
        json: Any = None,
        body: bytes | str | None = None,
        headers: Headers = None,
        referer: str | None = None,
        raise_on_block: bool = True,
    ) -> Page:
        """Sends any request. ``form`` is submitted as a form; ``json`` or a
        raw ``body`` as a page's script would send it. ``referer`` is the
        page it comes from."""
        how = payload(method, form, json, body)
        response = await self._fetcher.send(url, headers=_pairs(headers), referer=referer, **how)
        return _checked(Page(response), raise_on_block)

    async def post(self, url: str, **kwargs: Any) -> Page:
        """``request("POST", url, ...)``."""
        return await self.request("POST", url, **kwargs)

    async def submit(
        self, form: Form, headers: Headers = None, raise_on_block: bool = True
    ) -> Page:
        """Submits a form read with ``page.form()``, from its page."""
        url, how = _from_form(form)
        response = await self._fetcher.send(
            url, headers=_pairs(headers), referer=form.referer, **how
        )
        return _checked(Page(response), raise_on_block)

    async def get_many(
        self, urls: Iterable[str], headers: Headers = None, return_exceptions: bool = False
    ) -> list[Page | BaseException]:
        """Fetches every URL concurrently and returns pages in the same
        order. With ``return_exceptions=True`` a failed URL gives its
        FetchError in place of a page instead of raising."""
        pairs = _pairs(headers)
        return await asyncio.gather(
            *(self.get(url, pairs) for url in urls), return_exceptions=return_exceptions
        )

    async def __aenter__(self) -> Client:
        return self

    async def __aexit__(self, *exc: object) -> None:
        pass


def get(
    url: str,
    profile: str = "chrome",
    headers: Headers = None,
    proxy: str | None = None,
    timeout: float = 30.0,
    raise_on_block: bool = True,
    referer: str | None = None,
    max_size: int | None = None,
) -> Page:
    """Fetches one page and waits for it. For many pages, use Client.

    A page from bot protection (a Cloudflare challenge, a DataDome captcha
    and the like) raises Blocked; pass ``raise_on_block=False`` to get it
    as a page, with ``page.blocked`` naming the vendor. With ``referer``,
    it's sent as following a link on that page."""
    return request(
        "GET",
        url,
        profile=profile,
        headers=headers,
        proxy=proxy,
        timeout=timeout,
        raise_on_block=raise_on_block,
        referer=referer,
        max_size=max_size,
    )


def request(
    method: str,
    url: str,
    *,
    form: Any = None,
    json: Any = None,
    body: bytes | str | None = None,
    profile: str = "chrome",
    headers: Headers = None,
    proxy: str | None = None,
    timeout: float = 30.0,
    raise_on_block: bool = True,
    referer: str | None = None,
    max_size: int | None = None,
) -> Page:
    """Sends any request and waits for the page. ``form`` is submitted as
    a form (URL-encoded); ``json`` or a raw ``body`` as a page's script
    would send it. ``referer`` is the page the request comes from; a form
    or a script without one comes from the root of the target's site."""
    how = payload(method, form, json, body)
    fetcher = Fetcher(profile, proxy, timeout, max_size=max_size)
    response = fetcher.send_blocking(url, headers=_pairs(headers), referer=referer, **how)
    return _checked(Page(response), raise_on_block)


def post(url: str, **kwargs: Any) -> Page:
    """``request("POST", url, ...)``: ``form=``, ``json=`` or ``body=``."""
    return request("POST", url, **kwargs)


def submit(
    form: Form,
    profile: str = "chrome",
    headers: Headers = None,
    proxy: str | None = None,
    timeout: float = 30.0,
    raise_on_block: bool = True,
    max_size: int | None = None,
) -> Page:
    """Submits a form read with ``page.form()``, from its page. Use a
    Client's ``submit`` to keep the cookies the page was fetched with."""
    url, how = _from_form(form)
    fetcher = Fetcher(profile, proxy, timeout, max_size=max_size)
    response = fetcher.send_blocking(url, headers=_pairs(headers), referer=form.referer, **how)
    return _checked(Page(response), raise_on_block)


def _checked(page: Page, raise_on_block: bool) -> Page:
    if raise_on_block and page.blocked is not None:
        raise Blocked(page.blocked, page._classified()[2], page)
    return page
