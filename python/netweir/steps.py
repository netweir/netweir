"""Steps: what to do on a page in Chrome before its callback sees it.

Give a request a list of them and it's fetched in Chrome, where they run
in order once the page has loaded::

    from netweir import steps

    yield netweir.Request(
        "https://shop.example/catalogue",
        steps=[
            steps.click("#accept-cookies", timeout=3, on_error="continue"),
            steps.click("button.load-more"),
            steps.wait_for("li.product:nth-child(40)"),
        ],
    )

Each function here gives a plain dict, ``{"action": "click", "selector":
...}``, so a step list is JSON: it's kept in a checkpoint and sent through
a shared queue as the request is. Writing the dicts yourself does the same.

Every step takes ``timeout``, in seconds (the browser's 30 by default),
and ``on_error``: ``"fail"`` (the default) fails the request, so its
errback gets a ``FetchError`` of kind ``"step"``; ``"continue"`` notes the
error and goes on; ``"stop"`` skips the steps after it and hands the
callback the page as it is. Steps that find an element take ``frame``,
``{"name": ...}`` and/or ``{"url": glob}``, to act in an ``<iframe>``.

What each step did is in ``page.steps``: a dict per step with ``action``,
``status`` (``"ok"``, ``"failed"`` or ``"skipped"``), ``error``,
``elapsed`` (seconds) and ``result``.
"""

from __future__ import annotations

import os
from collections.abc import Callable, Iterable
from typing import Any

__all__ = [
    "answer_dialogs",
    "call",
    "check",
    "click",
    "evaluate",
    "fill",
    "hover",
    "press",
    "screenshot",
    "scroll",
    "select_option",
    "set_input_files",
    "type",
    "uncheck",
    "wait",
    "wait_for",
    "wait_for_navigation",
    "wait_for_response",
]

_FRAME = dict[str, str] | None


def _step(action: str, timeout: float | None, on_error: str, **fields: Any) -> dict[str, Any]:
    step: dict[str, Any] = {"action": action}
    step.update({k: v for k, v in fields.items() if v is not None})
    if timeout is not None:
        step["timeout"] = timeout
    if on_error != "fail":
        step["on_error"] = on_error
    return step


