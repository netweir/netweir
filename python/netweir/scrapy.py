"""A Scrapy download handler that fetches with Netweir, so a Scrapy project
sends its requests the way a browser does, from the TLS handshake to the
order of the headers, without changing anything else about the project:

    DOWNLOAD_HANDLERS = {
        "http": "netweir.scrapy.DownloadHandler",
        "https": "netweir.scrapy.DownloadHandler",
    }

Scrapy's middlewares keep their jobs: redirects, cookies, retries, proxies
(``meta["proxy"]``) and timeouts (``download_timeout``) work as they do
with Scrapy's own handler. The browser's own headers (User-Agent, Accept,
Accept-Language, Accept-Encoding and the client hints) come from the
Netweir profile, ``NETWEIR_PROFILE`` in the settings ("chrome" by
default); the rest of a request's headers are sent as Scrapy set them.
"""

from __future__ import annotations

import base64
import collections
import time
import urllib.parse
from typing import Any

from netweir._errors import FetchError
from netweir._native import Fetcher

#: Headers the browser profile always sends itself: Scrapy's versions of
#: them would give the crawler away.
_PROFILE_HEADERS = {
    "user-agent",
    "accept-encoding",
    "connection",
    "host",
    "content-length",
    "upgrade-insecure-requests",
    "te",
}
#: Headers the profile sends unless the spider chose its own: Scrapy's
#: defaults for them (DEFAULT_REQUEST_HEADERS) are dropped, anything else
#: is the spider asking for something, as an API's JSON.
_DEFAULTED_HEADERS = ("accept", "accept-language")
#: Response headers that describe the body as it travelled, not as it's
#: handed on: Netweir has already decompressed it.
_TRANSPORT_HEADERS = {"content-encoding", "content-length", "transfer-encoding"}
#: Clients kept at once, one per proxy, timeout and size limit: each holds
#: its own connections, and a rotating gateway with a session in its login
#: would otherwise add one for every request.
_MAX_CLIENTS = 64


