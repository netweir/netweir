from __future__ import annotations

import asyncio
import json as _json
import logging
import os
import threading
from collections.abc import Iterable, Mapping
from typing import Any
from urllib.parse import unquote, urlencode, urljoin, urlsplit, urlunsplit

from netweir import _jmespath, _track
from netweir._cookies import Cookies
from netweir._errors import Blocked
from netweir._form import Files, Form, read_form
from netweir._helpers import Article
from netweir._native import BrowserPage, Fetcher, Node, Response, Selection, _encode, _response
from netweir._request import Request

log = logging.getLogger("netweir")

Headers = Mapping[str, str] | Iterable[tuple[str, str]] | None
Params = Mapping[str, Any] | Iterable[tuple[str, Any]] | str | bytes | None
Auth = tuple[str, str] | None
#: True, False, or the path of a PEM file of CA certificates.
Verify = bool | str | os.PathLike


def _pairs(headers: Headers) -> list[tuple[str, str]]:
    if headers is None:
        return []
    if isinstance(headers, Mapping):
        return list(headers.items())
    return list(headers)


def with_params(url: str, params: Params) -> str:
    """``url`` with ``params`` added after its own query, as requests adds
    them: URL-encoded, a list value repeating its key, None left out (in a
    list too). A str or bytes is a query already, added as it is."""
    if not params:
        return url
    if isinstance(params, (str, bytes)):
        query = params.decode() if isinstance(params, bytes) else params
    else:
        pairs = params.items() if isinstance(params, Mapping) else params
        kept = []
        for k, v in pairs:
            if isinstance(v, (list, tuple)):
                v = [item for item in v if item is not None]
            if v is not None:
                kept.append((k, v))
        query = urlencode(kept, doseq=True)
    if not query:
        return url
    rest, hash_, fragment = url.partition("#")
    joint = "" if rest.endswith(("?", "&")) else "&" if "?" in rest else "?"
    return f"{rest}{joint}{query}{hash_}{fragment}"


def login_of(url: str, auth: Auth) -> tuple[str, Auth]:
    """``url`` without a login written into it (``https://user:pass@host``),
    and the login to send: ``auth`` if given, else the URL's, as requests
    reads one."""
    parts = urlsplit(url)
    if "@" not in parts.netloc:
        return url, auth
    url = urlunsplit(parts._replace(netloc=parts.netloc.rpartition("@")[2]))
    written = (unquote(parts.username or ""), unquote(parts.password or ""))
    # An empty one (https://@host/) is no login, as requests reads it.
    if auth is None and any(written):
        auth = written
    return url, auth


#: The longest timeout, in seconds, and redirect limit the engine can hold:
#: a Duration's whole seconds are a u64, and a redirect count a u32.
_MOST_SECONDS = 2.0**64
_MOST_REDIRECTS = 2**32 - 1


def seconds(value: Any, name: str = "timeout") -> float:
    """``value`` as a timeout in seconds, or why it can't be one."""
    try:
        value = float(value)
    except (TypeError, ValueError):
        raise ValueError(f"{name} must be a number of seconds, not {value!r}") from None
    if not 0 < value < _MOST_SECONDS:
        raise ValueError(
            f"{name} must be a positive number of seconds below {_MOST_SECONDS:.3g}, not {value!r}"
        )
    return value


def checked_auth(auth: Any) -> tuple[str, str] | None:
    """``auth`` as a (user, password) tuple, or why it can't be one."""
    if auth is None:
        return None
    if (
        not isinstance(auth, (tuple, list))
        or len(auth) != 2
        or not all(isinstance(part, str) for part in auth)
    ):
        raise TypeError("auth is a (user, password) pair of strings")
    return (auth[0], auth[1])


def _follows(allow_redirects: bool, follow_redirects: bool | None) -> bool:
    """Whether to follow redirects, from requests' name for it or httpx's.
    allow_redirects defaults to True, so only a False can disagree."""
    if follow_redirects is None:
        return allow_redirects
    if not allow_redirects and follow_redirects:
        raise ValueError(
            "allow_redirects and follow_redirects say different things: pass one of them"
        )
    return follow_redirects


def certificates(verify: Verify) -> tuple[bool, bytes | None]:
    """Whether to check certificates, and the CA certificates (PEM) to
    check them against in place of the bundled roots."""
    if isinstance(verify, bool):
        return verify, None
    if isinstance(verify, (str, os.PathLike)):
        with open(verify, "rb") as f:
            return True, f.read()
    raise TypeError(f"verify is True, False or a CA file's path, not {verify!r}")


