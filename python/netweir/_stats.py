"""A crawl's stats while it runs: ``spider.stats``, the progress line in
the log, and the metrics endpoint. See docs/design/metrics.md."""

from __future__ import annotations

import functools
import json
import logging
import math
import os
import socket
import threading
import time
from collections.abc import Callable, Iterator
from typing import Any
from urllib.parse import urlsplit

log = logging.getLogger("netweir")

#: Netweir's own stats, as Prometheus metrics: (type, help). Counters are
#: exported as netweir_<stat>_total, gauges as netweir_<stat>.
METRICS: dict[str, tuple[str, str]] = {
    "queued": ("gauge", "Requests waiting in this process's queues."),
    "in_flight": ("gauge", "Requests being fetched."),
    "fetched": ("counter", "Pages fetched, from the cache too."),
    "failed": ("counter", "Requests that failed after their retries."),
    "duplicates": ("counter", "Requests dropped as already seen."),
    "dropped_robots": ("counter", "Requests robots.txt kept out."),
    "dropped_tdm": ("counter", "Requests skipped for a site's TDM reservation."),
    "bytes": ("counter", "Bytes of response bodies received."),
    "skipped_depth": ("counter", "Requests dropped past max_depth."),
    "skipped_traps": ("counter", "Requests dropped as crawler traps."),
    "skipped_domain_full": ("counter", "Requests dropped past max_pages_per_domain."),
    "skipped_delta": ("counter", "Requests deltafetch skipped."),
    "retries": ("counter", "Requests tried again."),
    "blocked": ("counter", "Responses that were blocks."),
    "throttled": ("counter", "Responses that asked the crawl to slow down."),
    "sessions_replaced": ("counter", "Sessions started afresh after a block."),
    "breaker_trips": ("counter", "Times a host was paused for blocking."),
    "browser_fetches": ("counter", "Pages fetched in Chrome."),
    "browser_unblocked": ("counter", "Requests blocked over HTTP that Chrome got through."),
    "offsite": ("counter", "Requests dropped for going outside allowed_domains."),
    "cache_hits": ("counter", "Pages served from the cache."),
    "cache_stores": ("counter", "Responses written to the cache."),
    "proxies_benched": ("counter", "Times a proxy was benched."),
    "archive_errors": ("counter", "Responses that didn't reach the WARC or HAR file."),
    "pushed": ("counter", "Requests sent to the shared queue."),
    "leased": ("counter", "Requests taken from the shared queue."),
    "leases_reclaimed": ("counter", "Requests taken back from processes that died."),
    "queue_bad": ("counter", "Shared queue members that weren't requests."),
    "items": ("counter", "Items delivered."),
    "items_dropped": ("counter", "Items a pipeline dropped."),
    "items_validated": ("counter", "Items a validate stage checked."),
    "items_invalid": ("counter", "Items that failed validation."),
    "requests_dropped": ("counter", "Requests a middleware dropped."),
    "responses_dropped": ("counter", "Responses a middleware dropped."),
    "pages_served": ("counter", "Pages a middleware gave in place of a fetch."),
    "callback_errors": ("counter", "Exceptions in callbacks and pipelines."),
    "invalid_urls": ("counter", "Requests whose URL couldn't be used."),
    "pages_ignored": ("counter", "Pages no callback took."),
    "items_already_written": ("counter", "Items a resumed crawl had already written."),
    "relocated": ("counter", "Tracked selectors found again after breaking."),
    "lost": ("counter", "Tracked selectors that couldn't be found again."),
    "repair_proposals": ("counter", "Selector repairs proposed."),
    "checks_failed": ("counter", "Checks that failed at the end of the crawl."),
    "files_downloaded": ("counter", "Files and images downloaded."),
    "files_cached": ("counter", "Files and images found already in the store."),
    "files_uptodate": ("counter", "Files and images still fresh in the store."),
    "files_failed": ("counter", "Files and images that couldn't be downloaded."),
    "images_too_small": ("counter", "Images dropped as smaller than min_size."),
}

#: The stats that are the engine's gauges, not part of the stats a crawl
#: returns.
GAUGES = ("queued", "in_flight")


