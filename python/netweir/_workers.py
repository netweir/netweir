"""Callbacks in worker processes: ``Settings(workers=N)``.

The main process keeps the engine, the checkpoint and the pipelines; a
worker builds its own spider, runs one page's callback to the end and
sends back everything it yielded. See docs/design/workers.md.
"""

from __future__ import annotations

import asyncio
import importlib
import importlib.util
import inspect
import logging
import logging.handlers
import multiprocessing
import multiprocessing.connection
import os
import pickle
import signal
import sys
import threading
import traceback
from collections.abc import Iterable
from concurrent.futures import ProcessPoolExecutor
from typing import Any

from netweir import _memory, _stats, _track
from netweir._native import TrackStore
from netweir._request import Request

#: The spider this worker runs callbacks for.
_spider: Any = None
#: Whether the crawl asks for repairs, and the (site, name) pairs this
#: worker has already sent a page for.
_repairing = False
_asked: set[tuple[str, str]] = set()


def spec_of(spider: Any) -> tuple[str, ...]:
    """Where a worker finds the spider's class: a module, or the file
    `netweir crawl` loaded it from."""
    cls = type(spider)
    try:
        inspect.signature(cls).bind()
    except TypeError:
        raise TypeError(
            f"with workers, each worker builds its own {cls.__name__} with no arguments,"
            " which it can't be"
        ) from None
    if "<locals>" in cls.__qualname__:
        raise TypeError(
            f"with workers, the spider class {cls.__name__} must be defined at module level,"
            " so worker processes can import it"
        )
    module = sys.modules.get(cls.__module__)
    path = getattr(module, "__file__", None)
    if cls.__module__.startswith("netweir_spider_") and path:
        return ("file", path, cls.__module__, cls.__qualname__)
    if cls.__module__ == "__main__":
        # A spawned worker imports the main script as __mp_main__.
        return ("module", "__mp_main__", cls.__qualname__)
    return ("module", cls.__module__, cls.__qualname__)


def _load(spec: tuple[str, ...]) -> type:
    if spec[0] == "file":
        _, path, name, qualname = spec
        module = sys.modules.get(name)
        if module is None:
            # As netweir crawl loaded it: its folder's settings.py and
            # files beside it importable while it runs.
            from netweir._cli import _exec_file

            module = _exec_file(name, path)
    else:
        _, name, qualname = spec
        module = importlib.import_module(name)
    found: Any = module
    for part in qualname.split("."):
        found = getattr(found, part)
    return found


def _start(
    spec: tuple[str, ...],
    settings: Any,
    logs: Any,
    level: int,
    track_path: str,
    threshold: float,
    repairing: bool,
    arguments: dict[str, str],
) -> None:
    """Sets a worker up: its logging goes to the main process, and it has
    its own spider and the crawl's track store."""
    # A main process killed outright (a crash, kill -9) can't shut its
    # workers down: each watches it and goes with it.
    parent = multiprocessing.parent_process()
    if parent is not None:
        threading.Thread(target=_outlive_not, args=(parent.sentinel,), daemon=True).start()
    if sys.platform == "win32":
        # The Ctrl-Break that cancels a `netweir serve` job reaches every
        # process on the job's console. Left to it, a worker would exit at
        # once and fail the page it had; the job stops its workers itself.
        # Known limit: one still starting up hasn't got this far.
        signal.signal(signal.SIGBREAK, signal.SIG_IGN)
    root = logging.getLogger()
    root.handlers[:] = [logging.handlers.QueueHandler(logs)]
    root.setLevel(level)
    global _spider, _repairing
    _repairing = repairing
    _spider = _load(spec)()
    _spider.settings = settings
    # Records what callbacks write to it, for the main process to replay.
    _spider.stats = _stats.Stats()
    # What `netweir crawl -a` set on the main process's spider.
    for name, value in arguments.items():
        setattr(_spider, name, value)
    _track._crawl.set((TrackStore(track_path), threshold))


def _outlive_not(sentinel: Any) -> None:
    multiprocessing.connection.wait([sentinel])
    os._exit(1)


