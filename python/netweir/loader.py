"""Item loaders, as Scrapy's (the itemloaders package): collect values for
an item's fields from queries and plain values, clean them with input
processors as they come in, and settle each field with an output processor
when the item is loaded.

    from netweir.loader import ItemLoader, MapCompose, TakeFirst

    class ProductLoader(ItemLoader):
        default_output_processor = TakeFirst()
        name_in = MapCompose(str.strip)

    loader = ProductLoader(response=page)
    loader.add_css("name", "h1::text")
    loader.add_xpath("price", "//p[@class='price']/text()", re=r"[\\d.]+")
    item = loader.load_item()

Written for Netweir's selectors (a Page, a Node or a Selection) and with no
dependencies; parsel selectors and Scrapy responses work too. Items can be
dicts, dataclasses, attrs classes or Scrapy Items. See docs/loaders.md.
"""

from __future__ import annotations

import dataclasses
import inspect
import sys
from collections import ChainMap
from collections.abc import Callable, Iterable, Iterator, Mapping, MutableMapping
from collections.abc import Set as AbstractSet
from functools import lru_cache, partial
from re import Pattern
from types import BuiltinFunctionType, FunctionType, MappingProxyType, MethodType
from typing import Any, Protocol, TypeVar

from netweir import _jmespath, _regex
from netweir._native import Selection

__all__ = [
    "Compose",
    "Identity",
    "ItemLoader",
    "Join",
    "MapCompose",
    "SelectJmes",
    "TakeFirst",
]

_L = TypeVar("_L", bound="ItemLoader")


# --- values ------------------------------------------------------------------


def _as_list(value: Any) -> Iterable[Any]:
    """``value`` as something to iterate over: None is nothing, a list,
    tuple, set or iterator is itself, and anything else (a string, a dict,
    an item) is one value."""
    if value is None:
        return []
    kind = type(value)
    # The usual cases first: checking against the abstract classes costs
    # ten times as much.
    if kind is list or kind is tuple:
        return value
    if kind is str or kind is dict:
        return [value]
    if isinstance(value, (list, tuple, Iterator, AbstractSet)):
        return value
    return [value]


def _flatten_json(values: Iterable[Any], out: list[Any]) -> list[Any]:
    # Lists inside lists become one list; objects stay whole.
    for value in values:
        if isinstance(value, (list, tuple)):
            _flatten_json(value, out)
        else:
            out.append(value)
    return out


# --- loader context ----------------------------------------------------------


def _takes_context(function: Callable[..., Any]) -> bool:
    if not callable(function):
        raise TypeError(f"a processor must be callable, not {function!r}")
    try:
        parameters = inspect.signature(function).parameters
    except ValueError:  # some built-ins have no signature, and so no context
        return False
    if isinstance(function, partial) and "loader_context" in function.keywords:
        return False
    return "loader_context" in parameters


#: Whether a processor takes the context, by what decides it (see
#: _context_key). Cleared when full rather than ordered by use: the same few
#: processors are asked about over and over.
_context_answers: dict[Any, bool] = {}


def _context_key(function: Callable[..., Any]) -> Any:
    """What decides whether ``function`` takes the context, and holds
    nothing that should be let go: a method's function rather than the
    method, which holds its object (often a loader, with its whole page),
    and a callable object's class. None for a partial, whose arguments do
    hold objects and change the answer."""
    if isinstance(function, MethodType):
        return function.__func__
    if isinstance(function, (FunctionType, BuiltinFunctionType, type)):
        return function
    if isinstance(function, partial):
        return None
    return type(function)


def _wants_context(function: Callable[..., Any]) -> bool:
    # Reading a signature is the slow part of calling a processor. The
    # processors here are known without reading one, which matters because
    # they're often made afresh in every add_css(); for other callables the
    # answer is kept.
    known = _KNOWN.get(type(function))
    if known is not None:
        return known
    if not callable(function):
        raise TypeError(f"a processor must be callable, not {function!r}")
    key = _context_key(function)
    if key is None:
        return _takes_context(function)
    try:
        return _context_answers[key]
    except KeyError:
        pass
    except TypeError:  # unhashable
        return _takes_context(function)
    answer = _takes_context(function)
    if len(_context_answers) >= 1024:
        _context_answers.clear()
    _context_answers[key] = answer
    return answer