def redirect_limit(n: int | None, name: str = "max_redirects") -> int | None:
    if n is not None and (
        isinstance(n, bool) or not isinstance(n, int) or not 0 <= n <= _MOST_REDIRECTS
    ):
        raise ValueError(f"{name} must be a whole number from 0 to {_MOST_REDIRECTS}")
    return n


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


def _encoded(
    text: str, url: str, status: int, version: str, pairs: list, old: bytes
) -> tuple[bytes, list]:
    """``text`` as bytes a response with headers ``pairs`` decodes back
    to it, and the headers to send with them."""
    encoding = _response(url, status, version, pairs, old).encoding
    # By the encoder a browser has, which is the decoder's other half:
    # Python's codecs for Big5, EUC-KR and others write bytes WHATWG's
    # decoders read as other characters.
    body = _encode(text, encoding)
    if body is not None:
        return body, pairs
    if not any(k == "content-type" for k, _ in pairs):
        # Says nothing of what it is, so the body is still looked at.
        pairs = [*pairs, ("content-type", "application/octet-stream")]
    return text.encode("utf-8"), [(k, _utf8(v) if k == "content-type" else v) for k, v in pairs]


def _utf8(content_type: str) -> str:
    """``content_type`` with its charset UTF-8."""
    params = [
        p for p in content_type.split(";")[1:] if p.split("=")[0].strip().lower() != "charset"
    ]
    return "; ".join(
        [content_type.split(";")[0].strip(), *[p.strip() for p in params], "charset=utf-8"]
    )


def _from_form(form: Form) -> tuple[str, dict[str, Any]]:
    url, body, content_type = form.encoded()
    if body is None:
        return url, {"method": "GET", "kind": "navigate"}
    return url, {"method": form.method, "kind": "form", "body": body, "content_type": content_type}


