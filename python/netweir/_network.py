"""What a page fetches, from Python: listeners, waiters and routes; and
the popups, downloads and dialogs it starts.

Each page (and each shared context) keeps one ``Network``. Chrome's events
reach it only while something listens or waits, and nothing is kept for
later: a response nobody asked for is let go as it arrives.
"""

from __future__ import annotations

import asyncio
import contextlib
import inspect
import logging
import re
from typing import Any

from netweir._errors import BrowserError, BrowserTimeout
from netweir._native import BrowserDownload, BrowserPage, BrowserResponse, _glob_match

log = logging.getLogger("netweir")


def _check(pattern: Any, kind: str = "") -> None:
    # Only a popup or a download can be waited for without a pattern.
    if pattern is None and kind in ("popup", "download"):
        return
    if not (isinstance(pattern, (str, re.Pattern)) or callable(pattern)):
        raise TypeError(
            "a pattern is a glob string, a compiled regex or a callable, not "
            f"{type(pattern).__name__}"
        )


async def _call(f: Any, *args: Any) -> Any:
    result = f(*args)
    if inspect.isawaitable(result):
        result = await result
    return result


async def _matches(pattern: Any, url: str, subject: Any) -> bool:
    """Whether ``url`` matches a glob or regex, or a callable is true for
    ``subject``."""
    if isinstance(pattern, str):
        return _glob_match(pattern, url)
    if isinstance(pattern, re.Pattern):
        return pattern.search(url) is not None
    return bool(await _call(pattern, subject))