def _merged(context: Mapping[str, Any] | None, defaults: dict[str, Any]) -> Mapping[str, Any]:
    # The loader's context over the processor's defaults, as a view rather
    # than a copy, so a function that writes to it writes to the loader's.
    if not context:
        return defaults
    if not defaults:
        return context
    return ChainMap(context, defaults)  # type: ignore[arg-type]


# --- processors --------------------------------------------------------------


class Identity:
    """Gives the values back unchanged. The default for every field."""

    def __call__(self, values: Any) -> Any:
        return values


class TakeFirst:
    """The first value that isn't None or an empty string, or None. The
    usual output processor for a field with one value."""

    def __call__(self, values: Any) -> Any:
        for value in values:
            if value is not None and value != "":
                return value
        return None


class Join:
    """The values joined into one string with ``separator`` (a space by
    default)."""

    def __init__(self, separator: str = " "):
        self.separator = separator

    def __call__(self, values: Any) -> str:
        return self.separator.join(values)


class Compose:
    """The functions applied in turn to the whole value: ``Compose(f, g)``
    is ``g(f(values))``. Stops at the first None unless made with
    ``stop_on_none=False``.

    A function with a ``loader_context`` parameter is given the loader's
    context. Keyword arguments here are defaults for that context, which
    the loader's own values override."""

    def __init__(self, *functions: Callable[..., Any], **default_loader_context: Any):
        self.functions = functions
        self.stop_on_none = default_loader_context.get("stop_on_none", True)
        self.default_loader_context = default_loader_context

    def __call__(self, value: Any, loader_context: Mapping[str, Any] | None = None) -> Any:
        context = _merged(loader_context, self.default_loader_context)
        wants = [_wants_context(f) for f in self.functions]
        for function, wanted in zip(self.functions, wants, strict=True):
            if value is None and self.stop_on_none:
                break
            try:
                value = function(value, loader_context=context) if wanted else function(value)
            except Exception as e:
                raise ValueError(
                    f"Error in Compose with {function!s} value={value!r} "
                    f"error='{type(e).__name__}: {e!s}'"
                ) from e
        return value


class MapCompose:
    """The functions applied to each value in turn: every value goes
    through the first function, every result through the second, and so
    on. A function can return a list, whose items carry on as separate
    values, or None, which drops the value. Gives a list.

    Context works as in ``Compose``."""

    def __init__(self, *functions: Callable[..., Any], **default_loader_context: Any):
        self.functions = functions
        self.default_loader_context = default_loader_context

    def __call__(self, value: Any, loader_context: Mapping[str, Any] | None = None) -> list[Any]:
        values = _as_list(value)
        context = _merged(loader_context, self.default_loader_context)
        wanted = [_wants_context(f) for f in self.functions]
        for function, wants in zip(self.functions, wanted, strict=True):
            results: list[Any] = []
            for v in values:
                try:
                    out = function(v, loader_context=context) if wants else function(v)
                except Exception as e:
                    raise ValueError(
                        f"Error in MapCompose with {function!s} value={value!r} "
                        f"error='{type(e).__name__}: {e!s}'"
                    ) from e
                results.extend(_as_list(out))
            values = results
        return values  # type: ignore[return-value]


class SelectJmes:
    """The value searched with a JMESPath expression; one value at a time,
    so wrap it in ``MapCompose`` for a list. Needs the ``netweir[jmespath]``
    extra."""

    def __init__(self, json_path: str):
        self.json_path = json_path
        self.compiled_path = _jmespath._module().compile(json_path)

    def __call__(self, value: Any) -> Any:
        return self.compiled_path.search(value)


#: Whether each processor class here takes the context; exact classes only,
#: since a subclass may define its own __call__.
_KNOWN = {Identity: False, TakeFirst: False, Join: False, SelectJmes: False}
_KNOWN.update({Compose: True, MapCompose: True})


# --- items -------------------------------------------------------------------


@lru_cache(maxsize=1024)
def _declared(cls: type) -> dict[str, Mapping[str, Any]] | None:
    """A dataclass's or attrs class's fields and their metadata, or None
    for a class of another kind."""
    if dataclasses.is_dataclass(cls):
        return {f.name: f.metadata for f in dataclasses.fields(cls)}
    attrs = getattr(cls, "__attrs_attrs__", None)
    if attrs is not None:
        return {a.name: a.metadata for a in attrs}
    return None