class DownloadHandler:
    """Scrapy's download handler protocol, fetching with Netweir."""

    lazy = False

    def __init__(self, crawler: Any = None):
        settings = crawler.settings if crawler is not None else None

        def get(name: str, default: Any, kind: str = "get") -> Any:
            if settings is None:
                return default
            return getattr(settings, kind)(name, default)

        self._profile = get("NETWEIR_PROFILE", "chrome")
        self._timeout = float(get("DOWNLOAD_TIMEOUT", 180, "getfloat"))
        self._max_size = int(get("DOWNLOAD_MAXSIZE", 1024 * 1024 * 1024, "getint"))
        # Scrapy's own defaults, not the project's: a project that set its
        # own Accept or Accept-Language asked for it.
        from scrapy.settings import default_settings

        self._defaults = {
            k.lower(): str(v) for k, v in default_settings.DEFAULT_REQUEST_HEADERS.items()
        }
        self._auth_encoding = get("HTTPPROXY_AUTH_ENCODING", "latin-1")
        self._fetchers: collections.OrderedDict[tuple, Fetcher] = collections.OrderedDict()
        # A profile Netweir doesn't know raises here. Scrapy logs a handler
        # that fails to start and carries on, so the crawl's requests then
        # fail one by one, each saying why.
        self._fetcher(None, self._timeout, self._max_size)

    @classmethod
    def from_crawler(cls, crawler: Any) -> DownloadHandler:
        return cls(crawler)

    def _fetcher(self, proxy: str | None, timeout: float, max_size: int) -> Fetcher:
        key = (proxy, timeout, max_size)
        fetcher = self._fetchers.get(key)
        if fetcher is None:
            fetcher = Fetcher(
                self._profile,
                proxy,
                timeout,
                # 0 is Scrapy's "no limit".
                max_size=max_size or None,
                # Scrapy's cookie middleware keeps the cookies.
                cookies=False,
                # Scrapy's proxy middleware has decided already: no proxy
                # here means none, not the system's.
                system_proxy=False,
            )
            self._fetchers[key] = fetcher
            while len(self._fetchers) > _MAX_CLIENTS:
                self._fetchers.popitem(last=False)
        else:
            self._fetchers.move_to_end(key)
        return fetcher

    async def download_request(self, request: Any, spider: Any = None) -> Any:
        from scrapy.http import Headers
        from scrapy.responsetypes import responsetypes

        headers = self._headers(request)
        try:
            proxy = _proxy(request, headers, self._auth_encoding)
        except ValueError as e:
            raise _scrapy_error(FetchError(str(e), "invalid")) from e
        referer = _pop(headers, "referer")
        content_type = _pop(headers, "content-type")
        meta = request.meta
        chain = _chain(meta)
        if chain:
            # A hop of Scrapy's redirects: sent as the first request became
            # on the way here, as the browser would send it, not as a new
            # link from the page that redirected.
            method, kind, body, content_type, referer, _ = meta["netweir_first"]
        else:
            method = request.method.upper()
            body = bytes(request.body) if request.body else None
            kind = meta.get("netweir_kind") or _kind(method, body, content_type, headers)
            # Where this request's own redirects will start in
            # redirect_urls: anything before is history it copied.
            start = len(meta.get("redirect_urls") or [])
            meta["netweir_first"] = (method, kind, body, content_type, referer, start)
        timeout = float(meta.get("download_timeout", self._timeout))
        max_size = int(
            meta.get("download_maxsize", getattr(spider, "download_maxsize", self._max_size))
        )
        started = time.perf_counter()
        try:
            fetcher = self._fetcher(proxy, timeout, max_size)
            response = await fetcher.send(
                request.url,
                method,
                kind,
                body,
                content_type,
                referer,
                headers,
                follow=False,
                chain=chain,
            )
        except FetchError as e:
            raise _scrapy_error(e) from e
        except ValueError as e:
            # A proxy or header the client can't use.
            raise _scrapy_error(FetchError(str(e), "invalid")) from e
        # AutoThrottle adapts to this; without it, it does nothing.
        meta["download_latency"] = time.perf_counter() - started
        # The URL that redirected, as Scrapy will note it in redirect_urls:
        # only the request it makes for this redirect is a hop of the chain.
        if 300 <= response.status < 400 and dict(response.headers).get("location"):
            meta["netweir_from"] = request.url
        else:
            meta.pop("netweir_from", None)
        received = Headers()
        for name, value in response.headers:
            if name not in _TRANSPORT_HEADERS:
                received.appendlist(name, value)
        # As Scrapy's own handler gives it: without the fragment, which
        # never left the client.
        url = urllib.parse.urldefrag(response.url)[0]
        cls = responsetypes.from_args(headers=received, url=url, body=response.body)
        return cls(
            url=url,
            status=response.status,
            headers=received,
            body=response.body,
            request=request,
            protocol=response.version,
        )

    async def close(self) -> None:
        self._fetchers.clear()

    def _headers(self, request: Any) -> list[tuple[str, str]]:
        out = []
        for name, values in request.headers.items():
            key = name.decode("latin-1")
            lower = key.lower()
            if lower in _PROFILE_HEADERS:
                continue
            # One line, as a browser sends it, however many values Scrapy
            # holds for it.
            joiner = "; " if lower == "cookie" else ", "
            text = joiner.join(v.decode("latin-1") for v in values)
            if lower in _DEFAULTED_HEADERS and self._defaults.get(lower) == text:
                continue
            out.append((key, text))
        return out


def _kind(method: str, body: bytes | None, content_type: str | None, headers: list) -> str:
    """How a browser would send it: typing or following a link, submitting a
    form, or a page's script asking (a body that isn't a form's, or a GET
    that asks for JSON as an API's callers do)."""
    if (content_type or "").startswith(("application/x-www-form-urlencoded", "multipart/")):
        return "form"
    if method != "GET" or body is not None:
        return "fetch"
    accept = next((v for k, v in headers if k.lower() == "accept"), "")
    if "json" in accept.lower() or any(k.lower() == "x-requested-with" for k, _ in headers):
        return "fetch"
    return "navigate"


