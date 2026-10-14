"""What Scrapy's components expect around them: the crawler that
``from_crawler`` is handed, and the spider middlewares around a callback.
See docs/design/hooks.md."""

from __future__ import annotations

import contextlib
import inspect
import json
import logging
import sys
import traceback
from collections.abc import AsyncIterable, AsyncIterator, Callable, Iterable
from typing import Any

from netweir import signals as _signals

log = logging.getLogger("netweir")

#: Iterable, but one result rather than many.
_NOT_MANY = (dict, str, bytes, bytearray)


class Failure:
    """Stands in for Twisted's Failure, which Scrapy hands ``spider_error``
    and ``item_error`` handlers: the exception is ``value``."""

    def __init__(self, value: BaseException):
        self.value = value
        self.type = type(value)

    def getTraceback(self) -> str:  # noqa: N802 - Twisted's name
        return "".join(traceback.format_exception(self.type, self.value, self.value.__traceback__))

    def getErrorMessage(self) -> str:  # noqa: N802 - Twisted's name
        return str(self.value)

    def check(self, *types: type) -> type | None:
        """The first of ``types`` the exception is an instance of, or None."""
        return next((t for t in types if isinstance(self.value, t)), None)

    def __repr__(self) -> str:
        return f"<Failure {self.type.__name__}: {self.value}>"


class Settings:
    """``crawler.settings``: Scrapy's getters over the spider's
    ``custom_settings``, and Netweir's settings as attributes. Scrapy's
    names for its own settings aren't mapped to Netweir's."""

    def __init__(self, settings: Any, custom: dict[str, Any] | None):
        self._settings = settings
        self._custom = dict(custom or {})

    def __getattr__(self, name: str) -> Any:
        return getattr(self._settings, name)

    def __getitem__(self, name: str) -> Any:
        return self._custom.get(name)

    def __contains__(self, name: str) -> bool:
        return name in self._custom

    def get(self, name: str, default: Any = None) -> Any:
        return self._custom.get(name, default)

    def getbool(self, name: str, default: bool = False) -> bool:
        value = self.get(name, default)
        # As Scrapy reads them: "1", "0", "True", "false" and so on.
        if value in ("True", "true"):
            return True
        if value in ("False", "false"):
            return False
        return bool(int(value))

    def getint(self, name: str, default: int = 0) -> int:
        return int(self.get(name, default))

    def getfloat(self, name: str, default: float = 0.0) -> float:
        return float(self.get(name, default))

    def getlist(self, name: str, default: list[Any] | None = None) -> list[Any]:
        value = self.get(name, default or [])
        if isinstance(value, str):
            return value.split(",") if value else []
        return list(value)

    def getdict(self, name: str, default: dict[str, Any] | None = None) -> dict[str, Any]:
        value = self.get(name, default or {})
        return dict(json.loads(value) if isinstance(value, str) else value)


class Signals:
    """``crawler.signals``: connect a handler by hand, as in Scrapy."""

    def __init__(self, run: Any):
        self._run = run

    def connect(self, receiver: Callable[..., Any], signal: Any, **_: Any) -> None:
        name = signal_name(signal)
        if name is not None:
            self._run.connect(name, receiver)

    def disconnect(self, receiver: Callable[..., Any], signal: Any, **_: Any) -> None:
        name = signal_name(signal)
        if name is not None:
            self._run.disconnect(name, receiver)


def signal_name(signal: Any) -> str | None:
    """A signal's name, from Netweir's name for it or Scrapy's object. None,
    with a warning, for one of Scrapy's this crawl never sends."""
    if isinstance(signal, str) and signal in _signals.ALL:
        return signal
    scrapy = sys.modules.get("scrapy.signals")
    if scrapy is not None and not isinstance(signal, str):
        for name, value in vars(scrapy).items():
            if value is signal:
                signal = name
                break
    if isinstance(signal, str) and signal in _signals.ALL:
        return signal
    if isinstance(signal, str):
        log.warning("the %s signal is never sent here; see docs/crawling.md", signal)
        return None
    raise ValueError(f"{signal!r} isn't a signal: use netweir.signals or scrapy.signals")


class Engine:
    """``crawler.engine``: schedule a request from a signal handler, or end
    the crawl."""

    def __init__(self, run: Any):
        self._run = run

    def crawl(self, request: Any, spider: Any = None) -> None:
        # Queued once the handler is done: middlewares may be async.
        self._run.scheduled.append(request)

    def close_spider(self, spider: Any = None, reason: str = "cancelled") -> None:
        self._run.closing = reason


class Crawler:
    """What ``from_crawler(crawler)`` is given."""

    def __init__(self, run: Any):
        self.spider = run.spider
        self.settings = Settings(run.settings, getattr(run.spider, "custom_settings", None))
        self.signals = Signals(run)
        #: The spider's stats (spider.stats), live while the crawl runs.
        self.stats = run.live
        self.engine = Engine(run)


class Invalid(TypeError):
    """A spider middleware returned something it can't: an error in the
    middleware, which no process_spider_exception is offered."""