class PortTaken(OSError):
    """The metrics endpoint couldn't listen where the settings say."""


class Stats(dict):
    """A crawl's stats: read-only, a fresh snapshot at every read while
    the crawl runs, and its final stats once it has ended. Scrapy's stats
    collector methods work on it, and the writing ones keep stats of the
    spider's own beside Netweir's.

    It's a dict, as ``spider.stats`` always was: once the crawl has ended
    it holds the final stats, so json.dumps, pickle and copy see them.
    While the crawl runs only its methods are live; ``get_stats()`` gives
    a plain dict to hand to anything else."""

    def __init__(self, read: Callable[[], dict[str, Any]] | None = None):
        super().__init__()
        #: The live stats, gauges included; None in a worker process.
        self._read = read
        self._custom: dict[str, Any] = {}
        self._final: dict[str, Any] | None = None
        #: Taken by every read, so none reads the engine once it's closing.
        self._lock = threading.Lock()
        #: In a worker: the writes to send to the main process.
        self._ops: list[tuple[str, str, tuple]] | None = None

    def _snapshot(self) -> tuple[dict[str, Any], dict[str, int]]:
        """(the stats, the gauges)."""
        with self._lock:
            if self._final is not None:
                return dict(self._final), {g: 0 for g in GAUGES}
            if self._read is None:
                return dict(self._custom), {g: 0 for g in GAUGES}
            stats = self._read()
        gauges = {g: stats.pop(g, 0) for g in GAUGES}
        return stats, gauges

    def _freeze(self, final: dict[str, Any]) -> None:
        """``final`` becomes the stats for good, with the spider's own
        written since it was read (a pipeline's close, a check) added in
        place, so the dict run() returns has them too."""
        with self._lock:
            final.update(self._custom)
            self._final = final
            dict.update(self, final)

    # --- reading ----------------------------------------------------------------

    def get_stats(self, spider: Any = None) -> dict[str, Any]:
        return self._snapshot()[0]

    def get_value(self, key: str, default: Any = None, spider: Any = None) -> Any:
        return self.get_stats().get(key, default)

    def __getitem__(self, key: str) -> Any:
        return self.get_stats()[key]

    def get(self, key: str, default: Any = None) -> Any:
        return self.get_stats().get(key, default)

    def __contains__(self, key: object) -> bool:
        return key in self.get_stats()

    def __iter__(self) -> Iterator[str]:
        return iter(self.get_stats())

    def __len__(self) -> int:
        return len(self.get_stats())

    def keys(self):  # type: ignore[override]
        return self.get_stats().keys()

    def values(self):  # type: ignore[override]
        return self.get_stats().values()

    def items(self):  # type: ignore[override]
        return self.get_stats().items()

    def copy(self) -> dict[str, Any]:  # type: ignore[override]
        return self.get_stats()

    def __eq__(self, other: object) -> bool:
        if isinstance(other, Stats):
            other = other.get_stats()
        return self.get_stats() == other

    def __ne__(self, other: object) -> bool:
        return not self == other

    __hash__ = None  # type: ignore[assignment]

    def __repr__(self) -> str:
        return f"Stats({self.get_stats()!r})"

    def __reduce__(self) -> tuple:
        # Pickled and copied as the plain dict it reads as, without the
        # lock or the engine.
        return (dict, (self.get_stats(),))

    def _read_only(self, *args: Any, **kwargs: Any) -> Any:
        raise TypeError("the stats are read-only: keep your own with set_value or inc_value")

    __setitem__ = __delitem__ = __ior__ = _read_only
    update = pop = popitem = clear = setdefault = _read_only  # type: ignore[assignment]

    # --- writing a spider's own -----------------------------------------------

    def set_value(self, key: str, value: Any, spider: Any = None) -> None:
        self._write("set_value", key, (value,))

    def inc_value(self, key: str, count: Any = 1, start: Any = 0, spider: Any = None) -> None:
        self._write("inc_value", key, (count, start))

    def max_value(self, key: str, value: Any, spider: Any = None) -> None:
        self._write("max_value", key, (value,))

    def min_value(self, key: str, value: Any, spider: Any = None) -> None:
        self._write("min_value", key, (value,))

    def _write(self, op: str, key: str, args: tuple) -> None:
        if key in METRICS:
            raise ValueError(f"{key!r} is one of Netweir's own stats; name yours differently")
        with self._lock:
            if self._final is None:
                _apply(self._custom, op, key, args)
            else:
                _apply(self._final, op, key, args)
                dict.__setitem__(self, key, self._final[key])
        if self._ops is not None:
            self._ops.append((op, key, args))

    def _replay(self, ops: list[tuple[str, str, tuple]]) -> None:
        """A worker's writes, in the order it made them."""
        for op, key, args in ops:
            self._write(op, key, args)