def _chain(meta: dict) -> list[tuple[int, str]] | None:
    """The redirects Scrapy followed to this request, each (status, the URL
    it came from), when it's the request a redirect this handler answered
    led to: redirect_urls ends with the URL that answered with the 3xx.
    Only this request's own redirects count, not history a request copied
    in its meta; a meta refresh starts afresh, as a browser's does."""
    first = meta.get("netweir_first")
    urls = meta.get("redirect_urls") or []
    reasons = meta.get("redirect_reasons") or []
    if first is None or not urls or len(urls) != len(reasons):
        return None
    if meta.get("netweir_from") != urls[-1]:
        return None
    start = first[5]
    own = list(zip(reasons[start:], urls[start:], strict=True))
    if not own or not all(isinstance(status, int) for status, _ in own):
        return None
    return own


def _pop(headers: list[tuple[str, str]], name: str) -> str | None:
    found = None
    for pair in list(headers):
        if pair[0].lower() == name:
            headers.remove(pair)
            found = found or pair[1]
    return found


def _proxy(request: Any, headers: list[tuple[str, str]], encoding: str) -> str | None:
    """The request's proxy, with its login put back into the URL: Scrapy's
    proxy middleware moves it into a Proxy-Authorization header, which the
    proxy itself must not be sent twice. Scrapy encodes the login with
    HTTPPROXY_AUTH_ENCODING (latin-1 unless set); Netweir sends a login as
    UTF-8, so it's read back to text first."""
    proxy = request.meta.get("proxy")
    login = next((v for k, v in headers if k.lower() == "proxy-authorization"), None)
    if not proxy:
        return None
    if login is None:
        return proxy
    if not login.lower().startswith("basic "):
        raise ValueError(
            "a Proxy-Authorization other than Basic can't be sent through Netweir's client;"
            " put the login in meta['proxy'] instead"
        )
    _pop(headers, "proxy-authorization")
    try:
        raw = base64.b64decode(login[6:], validate=True)
    except ValueError as e:
        raise ValueError("the Proxy-Authorization header isn't valid Basic") from e
    try:
        user, _, password = raw.decode(encoding).partition(":")
    except UnicodeDecodeError as e:
        raise ValueError(
            f"the proxy login isn't {encoding}, as HTTPPROXY_AUTH_ENCODING says"
        ) from e
    parts = urllib.parse.urlsplit(proxy)
    if "@" not in parts.netloc:
        quoted = f"{urllib.parse.quote(user, safe='')}:{urllib.parse.quote(password, safe='')}"
        proxy = urllib.parse.urlunsplit(parts._replace(netloc=f"{quoted}@{parts.netloc}"))
    return proxy


def _scrapy_error(error: FetchError) -> Exception:
    """The Scrapy exception for a failed fetch, so its retry middleware
    treats it as it would one from its own handler: a timeout or a lost
    connection is retried, a body over the size limit or a request that
    can't be sent isn't. Scrapy before 2.15 has none of these names:
    Python's own stand in, which it retries as OSErrors."""
    from scrapy import exceptions

    def kind(name: str, fallback: type[Exception]) -> type[Exception]:
        return getattr(exceptions, name, fallback)

    message = str(error)
    if error.kind == "timeout":
        return kind("DownloadTimeoutError", TimeoutError)(message)
    if error.kind == "connect":
        # Known limit: a name that didn't resolve is told from a refused
        # connection by the client's message, which a new version could
        # word differently; both are retried either way.
        if "dns" in message.lower() or "resolve" in message.lower():
            return kind("CannotResolveHostError", ConnectionError)(message)
        return kind("DownloadConnectionRefusedError", ConnectionRefusedError)(message)
    if error.kind == "body":
        return kind("ResponseDataLossError", ConnectionError)(message)
    if error.kind == "too_large":
        return kind("DownloadCancelledError", exceptions.IgnoreRequest)(message)
    if error.kind == "invalid":
        return exceptions.NotSupported(message)
    return kind("DownloadFailedError", OSError)(message)