class Network:
    def __init__(self, parent: Network | None):
        # A page in a shared context falls back on the context's routes.
        self._parent = parent
        self._listeners: dict[str, list[tuple[Any, asyncio.AbstractEventLoop]]] = _no_listeners()
        self._waiters: list[tuple[str, Any, asyncio.Future]] = []
        self._pump: asyncio.Task | None = None
        self._events: Any = None
        self._routes: list[tuple[Any, Any]] = []
        self._routing: asyncio.Task | None = None
        # A lock belongs to one event loop; a new loop gets a new one.
        self._route_lock: tuple[asyncio.AbstractEventLoop, asyncio.Lock] | None = None
        # Callbacks and handlers running, so they aren't collected midway.
        self._tasks: set[asyncio.Task] = set()
        self._dialog_listeners: list[Any] = []
        self._dialog_pump: asyncio.Task | None = None
        self._dialogs: Any = None

    # Listening and waiting.

    def on(self, owner: Any, kind: str, callback: Any) -> None:
        if not callable(callback):
            raise TypeError("the listener must be callable")
        self._listeners[kind].append((callback, asyncio.get_running_loop()))
        self._listen(owner)

    def off(self, kind: str, callback: Any) -> None:
        self._listeners[kind] = [(c, loop) for c, loop in self._listeners[kind] if c != callback]
        self._idle()

    async def wait(self, owner: Any, kind: str, pattern: Any, timeout: float | None) -> Any:
        future = self._register(owner, kind, pattern)
        return await self._result(
            future, kind, pattern, owner._timeout if timeout is None else timeout
        )

    def expect(self, owner: Any, kind: str, pattern: Any, timeout: float | None) -> Expectation:
        _check(pattern, kind)
        return Expectation(self, owner, kind, pattern, timeout)

    def _register(self, owner: Any, kind: str, pattern: Any) -> asyncio.Future:
        """Starts waiting now, before anything is awaited, so a response
        that comes while the caller does something else still counts."""
        _check(pattern, kind)
        future = asyncio.get_running_loop().create_future()
        self._waiters.append((kind, pattern, future))
        self._listen(owner)
        return future

    async def _result(self, future: asyncio.Future, kind: str, pattern: Any, timeout: float) -> Any:
        try:
            return await asyncio.wait_for(future, timeout)
        except asyncio.TimeoutError:
            raise BrowserTimeout(f"timed out: waiting for a {_what(kind, pattern)}") from None
        finally:
            self._waiters = [w for w in self._waiters if w[2] is not future]
            self._idle()

    def _listen(self, owner: Any) -> None:
        # A pump that's done went with the event loop it ran on.
        if self._pump is None or self._pump.done():
            self._stop_listening()
            self._events = owner._listen()
            self._pump = asyncio.ensure_future(self._take(self._events))

    def _stop_listening(self) -> None:
        if self._pump is not None:
            self._pump.cancel()
            self._pump = None
        if self._events is not None:
            # The page sends nothing more to it, so nothing piles up.
            self._events.close()
            self._events = None

    def _wanted(self) -> bool:
        return bool(self._waiters or any(self._listeners.values()))

    def _idle(self) -> None:
        """Stops listening once nothing listens or waits: Chrome's events
        stop coming, and nothing piles up."""
        if not self._wanted():
            self._stop_listening()

    async def _take(self, events: Any) -> None:
        try:
            while (event := await events.next()) is not None:
                await self._dispatch(event)
                # Not held while the next is awaited: a download nobody
                # kept is cancelled once let go.
                del event
        except asyncio.CancelledError:
            if self._pump is asyncio.current_task():
                # Not stopped on purpose: the event loop is ending. Its
                # listeners and waits go with it, as they'd never run again.
                # Known limit: so do any another loop added meanwhile; a
                # page's listeners are meant for one loop at a time.
                self._listeners = _no_listeners()
                self._waiters = []
                self._pump = None
                if self._events is events:
                    self._events = None
                events.close()
            raise
        # The page closed: nothing more will come.
        if self._pump is asyncio.current_task():
            self._pump = None
            self._events = None
        for kind, pattern, future in self._waiters:
            error = BrowserError(f"the page closed before a {_what(kind, pattern)}")
            _soon(future.get_loop(), _fail, future, error)

    async def _dispatch(self, event: Any) -> None:
        # Each wait and listener is run on the event loop that added it,
        # which needn't be this one (another thread's, say).
        kind = _kind(event)
        for waiter_kind, pattern, future in list(self._waiters):
            if waiter_kind == kind and not future.done():
                _soon(future.get_loop(), self._judge, pattern, event, future)
        for callback, loop in list(self._listeners[kind]):
            _soon(loop, self._tell, kind, callback, event)

    def _judge(self, pattern: Any, event: Any, future: asyncio.Future) -> None:
        """Settles a wait if ``event`` is its match; on the wait's loop."""
        if future.done():
            return
        try:
            if pattern is None:
                matched = True
            elif isinstance(pattern, str):
                matched = _glob_match(pattern, event.url)
            elif isinstance(pattern, re.Pattern):
                matched = pattern.search(event.url) is not None
            else:
                matched = pattern(event)
                if inspect.isawaitable(matched):
                    # Settled on its own, so a slow predicate holds up
                    # nothing else. Known limit: among async predicates,
                    # the first to say yes wins, not the earliest event.
                    settle = _settle(matched, future, event)
                    self._track(asyncio.ensure_future(settle), "a wait's predicate")
                    return
        except Exception as e:
            future.set_exception(e)
            return
        if matched:
            future.set_result(event)

    def _tell(self, kind: str, callback: Any, event: Any) -> None:
        """Calls a listener; on the loop that added it."""
        try:
            result = callback(event)
        except Exception:
            log.exception("a %s listener raised", kind)
            return
        if inspect.isawaitable(result):
            self._track(asyncio.ensure_future(result), f"a {kind} listener")

    def _track(self, task: asyncio.Task, what: str) -> None:
        self._tasks.add(task)

        def done(task: asyncio.Task) -> None:
            self._tasks.discard(task)
            if not task.cancelled() and task.exception() is not None:
                log.error("%s raised", what, exc_info=task.exception())

        task.add_done_callback(done)

    # Dialogs. They come through a stream of their own, taken only while
    # a dialog listener is added: one the listeners don't answer is
    # answered the default way when they return, and with no listeners
    # Chrome's side answers every one at once.

    def on_dialog(self, owner: Any, callback: Any) -> None:
        if not callable(callback):
            raise TypeError("the listener must be callable")
        self._dialog_listeners.append(callback)
        # A pump that's done went with the event loop it ran on.
        if self._dialog_pump is None or self._dialog_pump.done():
            self._stop_dialogs()
            self._dialogs = owner._dialogs()
            self._dialog_pump = asyncio.ensure_future(self._take_dialogs(self._dialogs))

    def off_dialog(self, callback: Any) -> None:
        self._dialog_listeners = [c for c in self._dialog_listeners if c != callback]
        if not self._dialog_listeners:
            self._stop_dialogs()

    def _stop_dialogs(self) -> None:
        if self._dialog_pump is not None:
            self._dialog_pump.cancel()
            self._dialog_pump = None
        if self._dialogs is not None:
            # Dialogs not yet taken, and those to come, get the default.
            self._dialogs.close()
            self._dialogs = None

    async def _take_dialogs(self, dialogs: Any) -> None:
        try:
            while (dialog := await dialogs.next()) is not None:
                listeners = list(self._dialog_listeners)
                self._track(asyncio.ensure_future(_answer(dialog, listeners)), "a dialog listener")
        except asyncio.CancelledError:
            if self._dialog_pump is asyncio.current_task():
                # The event loop is ending: its listeners can't run any
                # more, and dialogs get the default rather than wait.
                self._dialog_listeners = []
                self._dialog_pump = None
                if self._dialogs is dialogs:
                    self._dialogs = None
                dialogs.close()
            raise

    # Routing.

    async def route(self, owner: Any, pattern: Any, handler: Any) -> None:
        _check(pattern)
        if not callable(handler):
            raise TypeError("the route handler must be callable")
        # The newest route comes first.
        self._routes.insert(0, (pattern, handler))
        async with self._lock():
            # A reader that's done went with the event loop it ran on; a
            # new one takes over, and Chrome's side drops the old one's.
            if (self._routing is None or self._routing.done()) and self._routes:
                routes = await owner._route()
                self._routing = asyncio.ensure_future(self._take_routes(routes, owner._drop_routes))

    def _lock(self) -> asyncio.Lock:
        loop = asyncio.get_running_loop()
        if self._route_lock is None or self._route_lock[0] is not loop:
            self._route_lock = (loop, asyncio.Lock())
        return self._route_lock[1]

    async def unroute(self, owner: Any, pattern: Any, handler: Any = None) -> None:
        self._routes = [
            (p, h)
            for p, h in self._routes
            if not (p == pattern and (handler is None or h == handler))
        ]
        async with self._lock():
            if self._routing is not None and not self._routes:
                # Routing stops in Chrome; the pump ends on its own once
                # Chrome's side lets go.
                self._routing = None
                await owner._unroute()

    async def _handler_for(self, url: str) -> Any:
        # Known limit: an async route pattern is awaited here, so a slow one
        # holds up the page's other routed requests; routes must be asked
        # in order, newest first.
        for pattern, handler in self._routes:
            try:
                if await _matches(pattern, url, url):
                    return handler
            except Exception:
                log.exception("a route's pattern raised for %s; it doesn't match", url)
        if self._parent is not None:
            return await self._parent._handler_for(url)
        return None

    async def _take_routes(self, routes: Any, drop: Any) -> None:
        try:
            while (route := await routes.next()) is not None:
                handler = await self._handler_for(route.request.url)
                if handler is None:
                    route._release()
                    continue
                self._track(asyncio.ensure_future(_handle(route, handler)), "a route handler")
        except asyncio.CancelledError:
            if self._routing is asyncio.current_task():
                # The event loop is ending, and its handlers can't run any
                # more: the routes go with it, and Chrome lets requests go
                # on unchanged rather than hold them for nobody. Known
                # limit: routes another loop added while this one read them
                # go too; a page's routes are meant for one loop at a time.
                self._routing = None
                self._routes = []
                drop()
            raise