class Page:
    """A fetched page: the response, and the document parsed from it."""

    __slots__ = (
        "_base",
        "_lock",
        "_outcome",
        "_parse",
        "_response",
        "_root",
        "browser",
        "request",
    )

    def __init__(
        self,
        response: Response,
        request: Request | None = None,
        root: Node | None = None,
        parse: tuple[float | None, bool] = (None, False),
    ):
        self._response = response
        #: How the document is parsed when first read: a time budget in
        #: seconds (None for none) and whether XML is read in recover mode.
        #: A crawl gives its pages Settings.parse_timeout and xml_recover;
        #: a page made by hand has no budget.
        self._parse = parse
        #: So threads reading the page at once parse it once, and all get
        #: the same document.
        self._lock = threading.Lock()
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
    def steps(self) -> list[dict[str, Any]]:
        """For a page fetched in Chrome with steps, how each went: a dict
        with ``action``, ``status`` (``"ok"``, ``"failed"`` or
        ``"skipped"``), ``error``, ``elapsed`` (seconds) and ``result``:
        what an evaluate or a call returned, a screenshot's PNG bytes, the
        values a select_option chose, how many rounds a scroll loaded, the
        ``{"url", "status"}`` a wait_for_response or wait_for_navigation
        saw. Empty for any other page."""
        return getattr(self._response, "steps", None) or []

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
        """The parsed document. Parsed on first use: as XML when the
        Content-Type says it's XML (``text/xml``, ``application/xml``, RSS,
        Atom and other ``+xml`` types but XHTML's), as HTML otherwise.
        ``netweir.parse(page.text, type="xml")`` reads any page as XML."""
        if self._root is None:
            repaired = None
            with self._lock:
                if self._root is None:
                    timeout, recover = self._parse
                    self._root, repaired = self._response._parse_quietly(
                        timeout=timeout, recover=recover
                    )
            # Logged with the lock let go: a logging filter may read the page.
            if repaired is not None:
                logging.getLogger("netweir").warning("%s", repaired)
        return self._root

    @property
    def encoding(self) -> str:
        """The encoding ``text`` was decoded with (``"UTF-8"``,
        ``"windows-1252"``)."""
        return self._response.encoding

    def replace(
        self,
        *,
        url: str | None = None,
        status: int | None = None,
        headers: Headers = None,
        body: bytes | str | None = None,
    ) -> Page:
        """A copy of the page with some of its response changed, as
        Scrapy's ``response.replace()``: for a feed spider's
        ``adapt_response``, say. A ``str`` body is encoded as Scrapy
        encodes one, in the encoding the copy's text will be decoded with
        (``encoding``), so a Content-Type charset or an XML declaration in
        the body still tells the truth; if that encoding can't hold the
        text, it's UTF-8, and the Content-Type's charset says so. The copy
        answers the same request and is parsed afresh."""
        r = self._response
        pairs = r.headers if headers is None else [(k.lower(), v) for k, v in _pairs(headers)]
        if isinstance(body, str):
            body, pairs = _encoded(body, r.url, r.status, r.version, pairs, r.body)
        copy = Page(
            _response(
                r.url if url is None else url,
                r.status if status is None else status,
                r.version,
                pairs,
                r.body if body is None else bytes(body),
            ),
            self.request,
            parse=self._parse,
        )
        copy.browser = self.browser
        return copy

    def css(self, query: str, track: str | None = None) -> Selection:
        """A CSS query. ``track`` names the element so it's found again
        after a redesign breaks the query (see ``Selection.relocated``)."""
        if track is not None:
            return _track.tracked(
                self.root, "css", query, track, _track.site_of(self.url), self.url
            )
        return self.root.css(query)

    def xpath(
        self,
        query: str,
        track: str | None = None,
        namespaces: Mapping[str, str] | None = None,
        **variables: str | int | float | bool,
    ) -> Selection:
        """An XPath 1.0 query; keyword arguments bind ``$variables``, and
        ``namespaces`` (prefix to URI) the prefixes it uses besides those
        registered with ``register_namespace``. ``track`` works as in
        ``css``."""
        if track is not None:
            if variables:
                raise TypeError("track= can't be combined with $variables")
            if namespaces is not None:
                raise TypeError(
                    "track= can't be combined with namespaces=; use register_namespace()"
                )
            return _track.tracked(
                self.root, "xpath", query, track, _track.site_of(self.url), self.url
            )
        return self.root.xpath(query, namespaces=namespaces, **variables)

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

    def markdown(self, main_only: bool = False) -> str | None:
        """The page as Markdown, links and images absolute. With
        ``main_only=True``, only its main content (see ``main_content``),
        without the boilerplate inside it; None when there's no main
        content."""
        return self.root.markdown(self.urljoin(""), main_only)

    def article(self) -> Article:
        """The article on the page: its title, byline, publication date,
        lead image and language, from its structured data first and its
        HTML after, and the main content's text and HTML. Fields the page
        doesn't give are None."""
        return self.root.article(self.urljoin(""))

    def js_data(self, name: str | None = None) -> Any:
        """Data the page keeps in its ``<script>`` elements, read as
        JavaScript (unquoted keys, single quotes, trailing commas and
        comments are all fine).

        With ``name``, the value assigned to it (``window.__STATE__ =
        {...}``, ``var data = [...]``, ``"name": {...}``, or a JSON script
        with that id, like Next.js's ``__NEXT_DATA__``), or None. Without,
        a list of every object and array in every script, in order."""
        return self.root.js_data(name)

    def jmespath(self, expression: str) -> Any:
        """The body read as JSON and searched with a JMESPath expression,
        for JSON APIs. Needs the ``netweir[jmespath]`` extra."""
        return _jmespath.search_text(expression, self.text, "the page")

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


def _policy(
    url: str,
    params: Params,
    auth: Auth,
    allow_redirects: bool,
    follow_redirects: bool | None,
    max_redirects: int | None,
    timeout: float | None,
    fallback_auth: Auth = None,
) -> tuple[str, dict[str, Any]]:
    """The URL to send, and how: the keyword arguments of Fetcher.send
    for the policy a call asks for. ``auth`` wins over a login in the URL,
    which wins over ``fallback_auth`` (a Client's)."""
    url, auth = login_of(with_params(url, params), checked_auth(auth))
    if auth is None:
        auth = fallback_auth
    return url, {
        "follow": _follows(allow_redirects, follow_redirects),
        "max_redirects": redirect_limit(max_redirects),
        "timeout": None if timeout is None else seconds(timeout),
        "auth": None if auth is None else (auth[0], auth[1], None),
    }