def _is_scrapy_item(item: Any) -> bool:
    # Only checked when Scrapy is already imported: a Scrapy Item can't
    # exist otherwise, and Netweir doesn't need Scrapy.
    module = sys.modules.get("scrapy.item")
    return module is not None and isinstance(item, module.Item)


def _check_item(item: Any) -> None:
    if isinstance(item, dict) or _is_scrapy_item(item):
        return
    if not isinstance(item, type) and _declared(type(item)) is not None:
        return
    from netweir._rules import Item

    if isinstance(item, Item) or (isinstance(item, type) and issubclass(item, Item)):
        raise TypeError(
            "a netweir.Item describes how to extract an item, it doesn't hold one: "
            "load into the dict it extracts (item=Book.extract(page)), or a dict"
        )
    raise TypeError(
        "ItemLoader takes a dict, a dataclass, an attrs class or a Scrapy Item, "
        f"not {type(item).__name__}"
    )


def _no_field(item: Any, name: str) -> KeyError:
    return KeyError(f"{type(item).__name__} does not support field: {name}")


_NO_META: Mapping[str, Any] = MappingProxyType({})


def _field_meta(item: Any, name: str) -> Mapping[str, Any]:
    if isinstance(item, dict):
        return _NO_META
    fields = _declared(type(item))
    if fields is not None:
        try:
            return fields[name]
        except KeyError:
            raise KeyError(name) from None
    return item.fields[name]  # a Scrapy Item: its Field is the metadata


def _item_values(item: Any) -> Iterable[tuple[str, Any]]:
    fields = None if isinstance(item, dict) else _declared(type(item))
    if fields is None:
        return list(item.items())
    return [(name, getattr(item, name)) for name in fields if hasattr(item, name)]


def _set_field(item: Any, name: str, value: Any) -> None:
    fields = None if isinstance(item, dict) else _declared(type(item))
    if fields is None:
        item[name] = value
    elif name in fields:
        setattr(item, name, value)
    else:
        raise _no_field(item, name)


# --- the loader --------------------------------------------------------------


class _Stats(Protocol):
    def inc_value(self, key: str, count: int = 1) -> None: ...


def _unbound(method: Callable[..., Any]) -> Callable[..., Any]:
    # A plain function set as a class attribute (``name_in = clean``) comes
    # back from getattr as a method; it's meant to take only the values.
    # One defined in the class body (its qualified name has a dot) is a real
    # method and keeps its self.
    func = getattr(method, "__func__", None)
    if func is not None and "." not in getattr(method, "__qualname__", "."):
        return func
    return method


