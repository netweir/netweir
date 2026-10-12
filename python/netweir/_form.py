"""Reading a form off a page as the browser would submit it."""

from __future__ import annotations

import dataclasses
import mimetypes
import os
import re
import secrets
import string
from collections.abc import Iterable, Mapping
from typing import TYPE_CHECKING, Any
from urllib.parse import urlencode, urljoin, urlsplit, urlunsplit

if TYPE_CHECKING:
    from netweir._native import Node
    from netweir._request import Request

URLENCODED = "application/x-www-form-urlencoded"
MULTIPART = "multipart/form-data"
# Never submitted by themselves: buttons only when they're the one pressed,
# images as coordinates nobody asked for.
_SKIPPED = {"submit", "button", "reset", "image"}


@dataclasses.dataclass(frozen=True)
class File:
    """A file in a form's fields: what a file input sends. An input with
    no file chosen is one with an empty name and no data, as browsers
    send it."""

    filename: str = ""
    # Out of the repr: printing form.fields shouldn't print the file.
    data: bytes = dataclasses.field(default=b"", repr=False)
    content_type: str = "application/octet-stream"


Files = Mapping[str, Any] | Iterable[tuple[str, Any]]


@dataclasses.dataclass
class Form:
    """A form ready to submit: where to, how, the fields in the order the
    browser would send them, and the page it's on.

    Submit it with ``netweir.submit(form)`` or ``client.submit(form)``, or
    in a crawl yield ``form.request(callback=...)``.
    """

    method: str
    action: str
    fields: list[tuple[str, str | File]]
    enctype: str = URLENCODED
    referer: str | None = None

    def encoded(self) -> tuple[str, bytes | None, str | None]:
        """The URL, body and Content-Type the browser would send: a GET
        form's fields go in the query string, replacing any there."""
        # Line breaks go as CRLF, whatever the page had.
        fields = [(_crlf(k), v if isinstance(v, File) else _crlf(v)) for k, v in self.fields]
        if self.method == "POST" and self.enctype == MULTIPART:
            return self.action, *_multipart(fields)
        # Anything but multipart sends a file's name and nothing of it.
        text = [(k, v.filename if isinstance(v, File) else v) for k, v in fields]
        if self.method == "GET":
            parts = urlsplit(self.action)
            query = urlencode(text)
            return urlunsplit(parts._replace(query=query, fragment="")), None, None
        return self.action, urlencode(text).encode(), URLENCODED

    def request(self, callback: Any = None, **kwargs: Any) -> Request:
        """A crawl Request that submits this form."""
        from netweir._request import Request

        url, body, content_type = self.encoded()
        if body is None:
            return Request(url, callback=callback, referer=self.referer, **kwargs)
        return Request(
            url,
            callback=callback,
            method=self.method,
            body=body,
            content_type=content_type,
            kind="form",
            referer=self.referer,
            **kwargs,
        )


def _crlf(text: str) -> str:
    return re.sub(r"\r\n|\r|\n", "\r\n", text)


def read_form(
    root: Node,
    page_url: str,
    base_url: str,
    query: str | None = None,
    *,
    data: Mapping[str, Any] | Iterable[tuple[str, Any]] | None = None,
    files: Files | None = None,
    click: str | bool | None = None,
    formid: str | None = None,
    formname: str | None = None,
    formnumber: int = 0,
) -> Form:
    """The form on the page chosen by ``query`` (a CSS selector, or an
    XPath when it starts with ``/`` or ``(``), ``formid``, ``formname``, or
    else its position, ``formnumber``."""
    form = _pick(root, query, formid, formname, formnumber)
    button = _button(form, click)

    def attr(name: str) -> str | None:
        # The pressed button's formmethod, formaction and formenctype win.
        if button is not None and button.attr("form" + name) is not None:
            return button.attr("form" + name)
        return form.attr(name)

    method = (attr("method") or "get").strip().upper()
    if method not in ("GET", "POST"):
        method = "GET"
    # Relative to <base href>; an empty action is the page itself.
    action = (attr("action") or "").strip()
    action = urljoin(base_url, action) if action else page_url
    enctype = (attr("enctype") or "").strip().lower()
    if enctype not in (URLENCODED, MULTIPART):
        enctype = URLENCODED
    fields = _successful(form)
    if button is not None and button.attr("name"):
        fields.append((button.attr("name"), button.attr("value") or ""))
    if data:
        fields = _merged(fields, data)
    if files:
        fields = _merged(fields, _files(files))
    return Form(method, action, fields, enctype, page_url)