class Client:
    """Fetches pages as one browser, reusing connections and cookies.

    ``profile`` names the browser to look like (``"chrome"`` is the newest
    Chrome). ``timeout`` covers each request, in seconds. ``verify`` is
    True, False (no certificate check), or the path of a PEM file of CA
    certificates to trust in place of the bundled ones. ``max_redirects``
    is how many redirects a request follows before it fails, and ``auth``
    a (user, password) login every call sends unless it has its own.
    ``storage_state`` starts the client with the cookies of a storage state:
    a path to a file ``storage_state()`` (here or in a browser context) or
    Playwright saved, or its dict. ``cookies`` is the jar.
    """

    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        max_size: int | None = None,
        *,
        verify: Verify = True,
        max_redirects: int = 20,
        auth: Auth = None,
        storage_state: str | os.PathLike[str] | dict[str, Any] | None = None,
    ):
        check, cas = certificates(verify)
        self._fetcher = Fetcher(
            profile,
            proxy,
            seconds(timeout),
            verify=check,
            max_size=max_size,
            ca_certificates=cas,
            max_redirects=redirect_limit(max_redirects),
        )
        self._auth = checked_auth(auth)
        self.cookies = Cookies(self._fetcher)
        if storage_state is not None:
            for why in self._fetcher.load_storage_state(storage_state):
                log.warning("a cookie in the storage state was left out: %s", why)

    def storage_state(self, path: str | os.PathLike[str] | None = None) -> dict[str, Any]:
        """The client's cookies as a storage state dict, in Playwright's
        form (``origins`` is empty: there's no localStorage over HTTP).
        With ``path``, also written there, readable by its owner alone."""
        return self._fetcher.storage_state(path)

    async def get(
        self,
        url: str,
        headers: Headers = None,
        raise_on_block: bool = True,
        referer: str | None = None,
        **policy: Any,
    ) -> Page:
        """Fetches one page. A block page raises Blocked unless
        ``raise_on_block=False``. With ``referer``, it's sent as following a
        link on that page. ``params``, ``auth``, ``allow_redirects`` (or
        ``follow_redirects``), ``max_redirects`` and ``timeout`` work as in
        ``request``."""
        return await self.request(
            "GET", url, headers=headers, raise_on_block=raise_on_block, referer=referer, **policy
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
        params: Params = None,
        auth: Auth = None,
        allow_redirects: bool = True,
        follow_redirects: bool | None = None,
        max_redirects: int | None = None,
        timeout: float | None = None,
    ) -> Page:
        """Sends any request. ``form`` is submitted as a form; ``json`` or a
        raw ``body`` as a page's script would send it. ``referer`` is the
        page it comes from.

        ``params`` are added to the URL's query. ``auth`` is a (user,
        password) login, sent as HTTP Basic to the URL's origin and not
        past a redirect that leaves it; a login written into the URL is
        used the same way. ``allow_redirects=False`` (or httpx's
        ``follow_redirects=False``) returns a redirect as the page.
        ``max_redirects`` and ``timeout`` replace the client's for this
        call."""
        how = payload(method, form, json, body)
        url, policy = _policy(
            url,
            params,
            auth,
            allow_redirects,
            follow_redirects,
            max_redirects,
            timeout,
            self._auth,
        )
        response = await self._fetcher.send(
            url, headers=_pairs(headers), referer=referer, **how, **policy
        )
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
            url,
            headers=_pairs(headers),
            referer=form.referer,
            auth=None if self._auth is None else (*self._auth, None),
            **how,
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
    **policy: Any,
) -> Page:
    """Fetches one page and waits for it. For many pages, use Client.

    A page from bot protection (a Cloudflare challenge, a DataDome captcha
    and the like) raises Blocked; pass ``raise_on_block=False`` to get it
    as a page, with ``page.blocked`` naming the vendor. With ``referer``,
    it's sent as following a link on that page. ``params``, ``auth``,
    ``allow_redirects`` (or ``follow_redirects``), ``max_redirects`` and
    ``verify`` work as in ``request``."""
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
        **policy,
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
    params: Params = None,
    auth: Auth = None,
    allow_redirects: bool = True,
    follow_redirects: bool | None = None,
    max_redirects: int = 20,
    verify: Verify = True,
) -> Page:
    """Sends any request and waits for the page. ``form`` is submitted as
    a form (URL-encoded); ``json`` or a raw ``body`` as a page's script
    would send it. ``referer`` is the page the request comes from; a form
    or a script without one comes from the root of the target's site.

    ``params`` are added to the URL's query, as requests adds them.
    ``auth`` is a (user, password) login, sent as HTTP Basic to the URL's
    origin and not past a redirect that leaves it; a login written into the
    URL is used the same way. ``allow_redirects=False`` (or httpx's
    ``follow_redirects=False``) returns a redirect as the page; more than
    ``max_redirects`` is FetchError("too_many_redirects"). ``verify`` is
    True, False (no certificate check), or the path of a PEM file of CA
    certificates to trust in place of the bundled ones."""
    how = payload(method, form, json, body)
    url, policy = _policy(url, params, auth, allow_redirects, follow_redirects, max_redirects, None)
    check, cas = certificates(verify)
    fetcher = Fetcher(
        profile, proxy, seconds(timeout), verify=check, max_size=max_size, ca_certificates=cas
    )
    response = fetcher.send_blocking(url, headers=_pairs(headers), referer=referer, **how, **policy)
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