def _no_listeners() -> dict[str, list[tuple[Any, asyncio.AbstractEventLoop]]]:
    return {"request": [], "response": [], "popup": [], "download": []}


def _kind(event: Any) -> str:
    if isinstance(event, BrowserResponse):
        return "response"
    if isinstance(event, BrowserPage):
        return "popup"
    if isinstance(event, BrowserDownload):
        return "download"
    return "request"


def _what(kind: str, pattern: Any) -> str:
    return kind if pattern is None else f"{kind} matching {pattern!r}"


async def _answer(dialog: Any, listeners: list[Any]) -> None:
    """Runs each dialog listener in turn; a dialog they leave unanswered,
    or raise over, is answered the default way."""
    try:
        for callback in listeners:
            try:
                await _call(callback, dialog)
            except Exception:
                log.exception("a dialog listener raised for %r", dialog)
    finally:
        dialog._release()


def _soon(loop: asyncio.AbstractEventLoop, call: Any, *args: Any) -> None:
    """Runs ``call`` on ``loop``: now, if that's the loop running, or else
    through the loop, safely from this thread. A loop that has closed
    has nobody left to tell."""
    try:
        running = asyncio.get_running_loop()
    except RuntimeError:
        running = None
    if loop is running:
        call(*args)
        return
    with contextlib.suppress(RuntimeError):
        loop.call_soon_threadsafe(call, *args)


def _fail(future: asyncio.Future, error: BaseException) -> None:
    if not future.done():
        future.set_exception(error)


async def _settle(matched: Any, future: asyncio.Future, event: Any) -> None:
    try:
        result = await matched
    except Exception as e:
        if not future.done():
            future.set_exception(e)
        return
    if result and not future.done():
        future.set_result(event)


async def _handle(route: Any, handler: Any) -> None:
    """Runs the handler; a request it leaves undecided, or raises over,
    goes on unchanged rather than waiting forever."""
    try:
        await _call(handler, route)
    except Exception:
        log.exception(
            "the route handler for %s raised; the request goes on unchanged", route.request.url
        )
    finally:
        route._release()


class Expectation:
    """``async with page.expect_response(...) as info:`` waits from the
    moment the block starts; ``await info.value`` is the match."""

    value: asyncio.Future

    def __init__(
        self, network: Network, owner: Any, kind: str, pattern: Any, timeout: float | None
    ):
        self._network = network
        self._owner = owner
        self._kind = kind
        self._pattern = pattern
        self._timeout = owner._timeout if timeout is None else timeout

    async def __aenter__(self) -> Expectation:
        future = self._network._register(self._owner, self._kind, self._pattern)
        self.value = asyncio.ensure_future(
            self._network._result(future, self._kind, self._pattern, self._timeout)
        )
        # The match is usually awaited after the block, so the wait can't
        # be called off when it ends; a value nobody awaits times out
        # quietly instead of being reported as an error never retrieved.
        self.value.add_done_callback(lambda task: task.cancelled() or task.exception())
        return self

    async def __aexit__(self, kind: object, *rest: object) -> None:
        # What was to make the request failed: nothing to wait for.
        if kind is not None:
            self.value.cancel()
