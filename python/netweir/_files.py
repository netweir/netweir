"""Pipeline stages that download the files and images items link to.

An item lists URLs in ``file_urls`` (or ``image_urls``); the stage fetches
each through the running crawl, so robots.txt, the per-host delay and
concurrency, the session, proxies, retries, the cache and the archive all
apply, stores it once under a name made from its URL, and puts what it did
on the item. The layout and field names are Scrapy's FilesPipeline's and
ImagesPipeline's, so items and stores carry over. See docs/design/files.md.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import dataclasses
import datetime
import glob
import hashlib
import io
import logging
import os
import posixpath
import re
import tempfile
import time
from typing import Any
from urllib.parse import unquote, urlsplit
from urllib.request import url2pathname

from netweir import export

log = logging.getLogger("netweir")

#: The extensions a stored file's name may have, each with the type a
#: Content-Type names it by; the first of a type's extensions is the one
#: it's given. Netweir's own, not Python's mimetypes, whose table differs
#: between versions (3.10 knows neither .webp nor .avif) and machines (it
#: reads the system's): a URL is named the same everywhere, so a store
#: filled on one is up to date on another. Script extensions such as .php
#: aren't here: what download.php sends is a file of its own type.
_EXTENSIONS = {
    "pdf": "application/pdf",
    "jpg": "image/jpeg",
    "jpeg": "image/jpeg",
    "png": "image/png",
    "gif": "image/gif",
    "webp": "image/webp",
    "avif": "image/avif",
    "jxl": "image/jxl",
    "heic": "image/heic",
    "svg": "image/svg+xml",
    "bmp": "image/bmp",
    "tif": "image/tiff",
    "tiff": "image/tiff",
    "ico": "image/x-icon",
    "mp4": "video/mp4",
    "webm": "video/webm",
    "mov": "video/quicktime",
    "mp3": "audio/mpeg",
    "m4a": "audio/mp4",
    "wav": "audio/wav",
    "ogg": "audio/ogg",
    "txt": "text/plain",
    "csv": "text/csv",
    "html": "text/html",
    "htm": "text/html",
    "xml": "application/xml",
    "json": "application/json",
    "zip": "application/zip",
    "gz": "application/gzip",
    "tar": "application/x-tar",
    "7z": "application/x-7z-compressed",
    "rtf": "application/rtf",
    "epub": "application/epub+zip",
    "doc": "application/msword",
    "docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "xls": "application/vnd.ms-excel",
    "xlsx": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "ppt": "application/vnd.ms-powerpoint",
    "pptx": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "odt": "application/vnd.oasis.opendocument.text",
    "ods": "application/vnd.oasis.opendocument.spreadsheet",
    "woff": "font/woff",
    "woff2": "font/woff2",
    "ttf": "font/ttf",
    "otf": "font/otf",
}

#: A type's extension, the first the table gives it; and a few other names
#: servers use for the same types.
_BY_TYPE = {
    **{t: e for e, t in reversed(_EXTENSIONS.items())},
    "image/jpg": "jpg",
    "image/pjpeg": "jpg",
    "image/vnd.microsoft.icon": "ico",
    "image/x-ms-bmp": "bmp",
    "audio/x-wav": "wav",
    "audio/mp3": "mp3",
    "text/xml": "xml",
    "application/x-gzip": "gz",
    "application/x-zip-compressed": "zip",
}

#: What a thumbnail's name, a directory in the store, may be.
_THUMB_NAME = re.compile(r"[A-Za-z0-9_-]{1,64}")


def _extension_of_url(url: str) -> str:
    """The URL's extension if it's one of _EXTENSIONS, else ""."""
    ext = posixpath.splitext(unquote(urlsplit(url).path))[1].lower()[1:]
    return ext if ext in _EXTENSIONS else ""


def _extension_of_type(content_type: str | None) -> str:
    """The extension _EXTENSIONS gives a Content-Type, else ""."""
    if not content_type:
        return ""
    return _BY_TYPE.get(content_type.split(";")[0].strip().lower(), "")


def _get(item: Any, field: str) -> Any:
    if isinstance(item, dict):
        return item.get(field)
    return getattr(item, field, None)


def _set(item: Any, field: str, value: Any) -> None:
    """Sets ``field`` on a dict, or on a dataclass that has it; an item of
    another kind keeps its fields as they are, as Scrapy leaves an Item
    without the field."""
    if isinstance(item, dict):
        item[field] = value
    elif dataclasses.is_dataclass(item) and any(f.name == field for f in dataclasses.fields(item)):
        setattr(item, field, value)


class _Skip(Exception):
    """An image that downloaded but is below min_size, so isn't stored."""