class SpiderMiddlewares:
    """Scrapy's spider middleware chain. ``middlewares`` is in Scrapy's
    order, the first nearest the engine; outputs and exceptions are kept
    nearest the spider first, the order they're called in, with None where
    a middleware has no such method, so an index means the same in both."""

    def __init__(self, middlewares: list[Any], spider: Any):
        self.spider = spider
        self.middlewares = middlewares
        self.inputs = [
            m.process_spider_input
            for m in middlewares
            if callable(getattr(m, "process_spider_input", None))
        ]
        inward = middlewares[::-1]
        self.outputs = [_output_method(m) for m in inward]
        self.exceptions = [_method(m, "process_spider_exception") for m in inward]

    def __bool__(self) -> bool:
        return any(self.outputs) or any(self.exceptions)

    def start(self, start: Any) -> Any:
        """The spider's start requests, through process_start (or the older
        process_start_requests), the middleware nearest the spider first."""
        for middleware in self.middlewares[::-1]:
            process = _method(middleware, "process_start")
            if process is not None:
                start = process(_aiter(start))
                continue
            legacy = _method(middleware, "process_start_requests")
            if legacy is not None:
                start = _legacy_start(legacy, start, self.spider)
        return start

    async def input(self, page: Any) -> BaseException | None:
        """process_spider_input, in order: the exception one raised, or None."""
        for method in self.inputs:
            try:
                result = await _awaited(method(page, self.spider))
            except Exception as e:  # noqa: BLE001 - handed on, as Scrapy does
                return e
            if result is not None:
                return Invalid(
                    f"{_name(method)} must return None or raise, not a {type(result).__name__}"
                )
        return None

    async def scrape(
        self,
        page: Any,
        call: Callable[[], Any],
        errback: Callable[[BaseException], Any],
        failed: BaseException | None = None,
    ) -> AsyncIterator[Any]:
        """What goes on from a page's callback: ``call()``'s result, or with
        ``failed`` (a process_spider_input's exception) ``errback(failed)``'s,
        through process_spider_output, with process_spider_exception for
        what raises. Raises what no middleware takes."""
        try:
            result = await _awaited(call() if failed is None else errback(failed))
            recovered: list[AsyncIterator[Any]] = []
            stream = self._evaluate(page, _aiter(result), 0, recovered)
            return _chained(self._output(page, stream, 0), recovered)
        except Exception as e:  # noqa: BLE001 - the middlewares' to take
            return await self._exception(page, e, 0)

    def _output(self, page: Any, result: AsyncIterator[Any], start: int) -> AsyncIterator[Any]:
        recovered: list[AsyncIterator[Any]] = []
        for index in range(start, len(self.outputs)):
            method = self.outputs[index]
            if method is not None:
                result = _call_output(method, page, result, self.spider)
                result = self._evaluate(page, result, index + 1, recovered)
        return _chained(result, recovered)

    async def _evaluate(
        self, page: Any, stream: AsyncIterator[Any], index: int, recover_to: list
    ) -> AsyncIterator[Any]:
        """``stream``, with an exception from it offered to the exception
        handlers from ``index`` on; what they give instead comes after."""
        try:
            async for out in stream:
                yield out
        except Exception as e:  # noqa: BLE001 - the middlewares' to take
            if getattr(e, "_netweir_unhandled", False):
                raise
            recover_to.append(await self._exception(page, e, index))

    async def _exception(self, page: Any, error: BaseException, start: int) -> AsyncIterator[Any]:
        if isinstance(error, Invalid):
            raise error
        for index in range(start, len(self.exceptions)):
            method = self.exceptions[index]
            if method is None:
                continue
            result = await _awaited(method(page, error, self.spider))
            if result is None:
                continue
            if isinstance(result, (Iterable, AsyncIterable)) and not isinstance(result, _NOT_MANY):
                # Handled: what it gives goes through the outputs of the
                # middlewares nearer the engine.
                return self._output(page, _aiter(result), index + 1)
            raise Invalid(
                f"{_name(method)} must return None or an iterable, not a {type(result).__name__}"
            )
        # Every middleware from here declined it: the outer layers let it
        # through rather than offer it to them again, as Scrapy does.
        with contextlib.suppress(AttributeError):
            error._netweir_unhandled = True  # type: ignore[attr-defined]
        raise error


def _method(obj: Any, name: str) -> Callable[..., Any] | None:
    method = getattr(obj, name, None)
    return method if callable(method) else None


def _output_method(middleware: Any) -> Callable[..., Any] | None:
    """process_spider_output, or process_spider_output_async when a
    "universal" middleware has both, as Scrapy picks."""
    universal = _method(middleware, "process_spider_output_async")
    if universal is not None and inspect.isasyncgenfunction(universal):
        return universal
    return _method(middleware, "process_spider_output")


def _name(method: Any) -> str:
    return getattr(method, "__qualname__", repr(method))


async def _awaited(value: Any) -> Any:
    return await value if inspect.isawaitable(value) else value


async def _aiter(result: Any) -> AsyncIterator[Any]:
    """A callback's result, whatever it is, as an async iterator."""
    if isinstance(result, AsyncIterable):
        async for out in result:
            yield out
        return
    if result is None:
        return
    if isinstance(result, Iterable) and not isinstance(result, _NOT_MANY):
        for out in result:
            yield out
        return
    yield result


async def _chained(first: AsyncIterator[Any], recovered: list) -> AsyncIterator[Any]:
    async for out in first:
        yield out
    # The list can grow while it's read: an exception further on adds to it.
    for more in recovered:
        async for out in more:
            yield out


async def _call_output(
    method: Callable[..., Any], page: Any, result: AsyncIterator[Any], spider: Any
) -> AsyncIterator[Any]:
    """One process_spider_output. An async generator gets the stream as it
    comes; anything else gets it gathered into a list first, as Scrapy
    gives a sync middleware an async callback's output."""
    if inspect.isasyncgenfunction(method):
        async for out in method(page, result, spider):
            yield out
        return
    given = [out async for out in result]
    async for out in _aiter(await _awaited(method(page, given, spider))):
        yield out


async def _legacy_start(method: Callable[..., Any], start: Any, spider: Any) -> AsyncIterator[Any]:
    """process_start_requests(start_requests, spider), which takes a plain
    iterable: an async start is gathered into a list first."""
    if isinstance(start, AsyncIterable):
        start = [request async for request in start]
    for request in method(start, spider):
        yield request