def _apply(into: dict[str, Any], op: str, key: str, args: tuple) -> None:
    if op == "set_value":
        into[key] = args[0]
    elif op == "inc_value":
        count, start = args
        into[key] = into.get(key, start) + count
    elif op == "max_value":
        into[key] = max(into[key], args[0]) if key in into else args[0]
    else:
        into[key] = min(into[key], args[0]) if key in into else args[0]


# --- the progress line ---------------------------------------------------------


def progress(
    stats: dict[str, Any], gauges: dict[str, int], before: dict[str, Any], seconds: float
) -> str:
    """The progress line: totals, and rates per minute since ``before``,
    ``seconds`` ago."""
    per_minute = 60 / seconds if seconds > 0 else 0.0
    pages, items = stats.get("fetched", 0), stats.get("items", 0)
    page_rate = (pages - before.get("fetched", 0)) * per_minute
    item_rate = (items - before.get("items", 0)) * per_minute
    errors = stats.get("failed", 0) + stats.get("callback_errors", 0)
    return (
        f"crawled {pages} pages (at {page_rate:.0f} pages/min),"
        f" scraped {items} items (at {item_rate:.0f} items/min);"
        f" {errors} errors, {gauges['queued']} queued, {gauges['in_flight']} in flight,"
        f" {_size(stats.get('bytes', 0))} received"
    )


def _size(n: float) -> str:
    for unit in ("B", "KiB", "MiB"):
        if n < 1024:
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} GiB"


# --- Prometheus --------------------------------------------------------------------


def prometheus(
    stats: dict[str, Any], gauges: dict[str, int], labels: dict[str, str], started: float
) -> str:
    """The stats in Prometheus' text exposition format, version 0.0.4."""
    common = ",".join(f'{k}="{_escape(v, quote=True)}"' for k, v in labels.items())
    lines: list[str] = []

    def family(name: str, kind: str, help: str, samples: list[tuple[str, Any]]) -> None:
        lines.append(f"# HELP {name} {_escape(help)}")
        lines.append(f"# TYPE {name} {kind}")
        for extra, value in samples:
            lines.append(f"{name}{{{common}{extra}}} {_number(value)}")

    for key, value in {**gauges, **stats}.items():
        if key not in METRICS or not _numeric(value):
            continue
        kind, help = METRICS[key]
        family(
            f"netweir_{key}_total" if kind == "counter" else f"netweir_{key}",
            kind,
            help,
            [("", value)],
        )
    family(
        "netweir_start_time_seconds",
        "gauge",
        "When the crawl started, in Unix time.",
        [("", started)],
    )
    custom = [
        (f',stat="{_escape(key, quote=True)}"', value)
        for key, value in stats.items()
        if key not in METRICS and _numeric(value)
    ]
    if custom:
        family("netweir_custom_stat", "untyped", "Stats the spider keeps itself.", custom)
    return "\n".join(lines) + "\n"


