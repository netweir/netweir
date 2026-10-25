"""Exporters: pipeline stages that write every item to a file.

    class Books(netweir.Spider):
        pipelines = [drop_out_of_stock, netweir.export.jsonl("books.jsonl")]

Serialising happens in Rust. A crawl opens its exporters when it starts
and closes them when it ends; each run writes the file afresh, except a run
that resumes from a checkpoint, or one with ``deltafetch``, which adds to it. Used on its own, an
exporter opens its file at the first item; close it to finish the file.

A path such as ``s3://bucket/books.jsonl`` is written locally and uploaded
through fsspec when the file is finished. ``batch=N`` starts a new file
every N items, named by ``{batch}`` (and ``{time}``) in the path.
"""

from __future__ import annotations

import contextlib
import csv as _csv
import datetime
import logging
import os
import shutil
import string
import tempfile
import urllib.parse
import urllib.request
from typing import Any

from netweir._native import CsvWriter, JsonlWriter, JsonWriter, ParquetWriter, XmlWriter

log = logging.getLogger("netweir")

#: The package each common filesystem needs alongside fsspec.
_FS_PACKAGES = {"s3": "s3fs", "gs": "gcsfs", "gcs": "gcsfs", "az": "adlfs", "abfs": "adlfs"}


def _sync_dir(directory: str) -> None:
    """Asks the OS to keep a directory's entries (a file created or renamed
    in it). Not on Windows, which has no such call for a directory."""
    if os.name == "nt":
        return
    fd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _remote(path: str) -> bool:
    return "://" in path and not path.lower().startswith("file://")


def _fsspec(path: str) -> Any:
    try:
        import fsspec
    except ImportError:
        scheme = path.split("://", 1)[0].lower()
        extra = f" {_FS_PACKAGES[scheme]}" if scheme in _FS_PACKAGES else ""
        raise ImportError(
            f"writing to {path} needs fsspec and the filesystem's package: "
            f"pip install fsspec{extra}"
        ) from None
    return fsspec