class _Refused(Exception):
    """A download that worked but can't be stored, and why."""


class _Store:
    """A directory, local or in fsspec's reach, that files go in by their
    path inside it ("full/<sha1>.pdf"). Every call blocks: run them in a
    thread."""

    def __init__(self, store: Any):
        self.url = os.fspath(store)
        self.remote = export._remote(self.url)
        if self.remote:
            # Resolved now, so a missing fsspec or s3fs fails before the
            # crawl, as an exporter's does.
            self.fs, root = export._fsspec(self.url).core.url_to_fs(self.url)
            self.root = root.rstrip("/")
        elif self.url.lower().startswith("file://"):
            # file:///C:/x is C:\x on Windows, and file:///tmp/x is /tmp/x.
            self.root = url2pathname(urlsplit(self.url).path)
        else:
            self.root = self.url

    def _path(self, relative: str) -> str:
        if self.remote:
            return f"{self.root}/{relative}"
        return os.path.join(self.root, *relative.split("/"))

    def find(self, stem: str) -> str | None:
        """The relative path of a stored file named ``stem`` with or
        without an extension, as one stored from a URL without one is
        named after its Content-Type."""
        pattern = self._path(stem)
        if self.remote:
            prefix = self.root + "/"
            names = [p[len(prefix) :] for p in self.fs.glob(pattern + "*") if p.startswith(prefix)]
        else:
            found = glob.glob(glob.escape(pattern) + "*")
            names = [os.path.relpath(p, self.root).replace(os.sep, "/") for p in found]
        for name in sorted(names):
            if name == stem or (name.startswith(stem + ".") and "/" not in name[len(stem) :]):
                return name
        return None

    def age(self, relative: str) -> float | None:
        """Seconds since the file was stored, or None if it isn't there or
        the filesystem doesn't say when it was written."""
        path = self._path(relative)
        if not self.remote:
            try:
                return time.time() - os.stat(path).st_mtime
            except FileNotFoundError:
                return None
        if not self.fs.exists(path):
            return None
        try:
            modified = self.fs.modified(path)
        except (NotImplementedError, KeyError, OSError):
            return None
        if modified.tzinfo is None:
            modified = modified.replace(tzinfo=datetime.UTC)
        return (datetime.datetime.now(datetime.UTC) - modified).total_seconds()

    def checksum(self, relative: str) -> str | None:
        """The MD5 of a stored file. A local one is read for it; a remote
        one isn't downloaded for it: the checksum is what the store's own
        record of the object says (S3's ETag for an object uploaded in one
        part, Google Cloud Storage's md5Hash), or None where it says
        nothing usable."""
        if not self.remote:
            digest = hashlib.md5()
            with open(self._path(relative), "rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    digest.update(chunk)
            return digest.hexdigest()
        info = self.fs.info(self._path(relative))
        for key in ("ETag", "etag", "e_tag"):
            tag = str(info.get(key) or "").strip('"').lower()
            # A multipart upload's ETag isn't the MD5 of the object.
            if re.fullmatch(r"[0-9a-f]{32}", tag):
                return tag
        md5 = info.get("md5Hash")
        if md5:
            try:
                return base64.b64decode(md5).hex()
            except ValueError:
                return None
        return None

    def write(self, relative: str, data: bytes) -> None:
        """Stores ``data`` whole: a crawl that stops part way never leaves
        half a file under the final name."""
        path = self._path(relative)
        if self.remote:
            self.fs.pipe_file(path, data)
            return
        directory = os.path.dirname(path)
        os.makedirs(directory, exist_ok=True)
        fd, temporary = tempfile.mkstemp(dir=directory, prefix=".netweir-")
        try:
            with os.fdopen(fd, "wb") as f:
                f.write(data)
            os.replace(temporary, path)
        except BaseException:
            with contextlib.suppress(OSError):
                os.remove(temporary)
            raise


class _Media:
    #: "fetch" or "image": what the browser profile sends for it.
    kind = "fetch"
    #: Stats this stage adds to the crawl's.
    counters = ("files_downloaded", "files_cached", "files_uptodate", "files_failed")
    #: Files stored at once, on threads; images hold their pixels meanwhile.
    workers = 16

    def __init__(
        self,
        store: Any,
        urls_field: str,
        result_field: str,
        expires: float | datetime.timedelta | None,
        per_jar: bool = False,
    ):
        if isinstance(expires, datetime.timedelta):
            expires = expires.total_seconds()
        if expires is not None and expires <= 0:
            raise ValueError("expires must be positive, or None for never")
        self.store = _Store(store)
        self.urls_field = urls_field
        self.result_field = result_field
        self.expires = expires
        self.per_jar = per_jar
        self._run: Any = None
        #: Each URL's download in this crawl, by URL (and with per_jar, the
        #: item's jar): one for every item that names it, made the first
        #: time one does.
        self._tasks: dict[tuple[str, str | None], asyncio.Future] = {}

    def __repr__(self) -> str:
        return f"netweir.{type(self).__name__}({self.store.url!r})"

    def _attach(self, run: Any) -> None:
        """Called by the crawl before it opens: what to fetch through, and
        where to count. A stage used by another crawl starts afresh."""
        self._run = run
        self._tasks = {}
        #: Images converted at once, each holding its pixels in memory;
        #: made here, for this crawl's event loop.
        self._slots = asyncio.Semaphore(self.workers)
        for name in self.counters:
            run.counts.setdefault(name, 0)

    async def process_item(self, item: Any, spider: Any) -> Any:
        if self._run is None:
            raise RuntimeError(f"{self!r} runs as a pipeline stage of a crawl")
        urls = _get(item, self.urls_field) or []
        if isinstance(urls, str):
            urls = [urls]
        referer = self._referer()
        # In the jar of the request the item's page came from, so a file
        # behind a login comes with the login. Once per URL, as Scrapy
        # stores it; with per_jar, once per URL and jar, each kept apart.
        jar = self._run.page_jar
        key = jar if self.per_jar else None
        tasks = []
        for url in urls:
            task = self._tasks.get((url, key))
            if task is None:
                task = asyncio.ensure_future(self._get(url, referer, jar))
                self._tasks[(url, key)] = task
            tasks.append(task)
        results = await asyncio.gather(*tasks)
        # Copies: a later stage changing one item's entry leaves another's.
        _set(item, self.result_field, [dict(r) for r in results if r is not None])
        return item

    def _referer(self) -> str | None:
        """The page the item came from, sent as the Referer an <img> on it
        would send."""
        url = self._run.page_url
        return url if url and url.startswith(("http://", "https://")) else None

    def _failed(self, url: str, why: str) -> None:
        self._run.counts["files_failed"] += 1
        log.warning("%s: not downloaded: %s", url, why)

    async def _get(
        self, url: str, referer: str | None, jar: str | None = None
    ) -> dict[str, Any] | None:
        """The URL's entry, or None when it failed, which is logged and
        counted. Never raises (but for cancelling): a store or a Pillow that
        fails in a way of its own (a cloud SDK's error, a MemoryError) fails
        this file, not the item or the page's other items, and the task
        every later item naming the URL awaits holds None, not the error."""
        if not isinstance(url, str) or urlsplit(url).scheme.lower() not in ("http", "https"):
            self._failed(str(url), "not an absolute http(s) URL")
            return None
        try:
            return await self._fetch(url, referer, jar)
        except Exception as e:  # noqa: BLE001 - logged and counted; see above
            self._failed(url, f"{type(e).__name__}: {e}")
            return None

    async def _fetch(
        self, url: str, referer: str | None, jar: str | None = None
    ) -> dict[str, Any] | None:
        # Scrapy's name, the URL's SHA-1; with per_jar, a named jar's file
        # is kept apart, as behind a login each user's may differ.
        key = url if jar is None or not self.per_jar else f"{jar}\0{url}"
        name = hashlib.sha1(key.encode()).hexdigest()
        stored = await asyncio.to_thread(self._fresh, name, url)
        if stored is not None:
            path, checksum = stored
            self._run.counts["files_uptodate"] += 1
            return {"url": url, "path": path, "checksum": checksum, "status": "uptodate"}
        outcome = await self._run.download(url, self.kind, referer, jar)
        if outcome is None:
            self._failed(url, "the crawl stopped first")
            return None
        if outcome[0] == "failed":
            self._failed(url, str(outcome[1]))
            return None
        if outcome[0] == "dropped":
            why = "robots.txt disallows it" if outcome[1] == "robots" else "TDM rights reserved"
            self._failed(url, why)
            return None
        if outcome[0] == "blocked":
            self._failed(url, f"blocked by {outcome[1]} ({outcome[2]})")
            return None
        _, response, cached = outcome
        if not 200 <= response.status < 300:
            self._failed(url, f"HTTP {response.status}")
            return None
        if not response.body:
            self._failed(url, "the response was empty")
            return None
        try:
            async with self._slots:
                path, checksum = await asyncio.to_thread(self._keep, name, url, response)
        except _Skip as e:
            # Counted here, on the event loop, not in the thread.
            self._run.counts["images_too_small"] += 1
            log.info("%s: not stored: %s", url, e)
            return None
        except _Refused as e:
            self._failed(url, str(e))
            return None
        except Exception as e:  # noqa: BLE001 - any store's or Pillow's error
            self._failed(url, f"storing it failed: {type(e).__name__}: {e}")
            return None
        status = "cached" if cached else "downloaded"
        self._run.counts[f"files_{status}"] += 1
        return {"url": url, "path": path, "checksum": checksum, "status": status}

    def _fresh(self, name: str, url: str) -> tuple[str, str | None] | None:
        """The stored file's path and checksum (see _Store.checksum), if
        it's there and younger than ``expires``. A store that can't be
        asked means fetching it."""
        try:
            path = self._stored(name, url)
            if path is None:
                return None
            age = self.store.age(path)
            if age is None or (self.expires is not None and age >= self.expires):
                return None
            return path, self.store.checksum(path)
        except Exception as e:  # noqa: BLE001 - fetched instead
            log.warning("%s: couldn't ask %s for it (%s); fetching it", url, self.store.url, e)
            return None

    def _stored(self, name: str, url: str) -> str | None:
        ext = _extension_of_url(url)
        if ext:
            return f"full/{name}.{ext}"
        return self.store.find(f"full/{name}")

    def _keep(self, name: str, url: str, response: Any) -> tuple[str, str]:
        # The engine's response: headers as (lowercase name, value) pairs.
        content_type = next((v for k, v in response.headers if k == "content-type"), None)
        ext = _extension_of_url(url) or _extension_of_type(content_type)
        path = f"full/{name}.{ext}" if ext else f"full/{name}"
        self.store.write(path, response.body)
        return path, hashlib.md5(response.body).hexdigest()


class files(_Media):  # noqa: N801 - reads as a function in a pipeline list
    """A pipeline stage that downloads the URLs an item lists in
    ``urls_field`` through the crawl, stores each under
    ``full/<sha1 of the URL>.<ext>`` in ``store`` (a directory, or an
    fsspec URL such as ``s3://bucket/files``), and sets ``result_field``
    to a list of ``{"url", "path", "checksum", "status"}``, one for each
    file it has. A file stored less than ``expires`` ago (seconds, or a
    timedelta; None for never) isn't fetched again. A download that fails
    is logged and left out; the item goes on either way. Each file is
    fetched in the cookie jar of the request whose page gave the item;
    with ``per_jar``, a named jar's copy is fetched and stored apart, at
    ``full/<sha1 of the jar's name, a NUL and the URL>.<ext>``."""

    def __init__(
        self,
        store: Any,
        *,
        urls_field: str = "file_urls",
        result_field: str = "files",
        expires: float | datetime.timedelta | None = datetime.timedelta(days=90),
        per_jar: bool = False,
    ):
        super().__init__(store, urls_field, result_field, expires, per_jar)


class images(_Media):  # noqa: N801
    """``files`` for images: each is converted to an RGB JPEG at
    ``full/<sha1>.jpg``, with a thumbnail at ``thumbs/<name>/<sha1>.jpg``
    for each of ``thumbs`` (name to the (width, height) it fits in, aspect
    ratio kept). An image narrower or shorter than ``min_size``
    ((width, height)) is skipped, and one of more than ``max_pixels``
    fails rather than be decoded. Two are converted at once. ``per_jar``
    is as for ``files``. Needs Pillow: pip install netweir[images]."""

    kind = "image"
    counters = (*_Media.counters, "images_too_small")
    # Each conversion holds up to a few times max_pixels * 4 bytes.
    workers = 2

    def __init__(
        self,
        store: Any,
        *,
        urls_field: str = "image_urls",
        result_field: str = "images",
        thumbs: dict[str, tuple[int, int]] | None = None,
        min_size: tuple[int, int] | None = None,
        max_pixels: int = 25_000_000,
        expires: float | datetime.timedelta | None = datetime.timedelta(days=90),
        per_jar: bool = False,
    ):
        try:
            import PIL.Image  # noqa: F401
        except ImportError:
            raise ImportError(
                "netweir.images needs Pillow, to convert images and make thumbnails:"
                " pip install netweir[images]"
            ) from None
        super().__init__(store, urls_field, result_field, expires, per_jar)
        self.thumbs = dict(thumbs or {})
        for thumb, size in self.thumbs.items():
            # The name is a directory in the store.
            if not isinstance(thumb, str) or not _THUMB_NAME.fullmatch(thumb):
                raise ValueError(
                    f"thumbs: {thumb!r} isn't a name a thumbnail can have (letters, digits,"
                    " _ and -)"
                )
            _check_size(f"thumbs[{thumb!r}]", size)
        if min_size is not None:
            _check_size("min_size", min_size, zero=True)
        self.min_size = min_size
        if not isinstance(max_pixels, int) or max_pixels < 1:
            raise ValueError(f"max_pixels is a number of pixels, not {max_pixels!r}")
        self.max_pixels = max_pixels

    def _stored(self, name: str, url: str) -> str | None:
        return f"full/{name}.jpg"

    def _keep(self, name: str, url: str, response: Any) -> tuple[str, str]:
        from PIL import Image

        try:
            # Reads only the header: the size is known before any pixel is.
            image = Image.open(io.BytesIO(response.body))
        except Exception as e:  # noqa: BLE001 - Pillow raises several kinds
            raise _Refused(f"not an image Pillow can read ({e})") from None
        width, height = image.size
        if width * height > self.max_pixels:
            raise _Refused(
                f"{width}x{height} is more than max_pixels ({self.max_pixels}) to decode"
            )
        try:
            image.load()
        except Exception as e:  # noqa: BLE001
            raise _Refused(f"not an image Pillow can read ({e})") from None
        if self.min_size is not None:
            min_width, min_height = self.min_size
            if width < min_width or height < min_height:
                raise _Skip(f"{width}x{height} is smaller than min_size {min_width}x{min_height}")
        image = _rgb(image)
        full = _jpeg(image)
        path = f"full/{name}.jpg"
        self.store.write(path, full)
        for thumb, size in self.thumbs.items():
            small = image.copy()
            small.thumbnail(size, Image.Resampling.LANCZOS)
            self.store.write(f"thumbs/{thumb}/{name}.jpg", _jpeg(small))
        return path, hashlib.md5(full).hexdigest()


def _check_size(name: str, size: Any, zero: bool = False) -> None:
    least = 0 if zero else 1
    if (
        not isinstance(size, (tuple, list))
        or len(size) != 2
        or not all(isinstance(n, int) and n >= least for n in size)
    ):
        raise ValueError(f"{name} is (width, height) in pixels, not {size!r}")


def _rgb(image: Any) -> Any:
    """The image as RGB, as a JPEG holds it: transparency over white."""
    from PIL import Image

    if image.mode in ("RGBA", "LA") or (image.mode == "P" and "transparency" in image.info):
        image = image.convert("RGBA")
        background = Image.new("RGB", image.size, (255, 255, 255))
        background.paste(image, mask=image.getchannel("A"))
        return background
    return image if image.mode == "RGB" else image.convert("RGB")


def _jpeg(image: Any) -> bytes:
    out = io.BytesIO()
    image.save(out, "JPEG")
    return out.getvalue()
