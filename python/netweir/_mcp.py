"""``netweir mcp``: Netweir's reading as tools for an AI agent, over the
Model Context Protocol on stdin and stdout. Needs the ``mcp`` package
(the ``netweir[mcp]`` extra)."""

from __future__ import annotations

import sys
from typing import Any

#: The auto-extracted types, by the name ``page_type()`` gives them.
_TYPES = (
    "product",
    "product-list",
    "product-navigation",
    "job-posting",
    "job-posting-navigation",
    "article",
    "article-list",
    "article-navigation",
    "forum-thread",
)

INSTRUCTIONS = """\
Netweir fetches web pages the way Chrome does and reads them. Start with
read_page for a page's text as Markdown. Use chunks to cut a long page
into pieces with their headings, extract for a product, job posting,
article, listing or forum thread as structured data, select for what a
CSS selector finds, and links for where a page leads. Set browser=true
for a page that builds itself in JavaScript; it's slower."""


def _module():
    try:
        from mcp.server import MCPServer
        from mcp.server.mcpserver.exceptions import ToolError
    except ImportError:
        raise ImportError("netweir mcp needs the MCP SDK: pip install 'netweir[mcp]'") from None
    return MCPServer, ToolError


def server(profile: str = "chrome", proxy: str | None = None, timeout: float = 30.0):
    """The MCP server, its tools fetching with a ``netweir.Client`` made
    from ``profile``, ``proxy`` and ``timeout``, and with one Chrome,
    started the first time a tool asks for it."""
    import netweir
    from netweir import Blocked, BrowserError, FetchError, auto
    from netweir._native import _response

    MCPServer, ToolError = _module()
    mcp = MCPServer("netweir", instructions=INSTRUCTIONS)
    client = netweir.Client(profile, proxy, timeout)
    chrome: dict[str, Any] = {}

    async def in_chrome(url: str) -> netweir.Page:
        if "browser" not in chrome:
            chrome["browser"] = await netweir.browser(proxy=proxy, timeout=timeout)
        tab = await chrome["browser"].new_page()
        try:
            response = await tab.goto(url)
            content = (await tab.content()).encode()
            return netweir.Page(
                _response(tab.url, response.status, "browser", response.headers, content)
            )
        finally:
            await tab.close()

    async def fetch(url: str, browser: bool) -> netweir.Page:
        if "://" not in url:
            url = "https://" + url
        try:
            page = await in_chrome(url) if browser else await client.get(url)
        except Blocked as e:
            raise ToolError(f"{url} is behind bot protection ({e}); try browser=true") from None
        except (FetchError, BrowserError) as e:
            raise ToolError(f"{url} couldn't be fetched: {e}") from None
        if page.status >= 400:
            raise ToolError(f"{url} answered {page.status}")
        return page

    @mcp.tool()
    async def read_page(
        url: str, main_only: bool = True, max_chars: int = 20_000, browser: bool = False
    ) -> dict[str, Any]:
        """A web page as Markdown: its main content (the article, post,
        product description or docs page, without menus, ads and
        comments), or the whole page with main_only=false or when it has
        no main content. Cut at max_chars, with truncated=true. Also its
        final URL, title and page_type (product, article, job-posting,
        ...; other when it's none)."""
        page = await fetch(url, browser)
        markdown = (page.markdown(main_only=True) if main_only else None) or page.markdown()
        return {
            "url": page.url,
            "title": page.css("title::text").get(),
            "page_type": page.page_type(),
            "markdown": markdown[:max_chars],
            "truncated": len(markdown) > max_chars,
        }

    @mcp.tool()
    async def chunks(
        url: str, max_chars: int = 2000, overlap: int = 0, browser: bool = False
    ) -> list[dict[str, Any]]:
        """A web page's main content as Markdown, cut at its headings into
        chunks of at most max_chars characters, each with its id, text,
        headings (the path of headings above it) and index."""
        page = await fetch(url, browser)
        try:
            return [c.to_dict() for c in page.chunks(max_chars, overlap=overlap)]
        except ValueError as e:
            raise ToolError(str(e)) from None

    @mcp.tool()
    async def extract(url: str, type: str = "auto", browser: bool = False) -> dict[str, Any]:
        """A web page read as structured data, without selectors: a
        product (name, price, currency, availability, brand, ...), a
        product-list, a job-posting, an article, an article-list, a
        forum-thread or a navigation page (its items and next page).
        type="auto" reads it as whatever it looks like."""
        if type != "auto" and type not in _TYPES:
            raise ToolError(f"type is auto or one of {', '.join(_TYPES)}, not {type!r}")
        page = await fetch(url, browser)
        if type == "auto":
            type = page.page_type()
            if type == "other":
                raise ToolError(
                    f"{url} doesn't look like a product, listing, job, article or thread; "
                    "give type to read it as one, or use read_page"
                )
        item = {name.lower(): cls for name, cls in vars(auto).items()}
        cls = item[type.replace("-", "")]
        return {"type": type, "item": cls.extract(page)}

    @mcp.tool()
    async def select(url: str, css: str, max_results: int = 50, browser: bool = False) -> list[str]:
        """What a CSS selector finds on a web page, up to max_results: each
        element's text as a reader sees it, or the strings ::text and
        ::attr(name) give."""
        page = await fetch(url, browser)
        try:
            found = page.css(css)
        except ValueError as e:
            raise ToolError(f"{css!r} isn't a selector Netweir reads: {e}") from None
        out = []
        for hit in found[:max_results]:
            out.append(hit if isinstance(hit, str) else hit.clean_text())
        return out

    @mcp.tool()
    async def links(
        url: str, max_results: int = 200, browser: bool = False
    ) -> list[dict[str, str]]:
        """The links on a web page, up to max_results, each with its
        absolute url and its text, in page order, without repeats."""
        page = await fetch(url, browser)
        seen: set[str] = set()
        out = []
        for a in page.css("a[href]"):
            href = page.urljoin(a.attrs.get("href", ""))
            if not href.startswith(("http://", "https://")) or href in seen:
                continue
            seen.add(href)
            out.append({"url": href, "text": a.clean_text()})
            if len(out) >= max_results:
                break
        return out

    return mcp


def main(args) -> int:
    """Runs the server on stdin and stdout until its client goes."""
    try:
        mcp = server(args.profile or "chrome", args.proxy, args.timeout)
    except (ImportError, ValueError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    mcp.run()
    return 0
