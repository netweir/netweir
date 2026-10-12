"""Validating items, and checking a crawl once it has ended."""

from __future__ import annotations

import collections.abc
import dataclasses
import inspect
import logging
import sys
import types
import typing
from typing import Any

log = logging.getLogger("netweir")


class validate:  # noqa: N801 - reads as a function in a pipeline list
    """A pipeline stage that checks each item against ``schema``: a JSON
    Schema (a dict, which needs the jsonschema package) or a dataclass.

    In a crawl, an item that fails is dropped and counted as
    ``items_invalid``, or with ``drop=False`` kept with its errors in an
    ``_errors`` field (a dict item's). Each error is counted against its
    field in ``spider.invalid_fields``. On its own, the stage returns the
    item, or None if it fails; ``errors(item)`` says why."""

    def __init__(self, schema: Any, drop: bool = True):
        self.drop = drop
        if isinstance(schema, dict):
            try:
                import jsonschema
            except ImportError:
                raise ImportError(
                    "a JSON Schema needs the jsonschema package: pip install jsonschema"
                ) from None
            cls = jsonschema.validators.validator_for(schema)
            cls.check_schema(schema)
            self._json = cls(schema)
            self._fields = None
        elif isinstance(schema, type) and dataclasses.is_dataclass(schema):
            self._json = None
            self._fields = []
            unresolved = []
            for f in dataclasses.fields(schema):
                kind = _resolve(schema, f.type)
                if kind is None:
                    unresolved.append(f.name)
                    kind = Any
                required = f.default is dataclasses.MISSING and (
                    f.default_factory is dataclasses.MISSING
                )
                self._fields.append((f.name, kind, required))
            if unresolved:
                # A type only imported for type checkers, or one defined
                # inside a function: those fields can't be checked.
                log.warning(
                    "%s: can't resolve the types of %s; their values aren't checked",
                    schema.__name__,
                    ", ".join(unresolved),
                )
        else:
            raise TypeError(
                f"validate takes a JSON Schema (a dict) or a dataclass, not {type(schema).__name__}"
            )

    def problems(self, item: Any) -> list[tuple[str, str]]:
        """(field, what's wrong) for each error; none for a valid item."""
        if dataclasses.is_dataclass(item) and not isinstance(item, type):
            # A shallow copy: asdict deep-copies every value, which is slow
            # and fails on values that can't be copied.
            item = {f.name: getattr(item, f.name) for f in dataclasses.fields(item)}
        if self._json is not None:
            return list(_json_problems(self._json, item))
        if not isinstance(item, dict):
            return [("", f"expected a dict, got {type(item).__name__}")]
        found = []
        for name, kind, required in self._fields:
            value = item.get(name)
            if value is None:
                # Missing, or None where the type doesn't allow it.
                if required and (name not in item or not _admits_none(kind)):
                    found.append((name, "required"))
                continue
            found.extend(_type_problems(value, kind, name))
        return found

    def errors(self, item: Any) -> list[str]:
        """What's wrong with ``item``, as "field: message" strings."""
        return [
            f"{field}: {message}" if field else message for field, message in self.problems(item)
        ]

    def __call__(self, item: Any) -> Any:
        return None if self.problems(item) else item


def _resolve(owner: type, annotation: Any) -> Any:
    """A field's annotation as a type, each field on its own, so one that
    can't be resolved leaves the others checked. None if it can't be."""
    if not isinstance(annotation, str):
        return annotation
    module = sys.modules.get(owner.__module__)
    namespace = dict(vars(module)) if module else {}
    try:
        # As typing.get_type_hints evaluates a string annotation.
        return eval(annotation, namespace, dict(vars(owner)))  # noqa: S307
    except Exception:  # noqa: BLE001 - unresolvable: not checked
        return None


def _admits_none(kind: Any) -> bool:
    if kind is Any or kind is None or kind is type(None):
        return True
    if typing.get_origin(kind) in (typing.Union, types.UnionType):
        return type(None) in typing.get_args(kind)
    return False


def _json_problems(validator: Any, item: Any):
    for error in sorted(validator.iter_errors(item), key=lambda e: list(map(str, e.path))):
        parent = ".".join(str(p) for p in error.absolute_path)
        if (
            error.validator == "required"
            and isinstance(error.instance, dict)
            and isinstance(error.validator_value, list)  # draft 3: a bool
        ):
            # One error per missing property, named in its message.
            named = [
                name
                for name in error.validator_value
                if name not in error.instance and repr(name) in error.message
            ]
            for name in named:
                yield (f"{parent}.{name}" if parent else name), "required"
            if named:
                continue
        yield parent, error.message


