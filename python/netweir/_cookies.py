from __future__ import annotations

from collections.abc import Iterable, Iterator
from typing import Any

from netweir._native import Fetcher


class Cookies:
    """A Client's cookie jar.

    Each cookie is a dict, as ``page.cookies()`` in Chrome gives them:
    ``name``, ``value``, ``domain`` (a leading dot for the domain and its
    subdomains, none for the host's own cookie), ``path``, ``expires``
    (seconds since the epoch, -1 for a session cookie), ``http_only``,
    ``secure``, ``same_site`` and ``partition_key``. Dicts in Playwright's
    spelling (``httpOnly``, ``sameSite``) go in too, so cookies move
    between Chrome and the HTTP client in one line::

        client.cookies.add(await page.cookies())
        await context.set_cookies(client.cookies.all())
    """

    def __init__(self, fetcher: Fetcher):
        self._fetcher = fetcher

    def get(
        self, name: str, domain: str | None = None, path: str | None = None, default: Any = None
    ) -> str | None:
        """The value of the cookie ``name`` (of ``domain`` and ``path``, if
        given), or ``default``. The oldest wins when several match."""
        found = self._fetcher.find_cookies(name, domain, path)
        return found[0]["value"] if found else default

    def set(
        self,
        name: str,
        value: str,
        domain: str,
        path: str = "/",
        *,
        expires: float = -1,
        secure: bool = False,
        http_only: bool = False,
        same_site: str | None = None,
    ) -> None:
        """Sets a cookie: ``domain``'s own, or with a leading dot the
        domain's and its subdomains'."""
        self.add(
            [
                {
                    "name": name,
                    "value": value,
                    "domain": domain,
                    "path": path,
                    "expires": expires,
                    "secure": secure,
                    "http_only": http_only,
                    "same_site": same_site,
                }
            ]
        )

    def delete(self, name: str, domain: str | None = None, path: str | None = None) -> int:
        """Removes the cookies named ``name`` (of ``domain`` and ``path``, if
        given); how many."""
        return self._fetcher.remove_cookies(name, domain, path)

    def clear(self, domain: str | None = None) -> int:
        """Removes every cookie, or ``domain``'s and its subdomains'; how
        many."""
        return self._fetcher.clear_cookies(domain)

    def all(self) -> list[dict[str, Any]]:
        """Every cookie, oldest first."""
        return self._fetcher.cookies()

    def add(self, cookies: Iterable[dict[str, Any]]) -> None:
        """Adds cookie dicts, from ``all()``, a browser or a saved state:
        all of them, or ValueError, saying which cookie and why, and none.
        One Chrome would refuse is refused here too (a ``__Host-`` name
        that isn't secure, say), as it would fail every page it went to."""
        self._fetcher.add_cookies(list(cookies))

    def __iter__(self) -> Iterator[dict[str, Any]]:
        return iter(self.all())

    def __len__(self) -> int:
        return len(self.all())

    def __repr__(self) -> str:
        # Names and domains only: values are secrets.
        names = ", ".join(f"{c['name']}@{c['domain']}" for c in self.all())
        return f"<Cookies [{names}]>"
