"""The signals a crawl sends, by the names Scrapy gives them, for
``crawler.signals.connect(handler, signal=netweir.signals.spider_idle)``.

A component can also hear one by having a method of its name; see
docs/crawling.md. Each is its name as a string, so ``"spider_idle"``
works as well, and so do Scrapy's own objects from ``scrapy.signals``.
"""

engine_started = "engine_started"
engine_stopped = "engine_stopped"
spider_opened = "spider_opened"
spider_idle = "spider_idle"
spider_closed = "spider_closed"
spider_error = "spider_error"
request_scheduled = "request_scheduled"
request_dropped = "request_dropped"
request_left_downloader = "request_left_downloader"
response_received = "response_received"
response_downloaded = "response_downloaded"
item_scraped = "item_scraped"
item_dropped = "item_dropped"
item_error = "item_error"

#: Every signal a crawl sends.
ALL = frozenset(v for k, v in list(globals().items()) if not k.startswith("_") and k == v)