def _camel(value: Any) -> Any:
    """A dict's keys, at every depth, in camelCase (``regular_price`` to
    ``regularPrice``), as the common item schema writes them."""
    if isinstance(value, dict):
        return {_camel_key(k): _camel(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_camel(v) for v in value]
    return value


def _camel_key(key: Any) -> Any:
    if not isinstance(key, str) or "_" not in key.strip("_"):
        return key
    first, *rest = key.split("_")
    return first + "".join(part[:1].upper() + part[1:] for part in rest)


class _Exporter:
    """Writes items to ``path``. The file is opened by ``start`` (a crawl
    calls it) or at the first item, so defining an exporter doesn't touch
    the file. Closing finishes it; the next start begins it afresh."""

    def __init__(self, path: str, batch: int | None = None, field_names: str = "snake"):
        if field_names not in ("snake", "zyte"):
            raise ValueError(f'field_names is "snake" or "zyte", not {field_names!r}')
        self.field_names = field_names
        if path.lower().startswith("file://"):
            path = urllib.request.url2pathname(urllib.parse.urlparse(path).path)
        self.remote = _remote(path)
        if self.remote:
            # The filesystem's own package and the scheme are found now, so a
            # missing s3fs fails before the crawl, not after it.
            fsspec = _fsspec(path)
            try:
                fsspec.core.url_to_fs(path)
            except ImportError as e:
                raise ImportError(f"writing to {path}: {e}") from None
            except ValueError as e:
                raise ValueError(f"can't write to {path}: {e}") from None
        else:
            directory = os.path.dirname(os.path.abspath(path))
            if not os.path.isdir(directory):
                raise ValueError(f"can't write {path}: {directory} is not a directory")
        if batch is not None:
            if batch < 1:
                raise ValueError(f"batch is how many items go in each file, 1 or more, not {batch}")
            try:
                fields = {f for _, f, _, _ in string.Formatter().parse(path) if f is not None}
                path.format(batch=1, time="")
            except (KeyError, IndexError, ValueError):
                fields = None
            if fields is None or not fields <= {"batch", "time"}:
                raise ValueError(
                    f"{path!r}: with batch=, only {{batch}} and {{time}} may be in braces;"
                    " write other braces twice ({{ and }})"
                )
            if "batch" not in fields:
                raise ValueError(
                    f"with batch=, put {{batch}} in the path ({path!r}), "
                    "or every batch overwrites the last"
                )
        self.path = path
        self.batch = batch
        self._append = False
        self._writer: Any = None
        #: Closed since it was last started: closing again does nothing.
        self._done = False
        #: Batches begun so far, and items in the one being written.
        self._number = 0
        self._count = 0
        #: Items written since it was made: the crawl counts one an
        #: interrupt cut off after it was written.
        self.written = 0
        #: Where the file being written ends up, and where it's written.
        self._target = ""
        self._file = ""

    def start(self, append: bool = False) -> None:
        """Opens the file: emptied, or with ``append`` added to."""
        if self._writer is not None:
            self.close()
        self._append = append
        self._number = 0
        self._done = False
        self._writer = self._begin()

    def _begin(self) -> Any:
        """Opens the next file: this batch's, or the only one."""
        self._number += 1
        self._count = 0
        self._target = self.path
        if self.batch is not None:
            while True:
                started = datetime.datetime.now(datetime.timezone.utc)
                self._target = self.path.format(
                    batch=self._number, time=started.strftime("%Y-%m-%dT%H-%M-%SZ")
                )
                # Started with ``append``, batches go beside the files already
                # there, numbered on from them, never over one.
                if not (self._append and self._exists(self._target)):
                    break
                self._number += 1
        if self.remote:
            # Written here, in Rust, and uploaded when finished.
            suffix = os.path.splitext(urllib.parse.urlparse(self._target).path)[1]
            handle, self._file = tempfile.mkstemp(prefix="netweir-", suffix=suffix)
            os.close(handle)
        else:
            self._file = self._target
        return self._open()

    def _exists(self, target: str) -> bool:
        if not self.remote:
            return os.path.exists(target) or os.path.exists(target + ".partial")
        fs, path = _fsspec(target).core.url_to_fs(target)
        return fs.exists(path)

    def _resume_length(self) -> int:
        """How long the file will be once started with ``append``."""
        return os.path.getsize(self.path) if os.path.exists(self.path) else 0

    def _open(self) -> Any:
        """The writer for ``self._file``."""
        raise NotImplementedError

    def _head(self) -> bytes:
        """How every file of this format starts, if they all start alike:
        a resumed crawl checks it before cutting the file back."""
        return b""

    def _write(self, item: Any) -> Any:
        self._done = False
        if self._writer is None:
            self._writer = self._begin()
        if self.field_names == "zyte":
            item = _camel(item)
        result = self._writer.write(item)
        self.written += 1
        self._count += 1
        if self.batch is not None and self._count >= self.batch:
            # Full: finished now, so files appear as the crawl goes.
            writer, self._writer = self._writer, None
            self._finish(writer)
        return result

    def __call__(self, item: Any) -> Any:
        self._write(item)
        return item

    def flush(self) -> bool:
        """Puts what's written so far on disk. False if this format can't
        (Parquet is only readable once closed)."""
        if self._writer is not None:
            self._writer.flush()
        return True

    def close(self) -> None:
        """Finishes the file; one that never got an item is written empty
        (a CSV with ``fields`` gets its header). With batches, a crawl with
        no items still gets the first batch, empty."""
        writer, self._writer = self._writer, None
        if self._done:
            return  # closed already: closing again mustn't empty the file
        self._done = True
        if writer is None:
            if self._number and self.batch is not None:
                return  # the last batch was full, and is finished
            writer = self._begin()
        self._finish(writer)

    def _finish(self, writer: Any) -> None:
        if self._append and self._count == 0 and self.batch is not None:
            # Adding to what's there, and nothing came: no empty batch.
            self._discard(writer)
            return
        self._closed(writer.close())
        self._finished()

    def _discard(self, writer: Any) -> None:
        """Closes the file begun for a batch that got no items, and removes
        it."""
        writer.close()
        with contextlib.suppress(FileNotFoundError):
            os.remove(self._file)

    def _closed(self, report: Any) -> None:
        pass

    def _finished(self) -> None:
        """The file is complete: uploaded, if it goes somewhere remote."""
        if self.remote:
            self._upload(self._file)

    def _upload(self, local: str) -> None:
        fsspec = _fsspec(self.path)
        try:
            with open(local, "rb") as source, fsspec.open(self._target, "wb") as target:
                shutil.copyfileobj(source, target)
        except Exception as e:
            # The local copy is all there is: kept, and said where.
            raise OSError(
                f"uploading to {self._target} failed ({e}); the file is at {local}"
            ) from e
        os.remove(local)
        log.info("uploaded %s", self._target)


class jsonl(_Exporter):  # noqa: N801 - reads as a function in a pipeline list
    """One JSON object per line."""

    def _open(self) -> JsonlWriter:
        return JsonlWriter(self._file, self._append)


class json(_Exporter):  # noqa: N801
    """One JSON array, an item a line, as Scrapy writes it. Valid once
    closed; a resumed crawl goes on after the last item."""

    def _open(self) -> JsonWriter:
        return JsonWriter(self._file, self._append)

    def _resume_length(self) -> int:
        # Without its closing "]".
        return JsonWriter.resume_length(self.path)

    def _head(self) -> bytes:
        return b"["


class xml(_Exporter):  # noqa: N801
    """An XML document laid out as Scrapy's: an ``item`` element per item,
    inside a ``root`` element. A dict's keys are elements, a list's values
    ``<value>`` elements; a key that isn't a valid element name has the
    characters a name can't hold replaced with ``_``."""

    def __init__(
        self,
        path: str,
        root: str = "items",
        item: str = "item",
        batch: int | None = None,
        field_names: str = "snake",
    ):
        self.root = root
        self.item = item
        super().__init__(path, batch, field_names)

    def _open(self) -> XmlWriter:
        return XmlWriter(self._file, self.root, self.item, self._append)

    def _resume_length(self) -> int:
        # Without its closing root tag.
        return XmlWriter.resume_length(self.path, self.root)

    def _head(self) -> bytes:
        return b'<?xml version="1.0" encoding="utf-8"?>\n<'


class csv(_Exporter):  # noqa: N801
    """A CSV file. Columns are ``fields`` if given, otherwise the first
    item's keys; keys outside them are left out, with one warning."""

    def __init__(
        self,
        path: str,
        fields: list[str] | None = None,
        batch: int | None = None,
        field_names: str = "snake",
    ):
        self.fields = fields
        super().__init__(path, batch, field_names)

    def _open(self) -> CsvWriter:
        self._warned: set[str] = set()
        return CsvWriter(self._file, self.fields, self._append)

    def _check_columns(self) -> None:
        """Before a crawl adds to the file: ``fields``, if given, must be
        the columns its header already has."""
        if self.fields is None or not os.path.exists(self.path):
            return
        with open(self.path, newline="", encoding="utf-8-sig", errors="replace") as f:
            header = next(_csv.reader(f), None)
        if header is not None and header != list(self.fields):
            raise ValueError(
                f"can't add to {self.path}: its columns are {header}, not the fields given"
                f" ({list(self.fields)}); write to another file, or give the same fields"
            )

    def __call__(self, item: Any) -> Any:
        written = self._write(item)
        if written is None:
            if "" not in self._warned:
                self._warned.add("")
                log.warning(
                    "%s: an item with no fields was left out: it has no columns to start"
                    " the file with",
                    self.path,
                )
            return item
        # A checkpoint's _id is only a column if fields= asks for it.
        extra = set(written) - self._warned - {"_id"}
        if extra:
            self._warned |= extra
            log.warning(
                "%s: no column for %s; pass fields= to include them",
                self.path,
                ", ".join(sorted(extra)),
            )
        return item


class parquet(_Exporter):  # noqa: N801
    """A Parquet file. Columns and their types (bool, 64-bit int, double
    or text) come from the first 1,000 items: a key holding both ints and
    floats is a double column, any other mix is text, and lists and dicts
    are stored as JSON text.

    Later values that don't fit are stored as null, and later keys with no
    column are left out; closing logs a warning for each.
    """

    #: Set by a crawl with a checkpoint: a closed file stays under its
    #: .partial name until the crawl has recorded its items, then it calls
    #: publish(). Otherwise closing publishes it.
    hold: bool = False
    #: The finished file's name once closed and not yet published.
    pending: str | None = None

    def _part(self, path: str) -> str:
        """Where this run's file goes: ``path``, or with ``append`` and the
        path taken, the next free part (books.1.parquet, books.2.parquet ...),
        which readers such as pyarrow take together as one dataset."""
        if self._append and os.path.exists(path) and os.path.getsize(path) > 0:
            stem = path[: -len(".parquet")] if path.endswith(".parquet") else path
            n = 1
            while os.path.exists(f"{stem}.{n}.parquet"):
                n += 1
            log.info("%s exists: this run's items go to %s", path, f"{stem}.{n}.parquet")
            path = f"{stem}.{n}.parquet"
        return path

    def _open(self) -> ParquetWriter:
        # Written under a temporary name, so a file half-written when the
        # process died is never mistaken for a finished one.
        self._final = self._part(self._file)
        return ParquetWriter(self._final + ".partial")

    def _finish(self, writer: Any) -> None:
        if self._append and self._count == 0:
            # Adding a part, and nothing came: no empty part.
            self._discard(writer)
            return
        super()._finish(writer)

    def _discard(self, writer: Any) -> None:
        writer.close()
        with contextlib.suppress(FileNotFoundError):
            os.remove(self._final + ".partial")

    def _finished(self) -> None:
        self.pending = self._final
        # On disk under its .partial name before a crawl records it.
        _sync_dir(os.path.dirname(os.path.abspath(self._final)))
        if not self.hold:
            self.publish()
        if self.remote:
            self._upload(self._final)

    def publish(self) -> None:
        """Gives the closed file its real name."""
        if self.pending is not None:
            os.replace(self.pending + ".partial", self.pending)
            _sync_dir(os.path.dirname(os.path.abspath(self.pending)))
            self.pending = None

    def flush(self) -> bool:
        return False

    def _closed(self, report: list[tuple[str, int, int]]) -> None:
        for key, missing, wrong in report:
            if missing:
                log.warning("%s: no column for %r, left out of %d items", self.path, key, missing)
            if wrong:
                log.warning(
                    "%s: %d values of %r didn't fit the column type and were stored as null",
                    self.path,
                    wrong,
                    key,
                )


def to_path(path: str, batch: int | None = None, field_names: str = "snake"):
    """The exporter for a file name's extension: .jsonl/.jl, .json, .xml,
    .csv or .parquet."""
    lower = path.lower()
    for endings, exporter in [
        ((".jsonl", ".jl"), jsonl),
        ((".json",), json),
        ((".xml",), xml),
        ((".csv",), csv),
        ((".parquet",), parquet),
    ]:
        if lower.endswith(endings):
            return exporter(path, batch=batch, field_names=field_names)
    raise ValueError(
        f"can't tell the format of {path!r}: use .jsonl, .json, .xml, .csv or .parquet"
    )
