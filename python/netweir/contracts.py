"""Spider contracts, as Scrapy has them: lines in a callback's docstring
that ``netweir check`` turns into a request, and then checks against what
the callback gave::

    def parse(self, page):
        \"\"\"@url https://books.toscrape.com/
        @returns items 20 20
        @returns requests 1
        @scrapes title price
        \"\"\"

The built-in contracts are ``@url``, ``@returns``, ``@scrapes``,
``@cb_kwargs``, ``@meta``, ``@method``, ``@body`` and ``@header``.

A contract of your own subclasses ``Contract``, has a ``name`` (written
``@name`` in the docstring), and any of ``adjust_request_args``,
``pre_process`` and ``post_process``; it fails by raising ``ContractFail``.
List it on the spider: ``contracts = [MyContract]``.
"""

from __future__ import annotations

import dataclasses
import inspect
import json
import re
import sys
import time
import unittest
from collections.abc import Callable, Iterable
from typing import Any, TextIO

from netweir._request import Request


class ContractFail(AssertionError):
    """What a contract's pre_process or post_process raises when the page or
    the callback's output isn't what the contract says."""


class ContractError(ValueError):
    """A contract written wrong: reported by its message alone."""


class Contract:
    """One ``@name ARGS...`` line of a callback's docstring.

    ``method`` is the callback, bound to the spider, and ``args`` the
    line's words after the name. Subclasses may define:

    - ``adjust_request_args(args)``: change the keyword arguments the
      check's ``netweir.Request`` is made with (``url``, ``method``,
      ``headers``, ``meta``... and ``cb_kwargs``, for the callback), and
      return them;
    - ``pre_process(page)``: check the page before the callback gets it;
    - ``post_process(output)``: check the list of items and Requests the
      callback gave.

    Each of the last two counts as one contract in the results.
    """

    name: str = ""

    def __init__(self, method: Callable[..., Any], *args: str):
        self.method = method
        self.args = args

    def adjust_request_args(self, args: dict[str, Any]) -> dict[str, Any]:
        return args


def _rest(contract: Contract) -> str:
    if not contract.args:
        raise ContractError(f"@{contract.name} needs a value")
    return " ".join(contract.args)


def _json(contract: Contract) -> Any:
    try:
        return json.loads(_rest(contract))
    except ValueError as e:
        raise ContractError(f"@{contract.name} takes JSON: {e}") from None


class UrlContract(Contract):
    """``@url URL``: the page to fetch."""

    name = "url"

    def adjust_request_args(self, args: dict[str, Any]) -> dict[str, Any]:
        if len(self.args) != 1:
            raise ContractError("@url takes one URL")
        args["url"] = self.args[0]
        return args


class CallbackKeywordArgumentsContract(Contract):
    """``@cb_kwargs JSON``: keyword arguments for the callback."""

    name = "cb_kwargs"

    def adjust_request_args(self, args: dict[str, Any]) -> dict[str, Any]:
        args["cb_kwargs"] = _json(self)
        return args


class MetadataContract(Contract):
    """``@meta JSON``: the request's meta, which the page has as ``page.meta``."""

    name = "meta"

    def adjust_request_args(self, args: dict[str, Any]) -> dict[str, Any]:
        args["meta"] = _json(self)
        return args


class MethodContract(Contract):
    """``@method POST``: the request's method."""

    name = "method"

    def adjust_request_args(self, args: dict[str, Any]) -> dict[str, Any]:
        if len(self.args) != 1:
            raise ContractError("@method takes one method")
        args["method"] = self.args[0]
        return args


class BodyContract(Contract):
    """``@body TEXT``: the request's body."""

    name = "body"

    def adjust_request_args(self, args: dict[str, Any]) -> dict[str, Any]:
        args["body"] = _rest(self)
        return args


class HeaderContract(Contract):
    """``@header NAME VALUE``: a request header; give it once per header."""

    name = "header"

    def adjust_request_args(self, args: dict[str, Any]) -> dict[str, Any]:
        if len(self.args) < 2:
            raise ContractError("@header takes a name and a value")
        args["headers"] = {**(args.get("headers") or {}), self.args[0]: " ".join(self.args[1:])}
        return args