def click(
    selector: str, *, frame: _FRAME = None, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Clicks the element with the mouse (taps it on a phone), once it's
    visible, still, enabled and not covered: ``page.click``."""
    return _step("click", timeout, on_error, selector=selector, frame=frame)


def fill(
    selector: str,
    text: str,
    *,
    frame: _FRAME = None,
    timeout: float | None = None,
    on_error: str = "fail",
) -> dict[str, Any]:
    """Replaces what's in a field with ``text`` at once, as pasting would:
    ``page.fill``."""
    return _step("fill", timeout, on_error, selector=selector, text=text, frame=frame)


def type(  # noqa: A001 - the step's name
    selector: str,
    text: str,
    *,
    delay: float | tuple[float, float] | None = None,
    frame: _FRAME = None,
    timeout: float | None = None,
    on_error: str = "fail",
) -> dict[str, Any]:
    """Types ``text`` into a field a key at a time, replacing what's there,
    with a pause between keys picked from ``delay`` (seconds, or a (least,
    most) pair; a person's 0.08 to 0.25 by default): ``page.type``. The
    typing isn't held to ``timeout``, only the wait for the field."""
    if isinstance(delay, tuple):
        delay = list(delay)  # type: ignore[assignment]
    return _step("type", timeout, on_error, selector=selector, text=text, delay=delay, frame=frame)


def press(key: str, *, timeout: float | None = None, on_error: str = "fail") -> dict[str, Any]:
    """Presses a key in whatever has focus: ``page.press``."""
    return _step("press", timeout, on_error, key=key)


def hover(
    selector: str, *, frame: _FRAME = None, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Moves the mouse over the element: ``page.hover``."""
    return _step("hover", timeout, on_error, selector=selector, frame=frame)


def wait_for(
    selector: str,
    state: str = "visible",
    *,
    frame: _FRAME = None,
    timeout: float | None = None,
    on_error: str = "fail",
) -> dict[str, Any]:
    """Waits until the element is ``"attached"``, ``"visible"``,
    ``"hidden"`` or ``"detached"``: ``page.wait_for``."""
    return _step("wait_for", timeout, on_error, selector=selector, state=state, frame=frame)


def wait(seconds: float, *, on_error: str = "fail") -> dict[str, Any]:
    """Waits ``seconds``, whatever the page does."""
    return _step("wait", None, on_error, seconds=seconds)


def wait_for_navigation(
    wait: str = "load", *, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Waits for the page to move on from the document it showed before
    the step before this one, and for the new one to reach ``wait``
    (``"load"``, ``"domcontentloaded"`` or ``"networkidle"``). Put it after
    the step that navigates: a navigation that's already over counts."""
    return _step("wait_for_navigation", timeout, on_error, wait=wait)


def wait_for_response(
    url: str, *, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Waits for a response whose URL matches the glob ``url``, counting
    those that came in since the step before this one began. Its
    ``result`` is ``{"url", "status"}``."""
    return _step("wait_for_response", timeout, on_error, url=url)


def scroll(
    selector: str | None = None,
    *,
    x: float | None = None,
    y: float | None = None,
    times: int | None = None,
    idle: float | None = None,
    frame: _FRAME = None,
    timeout: float | None = None,
    on_error: str = "fail",
) -> dict[str, Any]:
    """Turns the mouse wheel, as ``page.scroll`` does: to the bottom over
    and over while more loads (at most ``times`` rounds, each waiting up
    to ``idle`` seconds, 2 by default), with no arguments; an element into
    view with ``selector``; by ``x`` and ``y`` pixels with those. Held to
    ``timeout`` even with ``times``. Its ``result`` is how many rounds
    loaded more."""
    return _step(
        "scroll",
        timeout,
        on_error,
        selector=selector,
        x=x,
        y=y,
        times=times,
        idle=idle,
        frame=frame,
    )


def select_option(
    selector: str,
    value: str | Iterable[str] | None = None,
    *,
    label: str | Iterable[str] | None = None,
    index: int | Iterable[int] | None = None,
    frame: _FRAME = None,
    timeout: float | None = None,
    on_error: str = "fail",
) -> dict[str, Any]:
    """Picks options of a ``<select>`` by ``value``, ``label`` or
    ``index``, one or a list: ``page.select_option``. Its ``result`` is
    the values selected."""

    def listed(v: Any) -> Any:
        return v if v is None or isinstance(v, (str, int)) else list(v)

    return _step(
        "select_option",
        timeout,
        on_error,
        selector=selector,
        value=listed(value),
        label=listed(label),
        index=listed(index),
        frame=frame,
    )


def check(
    selector: str, *, frame: _FRAME = None, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Checks a checkbox or radio button, if it isn't: ``page.check``."""
    return _step("check", timeout, on_error, selector=selector, frame=frame)


def uncheck(
    selector: str, *, frame: _FRAME = None, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Unchecks a checkbox, if it's checked: ``page.uncheck``."""
    return _step("uncheck", timeout, on_error, selector=selector, frame=frame)


def set_input_files(
    selector: str,
    files: str | os.PathLike[str] | Iterable[str | os.PathLike[str]],
    *,
    frame: _FRAME = None,
    timeout: float | None = None,
    on_error: str = "fail",
) -> dict[str, Any]:
    """Puts files in an ``<input type=file>``: ``page.set_input_files``.
    The paths are read when the step runs, on the machine running the
    crawl's Chrome."""
    files = str(files) if isinstance(files, (str, os.PathLike)) else [str(f) for f in files]
    return _step("set_input_files", timeout, on_error, selector=selector, files=files, frame=frame)


def evaluate(
    js: str, *, frame: _FRAME = None, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Runs JavaScript in the page, as ``page.evaluate`` does; its
    ``result`` is what it returned (a promise awaited)."""
    return _step("evaluate", timeout, on_error, js=js, frame=frame)


def screenshot(
    *, full_page: bool = False, timeout: float | None = None, on_error: str = "fail"
) -> dict[str, Any]:
    """Takes a PNG of the viewport, or the whole page; its ``result`` is
    the PNG's bytes."""
    return _step("screenshot", timeout, on_error, full_page=full_page or None)


def answer_dialogs(
    accept: bool = True, text: str | None = None, *, on_error: str = "fail"
) -> dict[str, Any]:
    """From here to the last step, answers every ``alert()``, ``confirm()``
    and ``prompt()``: OK with ``accept`` (``text`` in a prompt's box),
    Cancel without. They're dismissed otherwise."""
    return _step("answer_dialogs", None, on_error, accept=accept, text=text)


def call(
    method: str | Callable[..., Any],
    *,
    timeout: float | None = None,
    on_error: str = "fail",
) -> dict[str, Any]:
    """Runs a function of your own, with ``(page, request)``: the live
    ``BrowserPage`` and the request. ``method`` is a spider method or its
    name, which a checkpoint or a shared queue can keep; any other function
    works only while the request stays in this process. It may be ``async``,
    and what it returns, which must be JSON, is the step's ``result``. A
    plain function in a step list is the same as ``call(function)``."""
    if not (isinstance(method, str) or callable(method)):
        raise TypeError(f"call() takes a function or a spider method's name, not {method!r}")
    return _step("call", timeout, on_error, method=method)