def _numeric(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _number(value: float) -> str:
    if isinstance(value, float):
        if math.isnan(value):
            return "NaN"
        if math.isinf(value):
            return "+Inf" if value > 0 else "-Inf"
    return str(value)


def _escape(text: str, quote: bool = False) -> str:
    text = str(text).replace("\\", "\\\\").replace("\n", "\\n")
    return text.replace('"', '\\"') if quote else text


# --- watching a crawl ---------------------------------------------------------------


class Monitor:
    """The progress line and the metrics endpoint of one crawl, each on a
    thread of its own, so a busy event loop doesn't hold them up."""

    def __init__(self, stats: Stats, settings: Any, spider: str):
        self.stats = stats
        self.settings = settings
        self.labels = {"spider": spider, "process": f"{socket.gethostname()}:{os.getpid()}"}
        self.started = time.time()
        self.stopping = threading.Event()
        self.threads: list[threading.Thread] = []
        self.server: Any = None
        #: Host names a request may name, besides addresses.
        self.names = frozenset(n.lower().rstrip(".") for n in settings.metrics_allowed_hosts)

    def start(self) -> None:
        """Opens the endpoint first: a port that's taken stops the crawl
        before anything starts."""
        if self.settings.metrics_port is not None:
            self._listen()
        if self.settings.stats_interval is not None:
            self._thread(self._log_progress, "netweir-progress")

    def stop(self) -> None:
        self.stopping.set()
        try:
            if self.server is not None:
                self.server.shutdown()
                self.server.server_close()
        finally:
            for thread in self.threads:
                thread.join(timeout=5)

    def _thread(self, target: Callable[[], None], name: str) -> None:
        thread = threading.Thread(target=target, name=name, daemon=True)
        thread.start()
        self.threads.append(thread)

    def _log_progress(self) -> None:
        interval = self.settings.stats_interval
        before: dict[str, Any] = {}
        then = time.monotonic()
        while not self.stopping.wait(interval):
            try:
                stats, gauges = self.stats._snapshot()
                now = time.monotonic()
                log.info("%s", progress(stats, gauges, before, now - then))
            except Exception:  # noqa: BLE001 - one bad read doesn't end the lines
                log.exception("reading the crawl's stats for the progress line failed")
                continue
            before, then = stats, now

    def _listen(self) -> None:
        from netweir import _serve

        host, port = self.settings.metrics_host, self.settings.metrics_port
        cls, handler = _server_classes()
        if ":" in host:
            cls = type("Server6", (cls,), {"address_family": socket.AF_INET6})
        try:
            server = cls((host, port), handler)
        except OSError as e:
            raise PortTaken(f"metrics_port {port} on {host}: {e.strerror or e}") from None
        server.monitor = self
        self.server = server
        bound = server.server_address[1]
        shown = f"[{host}]" if ":" in host else host
        log.info("metrics at http://%s:%d/metrics", shown, bound)
        if not _serve._loopback(host):
            log.warning(
                "metrics_host %s: the stats can be read by anyone who can reach port %d;"
                " there's no password, so keep it behind a firewall or a proxy that asks for one",
                host,
                bound,
            )
        # Checking for shutdown every 0.1 s: stopping waits for that.
        self._thread(lambda: server.serve_forever(0.1), "netweir-metrics")

    def json(self) -> dict[str, Any]:
        stats, gauges = self.stats._snapshot()
        return {
            **self.labels,
            "elapsed": round(time.time() - self.started, 3),
            **gauges,
            "stats": stats,
        }

    def metrics(self) -> str:
        stats, gauges = self.stats._snapshot()
        return prometheus(stats, gauges, self.labels, self.started)


@functools.cache
def _server_classes() -> tuple[type, type]:
    """The endpoint's server, built on netweir serve's (its connection caps,
    its deadline, its refusal of web pages), which a crawl without the
    endpoint never imports."""
    from netweir import _serve

    class Server(_serve._Server):
        monitor: Monitor

    class Handler(_serve._Handler):
        """GET /metrics and /stats.json; nothing else."""

        server: Server

        def do_POST(self) -> None:
            self._fail(405, "only GET is answered here")

        def do_GET(self) -> None:
            try:
                self._check_origin(self.server.monitor.names)
            except _serve._Error as e:
                return self._fail(e.status, str(e))
            self.deadline.cancel()
            path = urlsplit(self.path).path
            monitor = self.server.monitor
            if path == "/metrics":
                body = monitor.metrics().encode()
                kind = "text/plain; version=0.0.4; charset=utf-8"
            elif path == "/stats.json":
                body = json.dumps(monitor.json(), default=str).encode()
                kind = "application/json"
            else:
                return self._fail(404, f"no such path: {path}; there's /metrics and /stats.json")
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _fail(self, status: int, message: str) -> None:
            self._send_json(status, {"status": "error", "message": message})

    return Server, Handler
