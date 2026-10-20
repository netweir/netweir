from __future__ import annotations

import dataclasses
import logging
from collections.abc import Callable, Iterable
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from netweir._fetch import Headers


@dataclasses.dataclass
class Request:
    """A page to fetch in a crawl, and what to do with it.

    ``callback`` is a function, or the name of a spider method, called with
    the Page; it defaults to the spider's ``parse``. ``errback`` is called
    with the Request and the FetchError when no response arrives. ``meta``
    travels with the request and comes back as ``page.meta``. ``depth`` is
    set by the crawl: 0 for a start request, one more than its page's for
    a request a callback yields. ``browser=True`` fetches it in Chrome,
    and the callback's ``page.browser`` is the page, open until the
    callback returns.

    ``steps`` is what to do on the page in Chrome before the callback
    sees it (see ``netweir.steps``): a list of step dicts, and functions
    to call with the page. A request with steps is fetched in Chrome. They
    take the place of the crawl's ``browser_steps``; ``steps=[]`` runs
    none. What each did is in the callback's ``page.steps``.

    ``method`` and one of ``form``, ``json`` or ``body`` send something
    other than a GET: ``form`` as a form submission (URL-encoded), ``json``
    or a raw ``body`` as a page's script would send it. ``referer`` is the
    page it comes from; ``page.follow`` sets it. A POST or PATCH that gets
    a server error isn't retried, since the server may have acted on it,
    unless ``retry_post=True``; nor is it ever handed to Chrome.

    ``proxy`` sends the request through that proxy, whatever the crawl's
    ``proxy_rotation``; ``""`` sends it direct, through no proxy at all.
    When ``proxy`` isn't set, ``meta["proxy"]`` does the same, as in
    Scrapy, None or ``""`` there meaning no proxy. Chrome has one proxy for all its pages, so it
    doesn't apply to a request fetched there.

    ``timeout`` and ``max_redirects`` replace the crawl's for this request;
    ``allow_redirects=False`` hands the callback a redirect itself. ``auth``
    is a (user, password) login sent as HTTP Basic to the request's own
    origin, and dropped by a redirect that leaves it; a login written into
    the URL becomes ``auth``. Scrapy's meta keys do the same when these
    aren't set: ``download_timeout``, ``dont_redirect``, ``redirect_ttl``
    (never above the crawl's limit), and ``http_user``, ``http_pass`` and
    ``http_auth_domain`` (a login for that domain and its subdomains, the
    request's own host by default). None of them applies in Chrome.
    """

    url: str
    callback: Callable[..., Any] | str | None = None
    #: Higher goes first among a site's requests. Kept below 2**31 - 1,
    #: which file downloads have: from there up, submission order decides.
    priority: int = 0
    headers: Headers = None
    meta: dict[str, Any] = dataclasses.field(default_factory=dict)
    dont_filter: bool = False
    errback: Callable[..., Any] | str | None = None
    depth: int = 0
    browser: bool = False
    #: GET, or POST when there's a form, json or body.
    method: str | None = None
    form: Any = None
    json: Any = None
    body: bytes | str | None = None
    referer: str | None = None
    retry_post: bool = False
    #: Left out of repr(): a proxy's URL may hold its password.
    proxy: str | None = dataclasses.field(default=None, repr=False)
    #: Set from form, json and body: "navigate", "form" or "fetch", and the
    #: body's Content-Type.
    kind: str | None = None
    content_type: str | None = None
    #: Seconds for each request, in place of the crawl's timeout.
    timeout: float | None = None
    #: Redirects to follow before failing, in place of the crawl's limit.
    max_redirects: int | None = None
    #: False hands the callback a redirect itself, Location and all.
    allow_redirects: bool = True
    #: A (user, password) login, sent as HTTP Basic to the request's own
    #: origin. Left out of repr(), like proxy.
    auth: tuple[str, str] | None = dataclasses.field(default=None, repr=False)
    #: Cookies for its jar, as Scrapy's ``Request.cookies``: a dict of name
    #: to value, or a list of cookie dicts (name, value, and optionally
    #: domain, path, expires, secure, http_only, same_site). Each goes into
    #: the request's jar just before it's sent, as if its URL had set it,
    #: and so with every later request the jar matches. Without a domain,
    #: a cookie is the URL's host's alone (``".shop.example"`` covers the
    #: subdomains too); without a path, the URL's directory is its path.
    #: Kept as a list of dicts, and left out of repr(): values are secrets.
    #:
    #: ``meta["cookiejar"]`` names the jar (any JSON value; requests naming
    #: the same share cookies, and no others do); without it, the crawl's
    #: own. ``meta["dont_merge_cookies"] = True`` leaves the jar out: only
    #: these cookies are sent, and nothing the response sets is kept.
    cookies: Any = dataclasses.field(default=None, repr=False)
    #: What to do on its page in Chrome before the callback, as a list of
    #: step dicts (``netweir.steps``); None for the crawl's
    #: ``browser_steps``. Not part of the request's fingerprint, as meta
    #: isn't in Scrapy's: the same URL with other steps is a duplicate
    #: unless ``dont_filter=True``.
    steps: list[Any] | None = None

    def __post_init__(self) -> None:
        from netweir._fetch import checked_auth, login_of, redirect_limit, seconds

        self.url, self.auth = login_of(self.url, checked_auth(self.auth))
        if self.timeout is not None:
            self.timeout = seconds(self.timeout)
        redirect_limit(self.max_redirects)
        if self.cookies is not None:
            self.cookies = _cookies(self.url, self.cookies)
        if self.steps is not None:
            self.steps = _steps(self.steps)
            if self.steps:
                self.browser = True
        has_body = self.form is not None or self.json is not None or self.body is not None
        self.method = (self.method or ("POST" if has_body else "GET")).upper()
        if self.form is not None or self.json is not None or self.kind is None:
            from netweir._fetch import payload

            how = payload(self.method, self.form, self.json, self.body)
            self.body = how.get("body")
            self.kind = how["kind"]
            self.content_type = how.get("content_type", self.content_type)
            # Encoded into body: replace() and pickling see one body.
            self.form = self.json = None
        if self.browser and (self.method != "GET" or self.body is not None):
            if self.steps:
                raise ValueError("only a GET can be fetched in Chrome, where steps run")
            raise ValueError("only a GET can be fetched in Chrome (browser=True)")

    def replace(self, **changes: Any) -> Request:
        """A copy with ``changes``, as Scrapy's ``Request.replace``."""
        return dataclasses.replace(self, **changes)

    @classmethod
    def from_form(
        cls,
        page: Any,
        query: str | None = None,
        *,
        data: Any = None,
        files: Any = None,
        click: str | bool | None = None,
        formid: str | None = None,
        formname: str | None = None,
        formnumber: int = 0,
        callback: Callable[..., Any] | str | None = None,
        **kwargs: Any,
    ) -> Request:
        """A Request that submits a form on ``page``, as Scrapy's
        ``FormRequest.from_response`` does; the form is picked and filled
        as ``page.form()`` does it."""
        form = page.form(
            query,
            data=data,
            files=files,
            click=click,
            formid=formid,
            formname=formname,
            formnumber=formnumber,
        )
        return form.request(callback=callback, **kwargs)