def _pick(root: Node, query: str | None, formid: str | None, formname: str | None, n: int) -> Node:
    if query is not None:
        is_xpath = query.lstrip().startswith(("/", "("))
        found = [
            x for x in (root.xpath(query) if is_xpath else root.css(query)) if hasattr(x, "tag")
        ]
        found = [x if x.tag == "form" else x.find_parent("form") for x in found]
        found = [x for x in found if x is not None]
        what = f"matching {query!r}"
    elif formid is not None:
        found = [f for f in root.select("form") if f.attr("id") == formid]
        what = f"with id {formid!r}"
    elif formname is not None:
        found = [f for f in root.select("form") if f.attr("name") == formname]
        what = f"named {formname!r}"
    else:
        forms = root.select("form")
        found = forms[n : n + 1] if 0 <= n < len(forms) else []
        what = f"number {n} (the page has {len(forms)})"
    if not found:
        raise ValueError(f"no form {what} on the page")
    return found[0]


def _successful(form: Node) -> list[tuple[str, str]]:
    """The controls the HTML standard says a submission includes, in
    document order."""
    fields: list[tuple[str, str]] = []
    radios: dict[str, int] = {}
    for control in form.select("input, select, textarea"):
        name = control.attr("name")
        if not name or _disabled(control):
            continue
        if control.tag == "select":
            options = [o for o in control.select("option") if o.attr("disabled") is None]
            chosen = [o for o in options if o.attr("selected") is not None]
            if control.attr("multiple") is None:
                # One option is selected: the last marked, or the first.
                chosen = chosen[-1:] or options[:1]
            fields.extend((name, _option_value(o)) for o in chosen)
        elif control.tag == "textarea":
            fields.append((name, control.text))
        else:
            kind = (control.attr("type") or "").strip().lower()
            if kind in _SKIPPED:
                continue
            if kind == "file":
                # Sent with no file chosen, unless files= gives one.
                fields.append((name, File()))
                continue
            if kind in ("checkbox", "radio"):
                if control.attr("checked") is not None:
                    value = control.attr("value")
                    if kind == "radio" and name in radios:
                        # One radio per group is checked: the last marked.
                        fields[radios[name]] = (name, value if value is not None else "on")
                        continue
                    if kind == "radio":
                        radios[name] = len(fields)
                    fields.append((name, value if value is not None else "on"))
            else:
                # Text, hidden and the like; an unknown type is text too.
                fields.append((name, control.attr("value") or ""))
    return fields


def _disabled(control: Node) -> bool:
    """Disabled itself, or inside a disabled fieldset but not in that
    fieldset's first legend."""
    if control.attr("disabled") is not None:
        return True
    path = [control, *control.parents]
    for i, ancestor in enumerate(path[1:], 1):
        if ancestor.tag == "fieldset" and ancestor.attr("disabled") is not None:
            legends = [c for c in ancestor.children if c.tag == "legend"]
            if not (legends and path[i - 1] == legends[0]):
                return True
    return False


def _option_value(option: Node) -> str:
    value = option.attr("value")
    return value if value is not None else " ".join(option.text.split())


def _button(form: Node, click: str | bool | None) -> Node | None:
    """The submit button pressed: the one named ``click``, none for
    ``click=False``, or else the first."""
    if click is False:
        return None
    buttons = [
        b
        for b in form.select("input, button")
        if not _disabled(b)
        and (
            (b.tag == "input" and (b.attr("type") or "").strip().lower() == "submit")
            or (b.tag == "button" and (b.attr("type") or "submit").strip().lower() == "submit")
        )
    ]
    if isinstance(click, str):
        named = [b for b in buttons if b.attr("name") == click]
        if not named:
            raise ValueError(f"no submit button named {click!r} in the form")
        return named[0]
    return buttons[0] if buttons else None