class ItemLoader:
    """Fills an item from a page.

    ``item`` is the item to fill (a dict, dataclass, attrs class or Scrapy
    Item); without one, ``default_item_class()`` is made. Values the item
    already has count as collected. ``response`` or ``selector`` is what
    queries run on: a Netweir Page, Node or Selection, a parsel Selector or
    a Scrapy response. Other keyword arguments go into ``context``, which
    processors with a ``loader_context`` parameter receive.

    Each field's input processor is ``<field>_in`` on the loader class, else
    ``input_processor`` in the item field's metadata, else
    ``default_input_processor``; output processors likewise with ``_out``,
    ``output_processor`` and ``default_output_processor``.
    """

    default_item_class: Callable[[], Any] = dict
    default_input_processor: Callable[..., Any] = Identity()
    default_output_processor: Callable[..., Any] = Identity()

    def __init__(
        self,
        item: Any = None,
        selector: Any = None,
        response: Any = None,
        parent: ItemLoader | None = None,
        stats: _Stats | None = None,
        **context: Any,
    ):
        if selector is None and response is not None:
            # A Scrapy response carries its selector; a Netweir Page is one.
            selector = getattr(response, "selector", response)
        self.selector = selector
        self.stats: _Stats | None = stats if stats is not None or parent is None else parent.stats
        given = item is not None
        if parent is not None:
            item = parent.item
            self._values: dict[str, list[Any]] = parent._values
        else:
            if item is None:
                item = self.default_item_class()
            self._values = {}
        _check_item(item)
        self.item: Any = item
        self.parent = parent
        context["response"] = response
        context["selector"] = selector
        context["item"] = item
        self.context: MutableMapping[str, Any] = context
        if given and parent is None:
            # An item made here holds only its class's defaults, which
            # aren't values anyone loaded; a field that's None holds nothing.
            for name, value in _item_values(item):
                if value is not None:
                    self._values.setdefault(name, []).extend(_as_list(value))

    # --- nesting

    def nested_css(self: _L, css: str, **context: Any) -> _L:
        """A loader for what ``css`` finds below this one's selector,
        filling the same item. Its context is this one's, plus ``context``."""
        return self._nested(self._selector().css(css), context)

    def nested_xpath(self: _L, xpath: str, **context: Any) -> _L:
        """``nested_css`` with an XPath."""
        return self._nested(self._selector().xpath(xpath), context)

    def _nested(self: _L, selector: Any, context: dict[str, Any]) -> _L:
        inherited = {k: v for k, v in self.context.items() if k != "item"}
        inherited.update(context)
        inherited["selector"] = selector
        return self.__class__(parent=self, **inherited)

    # --- values

    def add_value(
        self: _L,
        field_name: str | None,
        value: Any,
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        """Adds ``value`` to what's collected for ``field_name``, after
        ``re``, the ``processors`` and the field's input processor. With
        ``field_name`` None, the processed value is a dict of fields to
        values, and each is added. Other keyword arguments are accepted and
        ignored, as itemloaders ignores them."""
        value = self.get_value(value, *processors, re=re)
        if value is None:
            return self
        if not field_name:
            for name, v in value.items():
                self._add(name, v)
        else:
            self._add(field_name, value)
        return self

    def replace_value(
        self: _L,
        field_name: str | None,
        value: Any,
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        """``add_value``, throwing away what the field had collected."""
        value = self.get_value(value, *processors, re=re)
        if value is None:
            return self
        if not field_name:
            for name, v in value.items():
                self._values.pop(name, None)
                self._add(name, v)
        else:
            self._values.pop(field_name, None)
            self._add(field_name, value)
        return self

    def _add(self, field_name: str, value: Any) -> None:
        processed = self._process_input_value(field_name, _as_list(value))
        # An input processor that gives nothing (None, [], "") adds nothing.
        if processed:
            self._values.setdefault(field_name, []).extend(_as_list(processed))

    def get_value(
        self,
        value: Any,
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> Any:
        """``value`` after ``re`` (every match in every value: the whole
        match, or its groups) and then each of ``processors``, stopping at
        None. Collects nothing."""
        if re:
            value = _regex.extract(re, _as_list(value))  # type: ignore[arg-type]
        context = self.context
        for proc in processors:
            if value is None:
                break
            # A processor that isn't callable is a TypeError of its own,
            # not wrapped as the processor's failure.
            wants = _wants_context(proc)
            try:
                value = proc(value, loader_context=context) if wants else proc(value)
            except Exception as e:
                raise ValueError(
                    f"Error with processor {type(proc).__name__} value={value!r} "
                    f"error='{type(e).__name__}: {e!s}'"
                ) from e
        return value

    def load_item(self) -> Any:
        """The item, with every collected field set to its output
        processor's answer (a field whose answer is None is left alone)."""
        item = self.item
        for field_name in tuple(self._values):
            value = self.get_output_value(field_name)
            if value is not None:
                _set_field(item, field_name, value)
        return item

    def get_output_value(self, field_name: str) -> Any:
        """The field's collected values through its output processor,
        without touching the item."""
        proc = self.get_output_processor(field_name)
        value = self._values.get(field_name, [])
        wants = _wants_context(proc)
        try:
            return proc(value, loader_context=self.context) if wants else proc(value)
        except Exception as e:
            raise ValueError(
                f"Error with output processor: field={field_name!r} value={value!r} "
                f"error='{type(e).__name__}: {e!s}'"
            ) from e

    def get_collected_values(self, field_name: str) -> list[Any]:
        """The field's values so far, before its output processor."""
        return self._values.get(field_name, [])

    def get_input_processor(self, field_name: str) -> Callable[..., Any]:
        proc = getattr(self, f"{field_name}_in", None)
        if not proc:
            proc = _field_meta(self.item, field_name).get(
                "input_processor", self.default_input_processor
            )
        return _unbound(proc)

    def get_output_processor(self, field_name: str) -> Callable[..., Any]:
        proc = getattr(self, f"{field_name}_out", None)
        if not proc:
            proc = _field_meta(self.item, field_name).get(
                "output_processor", self.default_output_processor
            )
        return _unbound(proc)

    def _process_input_value(self, field_name: str, value: Any) -> Any:
        proc = self.get_input_processor(field_name)
        wants = _wants_context(proc)
        try:
            return proc(value, loader_context=self.context) if wants else proc(value)
        except Exception as e:
            raise ValueError(
                f"Error with input processor {type(proc).__name__}: field={field_name!r} "
                f"value={value!r} error='{type(e).__name__}: {e!s}'"
            ) from e

    # --- queries

    def _selector(self) -> Any:
        if self.selector is None:
            raise RuntimeError(
                f"To use XPath or CSS selectors, {type(self).__name__} "
                "must be instantiated with a selector"
            )
        return self.selector

    def _track(self, field_name: str | None, kind: str, rule: str, values: list[Any]) -> None:
        if self.stats is not None:
            key = f"parser/{field_name or '<none>'}/{kind}/{rule}"
            self.stats.inc_value(key, 1 if values else 0)

    def _css(self, queries: str | Iterable[str], field_name: str | None = None) -> list[Any]:
        selector = self._selector()
        out: list[Any] = []
        for query in [queries] if isinstance(queries, str) else queries:
            found = selector.css(query).getall()
            self._track(field_name, "css", query, found)
            out += found
        return out

    def _xpath(
        self, queries: str | Iterable[str], field_name: str | None = None, **kw: Any
    ) -> list[Any]:
        selector = self._selector()
        out: list[Any] = []
        for query in [queries] if isinstance(queries, str) else queries:
            found = selector.xpath(query, **kw).getall()
            self._track(field_name, "xpath", query, found)
            out += found
        return out

    def _jmes(self, queries: str | Iterable[str], field_name: str | None = None) -> list[Any]:
        selector = self._selector()
        if not hasattr(selector, "jmespath"):
            raise AttributeError(f"{type(selector).__name__} has no JMESPath support")
        out: list[Any] = []
        for query in [queries] if isinstance(queries, str) else queries:
            found = selector.jmespath(query)
            if hasattr(found, "getall"):  # parsel's
                found = found.getall()
            elif not isinstance(selector, Selection):
                # A Page or Node gives the answer itself, None for nothing;
                # a Selection gives a list of its results' answers.
                found = [] if found is None else [found]
            found = _flatten_json(found, [])
            self._track(field_name, "jmes", query, found)
            out += found
        return out

    def add_css(
        self: _L,
        field_name: str | None,
        css: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        """``add_value`` with every string ``css`` finds (a list of queries
        collects what each finds)."""
        return self.add_value(field_name, self._css(css, field_name), *processors, re=re)

    def replace_css(
        self: _L,
        field_name: str | None,
        css: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        return self.replace_value(field_name, self._css(css, field_name), *processors, re=re)

    def get_css(
        self,
        css: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> Any:
        return self.get_value(self._css(css), *processors, re=re)

    def add_xpath(
        self: _L,
        field_name: str | None,
        xpath: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        """``add_css`` with XPath; keyword arguments bind ``$variables``."""
        values = self._xpath(xpath, field_name, **kw)
        return self.add_value(field_name, values, *processors, re=re)

    def replace_xpath(
        self: _L,
        field_name: str | None,
        xpath: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        values = self._xpath(xpath, field_name, **kw)
        return self.replace_value(field_name, values, *processors, re=re)

    def get_xpath(
        self,
        xpath: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> Any:
        return self.get_value(self._xpath(xpath, **kw), *processors, re=re)

    def add_jmes(
        self: _L,
        field_name: str | None,
        jmes: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        """``add_css`` with a JMESPath expression over the selector's JSON
        (a Page's body, a Node's text, each of a Selection's results). Lists
        it finds are taken apart into values; objects stay whole."""
        return self.add_value(field_name, self._jmes(jmes, field_name), *processors, re=re)

    def replace_jmes(
        self: _L,
        field_name: str | None,
        jmes: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> _L:
        return self.replace_value(field_name, self._jmes(jmes, field_name), *processors, re=re)

    def get_jmes(
        self,
        jmes: str | Iterable[str],
        *processors: Callable[..., Any],
        re: str | Pattern[str] | None = None,
        **kw: Any,
    ) -> Any:
        return self.get_value(self._jmes(jmes), *processors, re=re)
