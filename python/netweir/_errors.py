class FetchError(Exception):
    """A request failed before a response arrived.

    ``kind`` says why: ``"invalid"`` (bad URL, header or proxy),
    ``"timeout"``, ``"connect"``, ``"tls"``, ``"too_many_redirects"``,
    ``"body"`` (the connection broke mid-response), ``"too_large"`` (the
    body passed the size limit) or ``"other"``.
    """

    def __init__(self, message: str, kind: str = "other"):
        super().__init__(message)
        self.kind = kind


class QueueError(ConnectionError):
    """A crawl's shared queue in Redis couldn't be reached, or answered
    with an error, or was lost for longer than ``queue_lease``."""


class DropItem(Exception):
    """Raised in a pipeline to drop the item, as returning None does. Scrapy
    pipelines drop items this way; Scrapy's own DropItem works too."""


class IgnoreRequest(Exception):
    """Raised in a middleware to drop the request or page it was given.
    Scrapy middlewares drop requests this way; Scrapy's own IgnoreRequest
    works too."""


class CloseSpider(Exception):
    """Raised in a callback, an errback, ``start()`` or a ``spider_idle``
    handler to end the crawl, with ``reason`` as its ``finish_reason``.
    Scrapy's own CloseSpider works too."""

    def __init__(self, reason: str = "cancelled"):
        super().__init__(reason)
        self.reason = reason


class DontCloseSpider(Exception):
    """Raised in a ``spider_idle`` handler to keep the crawl open with
    nothing to do; the signal comes again a few seconds later. Scrapy's own
    DontCloseSpider works too."""


class NotConfigured(Exception):
    """Raised in a component's ``from_crawler`` (or its ``__init__``) to
    leave it out of the crawl. Scrapy's own NotConfigured works too."""


class Blocked(Exception):
    """Bot protection turned the request away.

    ``vendor`` names it (``"cloudflare"``, ``"datadome"``, ``"akamai"``,
    ``"human"``, ``"kasada"``, ``"imperva"``, ``"aws-waf"``), ``kind`` says
    how (``"challenge"``, ``"captcha"``, ``"block"`` or ``"rate_limit"``)
    and ``page`` is the response that said so.
    """

    def __init__(self, vendor: str, kind: str, page):
        super().__init__(f"{page.url}: blocked by {vendor} ({kind})")
        self.vendor = vendor
        self.kind = kind
        self.page = page


class BrowserError(Exception):
    """Chrome couldn't be started, closed under a page, or a script in the
    page threw."""


class BrowserTimeout(BrowserError, TimeoutError):
    """A navigation or an action ran out of time. The message says what
    never happened, such as the element never becoming visible.

    From scrolling to the bottom, ``rounds`` is how many rounds of more had
    loaded when the time ran out; they're still in the page. Otherwise it's
    None."""

    rounds: int | None = None
