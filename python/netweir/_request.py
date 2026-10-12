from __future__ import annotations

import dataclasses
from collections.abc import Callable
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
    """

    url: str
    callback: Callable[..., Any] | str | None = None
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

    def __post_init__(self) -> None:
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