def _text(value: Any) -> Any:
    """A cookie's name or value as text: bytes as UTF-8, as Scrapy reads them."""
    return value.decode("utf-8", "replace") if isinstance(value, bytes) else value


def _cookies(url: str, cookies: Any) -> list[dict[str, Any]]:
    """``Request.cookies`` as a list of cookie dicts, from a dict of name to
    value or any iterable of dicts. Bytes are read as UTF-8, and a cookie
    whose value is None is left out with a warning, as in Scrapy."""
    if isinstance(cookies, dict):
        cookies = [{"name": k, "value": v} for k, v in cookies.items()]
    elif isinstance(cookies, (str, bytes)):
        raise TypeError("cookies must be a dict of name to value, or a list of dicts")
    else:
        cookies = list(cookies)
        if not all(isinstance(c, dict) for c in cookies):
            raise TypeError("cookies must be a dict of name to value, or a list of dicts")
    out = []
    for c in cookies:
        c = {k: _text(v) for k, v in c.items()}
        if "name" in c and not isinstance(c["name"], str):
            c["name"] = str(c["name"])
        if "value" in c and c["value"] is None:
            logging.getLogger("netweir").warning(
                "%s: the cookie %r has no value, so it's left out", url, c.get("name")
            )
            continue
        if "value" in c and not isinstance(c["value"], str):
            c["value"] = str(c["value"])
        out.append(c)
    return out


def _steps(steps: Any, where: str = "steps") -> list[Any]:
    """A step list checked, each step a dict of its own: a function in it
    becomes a call step. Raises TypeError or ValueError for one that's
    wrong, saying which."""
    if isinstance(steps, (str, bytes, dict)) or not isinstance(steps, Iterable):
        raise TypeError(f"{where} is a list of steps, not {type(steps).__name__}")
    out: list[Any] = []
    for step in steps:
        if isinstance(step, dict):
            step = dict(step)
        elif callable(step):
            step = {"action": "call", "method": step}
        else:
            raise TypeError(f"a step is a dict or a function, not {step!r}")
        out.append(step)
    from netweir._native import _check_steps

    try:
        _check_steps(steps_json(out, lambda fn: None))
    except ValueError as e:
        raise ValueError(f"{where}: {e}") from None
    return out


def steps_json(steps: list[Any], name: Callable[[Any], str | None]) -> str:
    """``steps`` as JSON, each call step's function given as ``name`` says:
    a name, or None for one only this process can run."""
    import json

    def portable(step: dict[str, Any]) -> dict[str, Any]:
        method = step.get("method")
        if step.get("action") == "call" and method is not None and not isinstance(method, str):
            return {**step, "method": name(method)}
        return step

    try:
        return json.dumps([portable(s) for s in steps])
    except TypeError as e:
        raise TypeError(f"steps must be JSON: {e}") from None