def run(name: str, response: Any, request: Request) -> tuple:
    """Runs the spider's callback `name` on a page, to the end. Returns
    what it yielded (callbacks by name), what tracked selectors had to
    report, the repairs they asked for, and the error it ended with, if
    any: (exception, traceback text). Then the worker's peak memory, and
    the writes the callback made to ``spider.stats``."""
    from netweir._fetch import Page

    outputs: list[Any] = []
    reports: list[tuple] = []
    repairs: list[tuple] = []

    def ask(site, field, kind, query, html, url):
        # The page goes to the main process once per (site, name) from
        # each worker, and only if the crawl asks for repairs.
        if (site, field) in _asked:
            return
        _asked.add((site, field))
        repairs.append((site, field, kind, query, html() if callable(html) else html, url))

    ops: list[tuple] = []
    stats = getattr(_spider, "stats", None)
    if isinstance(stats, _stats.Stats):
        stats._ops = ops
    from netweir import _fetch

    settings = getattr(_spider, "settings", None)
    _fetch._xml_recover.set(bool(getattr(settings, "xml_recover", False)))
    told = _track._report_sink.set(reports.append)
    asking = _track._repairs.set(ask if _repairing else None)
    error = None
    try:
        result = getattr(_spider, name)(Page(response, request))
        if inspect.isasyncgen(result):
            asyncio.run(_drain(result, outputs))
        else:
            if inspect.isawaitable(result):
                result = asyncio.run(_wait(result))
            if result is not None:
                many = isinstance(result, Iterable) and not isinstance(
                    result, (dict, str, bytes, bytearray)
                )
                for out in result if many else [result]:
                    outputs.append(out)
    except Exception as e:  # noqa: BLE001 - the main process decides
        error = (e, traceback.format_exc())
    finally:
        _track._repairs.reset(asking)
        _track._report_sink.reset(told)
    # Everything sent back must come apart again in the main process: what
    # pickles but can't be rebuilt would break the pool, not one page. What
    # can't travel ends the page's outputs there, as an exception would.
    sendable = []
    for out in outputs:
        try:
            out = _portable(out)
            pickle.loads(pickle.dumps(out))
        except Exception as e:  # noqa: BLE001
            error = error or (
                TypeError(f"{name} yielded something that can't go back to the main process: {e}"),
                traceback.format_exc(),
            )
            break
        sendable.append(out)
    try:
        pickle.loads(pickle.dumps(ops))
    except Exception as e:  # noqa: BLE001
        ops = []
        error = error or (
            TypeError(f"{name} set a stat that can't go back to the main process: {e}"),
            traceback.format_exc(),
        )
    if error is not None:
        try:
            pickle.loads(pickle.dumps(error[0]))
        except Exception:  # noqa: BLE001 - not every exception travels
            error = (RuntimeError(f"{type(error[0]).__name__}: {error[0]}"), error[1])
    # The worker's peak memory, for max_memory in the main process.
    return sendable, reports, repairs, error, (os.getpid(), _memory.peak()), ops


async def _drain(results: Any, into: list[Any]) -> None:
    async for out in results:
        into.append(out)


async def _wait(awaitable: Any) -> Any:
    return await awaitable


def _portable(out: Any) -> Any:
    """A Request whose callbacks are this worker's spider's methods, by
    name, as they must be to mean the same in the main process."""
    if isinstance(out, Request):
        out.callback = _name_of(out.callback)
        out.errback = _name_of(out.errback)
    return out


def _name_of(fn: Any) -> str | None:
    if fn is None or isinstance(fn, str):
        return fn
    name = getattr(fn, "__name__", None)
    if name is not None and getattr(_spider, name, None) == fn:
        return name
    raise TypeError(
        f"with workers, callbacks are passed by name, so {fn!r} must be a method of the spider"
    )


class Pool:
    """The worker processes of one crawl, and their logging."""

    def __init__(self, spider: Any, settings: Any, track_path: str):
        spec = spec_of(spider)
        repairing = getattr(spider, "repair", None) is not None
        context = multiprocessing.get_context("spawn")
        self._logs = context.Queue()
        self._listener = logging.handlers.QueueListener(self._logs, _Forward())
        self._listener.start()
        # Records below every level the main process shows aren't worth
        # sending; its own loggers then decide as usual.
        level = min(
            logging.getLogger().getEffectiveLevel(),
            logging.getLogger("netweir").getEffectiveLevel(),
        )
        self._executor = ProcessPoolExecutor(
            max_workers=settings.workers,
            mp_context=context,
            initializer=_start,
            initargs=(
                spec,
                settings,
                self._logs,
                level,
                track_path,
                settings.track_threshold,
                repairing,
                getattr(spider, "_arguments", {}),
            ),
        )
        self._jobs: set[asyncio.Future] = set()

    def submit(self, name: str, response: Any, request: Request) -> asyncio.Future:
        job = asyncio.wrap_future(self._executor.submit(run, name, response, request))
        # A crawl that stops early never awaits the jobs still running, and
        # their BrokenProcessPool, once close() ends the workers, would be
        # reported as an error nobody handled. Awaiting a job still raises.
        job.add_done_callback(_seen)
        self._jobs.add(job)
        job.add_done_callback(self._jobs.discard)
        return job

    def close(self, abandon: bool = False) -> None:
        """Waits for the workers to finish, or, abandoning the crawl (an
        error with fail_fast, an interrupt), stops them where they are."""
        if abandon:
            # Cancelled, the jobs still out can't report the BrokenProcessPool
            # ending the workers brings: it may arrive after the loop's last
            # turn, before _seen could retrieve it.
            for job in list(self._jobs):
                job.cancel()
            # Known limit: the executor has no public way to stop running
            # work; its processes are terminated directly.
            for process in list(getattr(self._executor, "_processes", {}).values()):
                process.terminate()
            self._executor.shutdown(wait=False, cancel_futures=True)
        else:
            self._executor.shutdown(wait=True, cancel_futures=True)
        self._listener.stop()


def _seen(job: asyncio.Future) -> None:
    if not job.cancelled():
        job.exception()


class _Forward(logging.Handler):
    """Hands a worker's log records to the main process's loggers."""

    def emit(self, record: logging.LogRecord) -> None:
        logging.getLogger(record.name).handle(record)