class ReturnsContract(Contract):
    """``@returns items|requests [MIN [MAX]]``: how many items, or
    Requests, the callback gives: at least MIN (default 1) and at most MAX
    (default no limit)."""

    name = "returns"

    def __init__(self, method: Callable[..., Any], *args: str):
        super().__init__(method, *args)
        if not 1 <= len(args) <= 3:
            raise ContractError(f"@returns takes 1, 2 or 3 values, not {len(args)}")
        if args[0] not in ("item", "items", "request", "requests"):
            raise ContractError(f"@returns counts items or requests, not {args[0]!r}")
        if not all(n.isdigit() for n in args[1:]):
            raise ContractError(f"@returns takes whole numbers after {args[0]}, not {args[1:]}")
        self.kind = args[0]
        self.min: float = int(args[1]) if len(args) > 1 else 1
        self.max: float = int(args[2]) if len(args) > 2 else float("inf")

    def post_process(self, output: list[Any]) -> None:
        requests = self.kind.startswith("request")
        count = sum(isinstance(x, Request) if requests else _is_item(x) for x in output)
        if not self.min <= count <= self.max:
            expected = str(self.min) if self.min == self.max else f"{self.min}..{self.max}"
            raise ContractFail(f"Returned {count} {self.kind}, expected {expected}")


def _is_item(x: Any) -> bool:
    """Whether the crawl would take ``x`` as an item: what isn't None, a
    Request, or a str, number or set (which it refuses)."""
    from netweir._crawl import _NOT_ITEMS

    return x is not None and not isinstance(x, (Request, *_NOT_ITEMS))


def _has_field(item: Any, name: str) -> bool:
    if dataclasses.is_dataclass(item):
        return name in {f.name for f in dataclasses.fields(item)}
    attrs = getattr(type(item), "__attrs_attrs__", None)
    if attrs is not None:
        return name in {a.name for a in attrs}
    try:
        # A dict's keys; a Scrapy Item's fields that are set.
        return name in item
    except TypeError:
        return hasattr(item, name)


class ScrapesContract(Contract):
    """``@scrapes FIELD...``: every item the callback gives has these
    fields: keys of a dict, fields of a dataclass or attrs class, set
    fields of a Scrapy Item."""

    name = "scrapes"

    def post_process(self, output: list[Any]) -> None:
        for item in output:
            if not _is_item(item):
                continue
            missing = [name for name in self.args if not _has_field(item, name)]
            if missing:
                raise ContractFail(f"Missing fields: {', '.join(missing)}")


BUILT_IN: list[type[Contract]] = [
    UrlContract,
    CallbackKeywordArgumentsContract,
    MetadataContract,
    MethodContract,
    BodyContract,
    HeaderContract,
    ReturnsContract,
    ScrapesContract,
]

_LINE = re.compile(r"@(\w+)\s*(.*)")
_HAS_CONTRACT = re.compile(r"^\s*@", re.M)


def tested_methods(spider_cls: type) -> list[str]:
    """The names of the spider's methods with a contract in their docstring."""
    names = []
    for name in sorted(dir(spider_cls)):
        if name.startswith("__"):
            continue
        value = inspect.getattr_static(spider_cls, name, None)
        value = getattr(value, "__func__", value)
        if inspect.isfunction(value) and value.__doc__ and _HAS_CONTRACT.search(value.__doc__):
            names.append(name)
    return names


def contracts_of(method: Callable[..., Any], known: Iterable[type[Contract]]) -> list[Contract]:
    """The contracts a callback's docstring lists, in order."""
    by_name = {c.name: c for c in known}
    found = []
    for line in (method.__doc__ or "").splitlines():
        match = _LINE.match(line.strip())
        if match is None:
            continue
        name, rest = match.groups()
        if name not in by_name:
            raise ContractError(f"unknown contract @{name}; known: {', '.join(sorted(by_name))}")
        found.append(by_name[name](method, *rest.split()))
    return found


# --- running them: netweir check ---------------------------------------------


class _Case(unittest.TestCase):
    """One line of the results: a contract's hook, or a callback."""

    def __init__(self, label: str):
        super().__init__("_nothing")
        self.label = label

    def _nothing(self) -> None:
        pass

    def __str__(self) -> str:
        return self.label


async def _collect(result: Any) -> list[Any]:
    """Everything a callback gave, however it gave it."""
    if inspect.isasyncgen(result):
        return [x async for x in result]
    if inspect.isawaitable(result):
        result = await result
    if result is None:
        return []
    if isinstance(result, Iterable) and not isinstance(result, (dict, str, bytes, bytearray)):
        return list(result)
    return [result]


async def _hook(result: unittest.TestResult, case: _Case, hook: Callable, value: Any) -> None:
    result.startTest(case)
    try:
        outcome = hook(value)
        if inspect.isawaitable(outcome):
            await outcome
    except ContractFail as e:
        # The message is the failure; a traceback would only point here.
        result.addFailure(case, (type(e), e, None))
    except Exception:
        result.addError(case, sys.exc_info())
    else:
        result.addSuccess(case)
    finally:
        result.stopTest(case)