def _type_problems(value: Any, kind: Any, path: str) -> list[tuple[str, str]]:
    """Checks ``value`` against an annotation, for the types items hold;
    any other annotation passes."""
    origin = typing.get_origin(kind)
    if origin in (typing.Union, types.UnionType):
        options = [o for o in typing.get_args(kind) if o is not type(None)]
        if value is None and len(options) < len(typing.get_args(kind)):
            return []
        found = [_type_problems(value, o, path) for o in options]
        if any(not problems for problems in found):
            return []
        # One option of the value's own container type (a list, for a list
        # in list[int] | None): its errors say what's really wrong inside.
        same = [
            problems
            for o, problems in zip(options, found, strict=True)
            if isinstance(value, (list, dict)) and (typing.get_origin(o) or o) is type(value)
        ]
        if len(same) == 1:
            return same[0]
        return [(path, f"expected {_name(kind)}, got {type(value).__name__}")]
    if origin is list or kind is list:
        if not isinstance(value, list):
            return [(path, f"expected list, got {type(value).__name__}")]
        (item_kind,) = typing.get_args(kind) or (Any,)
        return [p for i, v in enumerate(value) for p in _type_problems(v, item_kind, f"{path}.{i}")]
    if origin is dict or kind is dict:
        if not isinstance(value, dict):
            return [(path, f"expected dict, got {type(value).__name__}")]
        args = typing.get_args(kind)
        value_kind = args[1] if len(args) == 2 else Any
        return [p for k, v in value.items() for p in _type_problems(v, value_kind, f"{path}.{k}")]
    if kind in (str, bool):
        ok = isinstance(value, kind)
    elif kind is int:
        ok = isinstance(value, int) and not isinstance(value, bool)
    elif kind is float:
        ok = isinstance(value, (int, float)) and not isinstance(value, bool)
    else:
        return []
    return [] if ok else [(path, f"expected {kind.__name__}, got {type(value).__name__}")]


def _name(kind: Any) -> str:
    if typing.get_origin(kind) in (typing.Union, types.UnionType):
        return " | ".join(_name(k) for k in typing.get_args(kind))
    return "None" if kind is type(None) else getattr(kind, "__name__", str(kind))


@dataclasses.dataclass(frozen=True)
class Checks:
    """Checks a crawl once it has ended; each is off unless given.

    ``min_items``: at least this many items. ``max_error_rate``: failed
    fetches and callback errors, out of pages fetched plus failed fetches.
    ``max_invalid``: items that failed ``validate``, out of those it
    checked. ``coverage``: {field: fraction of items it's present and not
    empty in}. ``finish_reasons``: the reasons the crawl may end for, such
    as ("finished",)."""

    min_items: int | None = None
    max_error_rate: float | None = None
    max_invalid: float | None = None
    coverage: dict[str, float] | None = None
    finish_reasons: tuple[str, ...] | None = None

    def __post_init__(self) -> None:
        if isinstance(self.finish_reasons, str):
            # finish_reasons="finished": one reason, not its letters.
            object.__setattr__(self, "finish_reasons", (self.finish_reasons,))

    def __call__(self, stats: dict[str, int], spider: Any) -> list[str]:
        failures = []
        items = stats["items"]
        if self.min_items is not None and items < self.min_items:
            failures.append(f"{items} items, fewer than {self.min_items}")
        if self.max_error_rate is not None:
            errors = stats["failed"] + stats["callback_errors"]
            tried = stats["fetched"] + stats["failed"]
            rate = errors / tried if tried else 0.0
            if rate > self.max_error_rate:
                failures.append(f"error rate {rate:.1%}, more than {self.max_error_rate:.1%}")
        if self.max_invalid is not None:
            checked = stats["items_validated"]
            rate = stats["items_invalid"] / checked if checked else 0.0
            if rate > self.max_invalid:
                failures.append(f"{rate:.1%} of items invalid, more than {self.max_invalid:.1%}")
        for field, wanted in (self.coverage or {}).items():
            if not items:
                failures.append(f"{field}: no items to find it in")
                continue
            share = spider.coverage.get(field, 0) / items
            if share < wanted:
                failures.append(f"{field} in {share:.1%} of items, less than {wanted:.1%}")
        if self.finish_reasons is not None and spider.finish_reason not in self.finish_reasons:
            failures.append(
                f"ended by {spider.finish_reason}, not {' or '.join(self.finish_reasons)}"
            )
        return failures


async def run_checks(checks: Any, stats: dict[str, int], spider: Any) -> list[str]:
    """Every check's failures, in order. A check passes by returning None or
    True, and fails by returning False, a message or messages, or anything
    else; one that raises has failed too: a check that can't tell isn't a
    pass."""
    if not isinstance(checks, (list, tuple)):
        checks = [checks]  # checks = netweir.Checks(...): one check
    failures: list[str] = []
    for check in checks:
        name = getattr(check, "__name__", type(check).__name__)
        try:
            result = check(stats, spider)
            if inspect.isawaitable(result):
                result = await result
            if result is None or result is True:
                continue
            if isinstance(result, str):
                found = [result or f"{name} failed"]
            elif result is False:
                found = [f"{name} returned False"]
            elif isinstance(result, (list, tuple, set, collections.abc.Iterator)):
                found = [str(m) or f"{name} failed" for m in result]
            else:
                found = [f"{name} returned {result!r}"]
        except Exception as e:  # noqa: BLE001 - reported as a failure
            found = [f"{name} raised {type(e).__name__}: {e}"]
        failures.extend(found)
    for failure in failures:
        log.warning("check failed: %s", failure)
    if checks and not failures:
        log.info("all %d checks passed", len(checks))
    return failures


def empty(value: Any) -> bool:
    """None, "", and an empty list, dict, tuple or set: not there, for
    coverage. Never compares the value itself, which for some types (an
    array, pandas' NA) raises or answers with an array."""
    if value is None:
        return True
    return isinstance(value, (str, list, dict, tuple, set)) and len(value) == 0