def _files(files: Files) -> list[tuple[str, list[File]]]:
    """``files`` as Files by field name: each value a path, a
    (filename, bytes) or (filename, bytes, content_type) tuple, or a list
    of them for an input that takes several."""
    pairs = files.items() if isinstance(files, Mapping) else files
    out = []
    for name, value in pairs:
        values = value if isinstance(value, list) else [value]
        # No files chosen: the input goes empty, as a browser sends it.
        out.append((name, [_file(name, v) for v in values] or [File()]))
    return out


def _file(name: str, value: Any) -> File:
    if isinstance(value, File):
        return value
    if isinstance(value, str | os.PathLike):
        filename = os.path.basename(value)
        # Known limit: the whole file is read into memory, and the body
        # holds another copy; a file of gigabytes needs that much twice.
        with open(value, "rb") as f:
            data = f.read()
        return File(filename, data, _guess_type(filename))
    if isinstance(value, tuple) and len(value) in (2, 3):
        filename, data = value[0], value[1]
        if not isinstance(filename, str):
            raise TypeError(f"files[{name!r}]: the filename must be a str, not {filename!r}")
        if not isinstance(data, bytes | bytearray | memoryview):
            raise TypeError(f"files[{name!r}]: the data must be bytes, not {type(data).__name__}")
        content_type = value[2] if len(value) == 3 else _guess_type(filename)
        return File(filename, bytes(data), content_type)
    raise TypeError(
        f"files[{name!r}] must be a path, a (filename, bytes) or (filename, bytes,"
        f" content_type) tuple, a netweir.File, or a list of them, not {type(value).__name__}"
    )


# The types of compressed files, which mimetypes gives as an encoding of
# the extension before.
_COMPRESSED = {
    "gzip": "application/gzip",
    "bzip2": "application/x-bzip2",
    "xz": "application/x-xz",
}


def _guess_type(filename: str) -> str:
    """By the last extension, as a browser does; never by looking inside."""
    # Only the extension: a name such as "data:x.png" isn't a data URL.
    kind, encoding = mimetypes.guess_type("x" + os.path.splitext(filename)[1].lower())
    if encoding is not None:
        kind = _COMPRESSED.get(encoding)
    return kind or "application/octet-stream"


def _content_type(value: Any) -> str:
    # A type with anything outside printable ASCII is no type, which
    # browsers send as octet-stream; it never reaches the part's headers.
    if not isinstance(value, str) or not value or not all(" " <= c <= "~" for c in value):
        return "application/octet-stream"
    return value


def _merged(
    fields: list[tuple[str, str | File]], data: Mapping[str, Any] | Iterable[tuple[str, Any]]
) -> list[tuple[str, str | File]]:
    """``fields`` with ``data`` replacing every field of the same name, in
    the place of the first, and adding the rest at the end. A list value
    gives the name more than once."""
    pairs = data.items() if isinstance(data, Mapping) else data
    given: dict[str, list[str | File]] = {}
    for name, value in pairs:
        values = value if isinstance(value, list | tuple) else [value]
        given.setdefault(name, []).extend(v if isinstance(v, File) else str(v) for v in values)
    out: list[tuple[str, str | File]] = []
    placed: set[str] = set()
    for name, value in fields:
        if name not in given:
            out.append((name, value))
        elif name not in placed:
            out.extend((name, v) for v in given[name])
            placed.add(name)
    out.extend((name, v) for name, values in given.items() if name not in placed for v in values)
    return out


def _multipart(fields: list[tuple[str, str | File]]) -> tuple[bytes, str]:
    """The body and Content-Type of a multipart/form-data submission, with
    a boundary shaped as Chrome's."""
    alphabet = string.ascii_letters + string.digits
    boundary = "----WebKitFormBoundary" + "".join(secrets.choice(alphabet) for _ in range(16))

    def escape(name: str) -> str:
        return name.replace('"', "%22").replace("\r", "%0D").replace("\n", "%0A")

    out = bytearray()
    for name, value in fields:
        out += f'--{boundary}\r\nContent-Disposition: form-data; name="{escape(name)}"'.encode()
        if isinstance(value, File):
            out += f'; filename="{escape(value.filename)}"\r\n'.encode()
            out += f"Content-Type: {_content_type(value.content_type)}\r\n\r\n".encode()
            out += value.data + b"\r\n"
        else:
            out += b"\r\n\r\n" + value.encode() + b"\r\n"
    out += f"--{boundary}--\r\n".encode()
    return bytes(out), f"{MULTIPART}; boundary={boundary}"