def requests_for(spider: Any, result: unittest.TestResult) -> tuple[list[Request], set[str]]:
    """One Request per contracted callback of ``spider``, each with a
    callback that runs the contracts' hooks and reports to ``result``.
    Returns them and the names of the methods they're for; a method takes
    its name out of that set once its page arrives."""
    label = spider.name or type(spider).__name__
    known = [*BUILT_IN, *spider.contracts]
    requests: list[Request] = []
    waiting: set[str] = set()
    for name in tested_methods(type(spider)):
        method = getattr(spider, name)
        try:
            contracts = contracts_of(method, known)
            args: dict[str, Any] = {"callback": method, "dont_filter": True}
            for contract in contracts:
                args = contract.adjust_request_args(args)
            if not args.get("url"):
                raise ContractError("no @url: there's no page to check it against")
            cb_kwargs = args.pop("cb_kwargs", None) or {}
            callback = args.pop("callback")
            request = Request(**args)
        except Exception as e:
            # A traceback for a contract's own code that broke; just the
            # message for a contract written wrong.
            why = (type(e), e, None) if isinstance(e, ContractError) else sys.exc_info()
            result.addError(_Case(f"[{label}] {name} (contract)"), why)
            continue
        request.callback, request.errback = _wrapped(
            label, name, callback, cb_kwargs, contracts, result, waiting
        )
        requests.append(request)
        waiting.add(name)
    return requests, waiting


def _wrapped(label, name, callback, cb_kwargs, contracts, result, waiting):
    async def check(page: Any) -> None:
        waiting.discard(name)
        for contract in contracts:
            if hasattr(contract, "pre_process"):
                case = _Case(f"[{label}] {name} (@{contract.name} pre-hook)")
                await _hook(result, case, contract.pre_process, page)
        try:
            output = await _collect(callback(page, **cb_kwargs))
        except Exception:
            result.addError(_Case(f"[{label}] {name} (callback)"), sys.exc_info())
            return
        for contract in contracts:
            if hasattr(contract, "post_process"):
                case = _Case(f"[{label}] {name} (@{contract.name} post-hook)")
                await _hook(result, case, contract.post_process, output)

    def lost(request: Request, error: BaseException) -> None:
        waiting.discard(name)
        result.addError(
            _Case(f"[{label}] {name} (errback)"), (type(error), error, error.__traceback__)
        )

    return check, lost


def never_reached(spider: Any, waiting: set[str], result: unittest.TestResult) -> None:
    """Reports the contracted callbacks whose page never came: kept out by
    robots.txt, a block or a limit. Scrapy says nothing, and a contract
    that never ran looks like one that passed."""
    label = spider.name or type(spider).__name__
    for name in sorted(waiting):
        error = RuntimeError(
            "the page never reached the callback or its errback"
            " (robots.txt, a block or a limit kept it out)"
        )
        result.addError(_Case(f"[{label}] {name} (callback)"), (RuntimeError, error, None))


def new_result(stream: TextIO, verbose: bool) -> unittest.TextTestResult:
    return unittest.TextTestResult(
        unittest.runner._WritelnDecorator(stream),  # type: ignore[attr-defined]
        descriptions=False,
        verbosity=2 if verbose else 1,
    )


def outcome(result: unittest.TextTestResult) -> dict[str, Any]:
    """What a run found, as data: the count, and each failure and error as
    (label, text). For combining the runs of several processes."""
    return {
        "run": result.testsRun,
        "failures": [(str(case), text) for case, text in result.failures],
        "errors": [(str(case), text) for case, text in result.errors],
    }


def summary(stream: TextIO, found: dict[str, Any], started: float) -> None:
    """The errors and failures in full, then the count and the verdict, as
    Python's unittest prints them."""
    line1, line2 = "=" * 70, "-" * 70
    stream.write("\n")
    for flavour, cases in (("ERROR", found["errors"]), ("FAIL", found["failures"])):
        for label, text in cases:
            stream.write(f"{line1}\n{flavour}: {label}\n{line2}\n{text}\n")
    run = found["run"]
    stream.write(f"{line2}\nRan {run} contract{'' if run == 1 else 's'}")
    stream.write(f" in {time.time() - started:.3f}s\n\n")
    if found["failures"] or found["errors"]:
        counts = [("failures", len(found["failures"])), ("errors", len(found["errors"]))]
        stream.write(f"FAILED ({', '.join(f'{k}={n}' for k, n in counts if n)})\n")
    else:
        stream.write("OK\n")
