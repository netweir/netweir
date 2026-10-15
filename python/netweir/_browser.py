"""Driving Chrome: ``netweir.browser()``."""

from __future__ import annotations

import os
from collections.abc import Generator, Sequence
from typing import Any

from netweir._native import Browser, launch


class browser:
    """Starts Chrome. Use it either way:

    .. code-block:: python

        async with netweir.browser() as b:
            page = await b.new_page()

        b = await netweir.browser()
        ...
        await b.close()

    Without ``executable``, it uses ``$NETWEIR_CHROME``, then the newest
    Chrome ``netweir install chrome`` installed, then the usual install
    paths for Chrome, Chrome for Testing and Chromium. ``timeout``
    (seconds) is the default limit for navigations and actions. ``proxy``
    (``http://``, ``https://`` or ``socks5://``) carries everything Chrome
    fetches; an HTTP(S) proxy's URL may hold a login, a SOCKS proxy's
    can't.

    ``connect`` drives a browser that's already running instead of
    starting one: a ``ws://`` DevTools URL, or ``http://host:port``.
    Closing it closes the pages Netweir opened and leaves the browser
    running.

    ``user_data_dir`` is a profile directory to keep: cookies and storage
    survive to the next launch with the same directory, and it's never
    deleted. A leading ``~`` is your home directory. Pages from ``new_page()`` then share that profile.
    Chrome's "Preload pages" setting is turned off in it, and stays off.

    ``dialog_pause`` is how long, in seconds, a person takes to react:
    no dialog is answered sooner after it opened, and a tab brought back
    to the front waits as long before it's used, a time picked between
    the two each time. Shorter is faster, and a page that times how long
    ``confirm()`` kept it waiting can tell.
    """

    def __init__(
        self,
        executable: str | os.PathLike[str] | None = None,
        *,
        headless: bool = True,
        args: Sequence[str] = (),
        timeout: float = 30.0,
        proxy: str | None = None,
        connect: str | None = None,
        user_data_dir: str | os.PathLike[str] | None = None,
        dialog_pause: tuple[float, float] = (0.5, 1.5),
    ):
        self._options: dict[str, Any] = {
            "executable": None if executable is None else os.fspath(executable),
            "headless": headless,
            "args": list(args),
            "timeout": timeout,
            "proxy": proxy,
            "connect": connect,
            "user_data_dir": (
                None if user_data_dir is None else os.path.expanduser(os.fspath(user_data_dir))
            ),
            "dialog_pause": tuple(dialog_pause),
        }
        self._browser: Browser | None = None

    def __await__(self) -> Generator[Any, None, Browser]:
        return launch(**self._options).__await__()

    async def __aenter__(self) -> Browser:
        self._browser = await self
        return self._browser

    async def __aexit__(self, *exc: object) -> None:
        if self._browser is not None:
            await self._browser.close()
