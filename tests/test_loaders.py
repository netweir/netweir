"""Item loaders: netweir.loader, as Scrapy's itemloaders."""

import dataclasses
import gc
import json
import re
import threading
import weakref
from functools import partial
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest
import worker_spiders

import netweir
from netweir._native import _response
from netweir.loader import (
    Compose,
    Identity,
    ItemLoader,
    Join,
    MapCompose,
    SelectJmes,
    TakeFirst,
)

PRODUCT = """<html><head><base href="https://shop.example/catalogue/"></head><body>
<div class="product">
  <h1>  Oak Lamp </h1>
  <p class="price">Now £1,234.50 (was £1,500)</p>
  <ul class="tags"><li>oak</li><li>lamp</li><li></li></ul>
  <a class="img" href="img/lamp.jpg">picture</a>
  <p class="sku">SKU: 4417</p>
</div>
<footer><a href="/about">About</a></footer>
<script id="data" type="application/json">{"product": {"sku": "4417", "stock": 3,
  "sizes": [["S", "M"], ["L"]], "dims": {"w": 20}}}</script>
</body></html>"""


def page(html=PRODUCT, url="https://shop.example/catalogue/lamp", kind="text/html"):
    return netweir.Page(_response(url, 200, "HTTP/1.1", [("content-type", kind)], html.encode()))


def json_page(data):
    return page(json.dumps(data), "https://shop.example/api/lamp", "application/json")


@dataclasses.dataclass
class Product:
    name: str | None = None
    price: str | None = None
    url: str | None = None
    sku: str | None = None
    tags: list[str] = dataclasses.field(
        default_factory=list, metadata={"output_processor": Identity()}
    )


class ProductLoader(ItemLoader):
    default_item_class = Product
    default_output_processor = TakeFirst()
    name_in = MapCompose(str.strip)


# --- the whole thing ------------------------------------------------------------


def test_a_product_page_loads_into_a_dataclass():
    p = page()
    loader = ProductLoader(item=Product(), response=p)
    loader.add_css("name", "h1::text")
    loader.add_xpath("price", "//p[@class='price']/text()", MapCompose(str.strip), re=r"[\d,.]+")
    loader.add_value("url", p.url)
    loader.nested_css("#data::text").add_jmes("sku", "product.sku")
    loader.add_css("tags", "ul.tags li::text")
    item = loader.load_item()
    assert item == Product(
        name="Oak Lamp",
        price="1,234.50",
        url="https://shop.example/catalogue/lamp",
        sku="4417",
        tags=["oak", "lamp"],
    )


def test_the_default_item_is_a_dict_and_values_are_lists():
    loader = ItemLoader(response=page())
    loader.add_css("tags", "ul.tags li::text")
    loader.add_value("seen", 1)
    assert loader.load_item() == {"tags": ["oak", "lamp"], "seen": [1]}


def test_methods_chain():
    item = (
        ItemLoader(response=page())
        .add_css("name", "h1::text", MapCompose(str.strip))
        .add_value("x", "y")
        .load_item()
    )
    assert item == {"name": ["Oak Lamp"], "x": ["y"]}


# --- selectors -------------------------------------------------------------------


def test_a_node_or_a_selection_is_a_selector_too():
    p = page()
    node = p.css("div.product")[0]
    assert ItemLoader(selector=node).get_css("h1::text", TakeFirst()) == "  Oak Lamp "
    sel = p.css("ul.tags")
    assert ItemLoader(selector=sel).get_xpath("./li/text()") == ["oak", "lamp"]
    assert ItemLoader(selector=p.root).get_css("li::text") == ["oak", "lamp"]


def test_response_and_selector_are_in_the_context():
    p = page()
    loader = ItemLoader(response=p, currency="GBP")
    assert loader.selector is p
    assert loader.context["response"] is p
    assert loader.context["selector"] is p
    assert loader.context["item"] is loader.item
    assert loader.context["currency"] == "GBP"


def test_a_selector_given_wins_over_the_response():
    p = page()
    node = p.css("footer")[0]
    loader = ItemLoader(selector=node, response=p)
    assert loader.get_css("a::attr(href)") == ["/about"]
    assert loader.context["response"] is p


def test_queries_need_a_selector():
    loader = ItemLoader()
    assert loader.selector is None
    for call in (loader.add_css, loader.add_xpath, loader.add_jmes):
        with pytest.raises(RuntimeError, match="selector"):
            call("x", "y")
    with pytest.raises(RuntimeError):
        loader.nested_css("p")


def test_several_queries_collect_what_each_finds():
    loader = ItemLoader(response=page())
    loader.add_css("t", ["h1::text", "nothing::text", "footer a::text"], MapCompose(str.strip))
    loader.add_xpath("t", ["//li[1]/text()", "//li[2]/text()"])
    assert loader.get_collected_values("t") == ["Oak Lamp", "About", "oak", "lamp"]


def test_xpath_variables():
    loader = ItemLoader(response=page())
    loader.add_xpath("tag", "//li[. = $t]/text()", t="lamp")
    assert loader.get_output_value("tag") == ["lamp"]
    assert loader.get_xpath("//li[position() = $n]/text()", n=1) == ["oak"]


def test_a_parsel_selector_works_as_well():
    parsel = pytest.importorskip("parsel")
    loader = ItemLoader(selector=parsel.Selector(text=PRODUCT))
    loader.add_css("name", "h1::text", MapCompose(str.strip))
    loader.add_jmes("x", "a", MapCompose(str))
    assert loader.load_item() == {"name": ["Oak Lamp"]}


def test_a_scrapy_response_gives_its_selector():
    http = pytest.importorskip("scrapy.http")
    response = http.HtmlResponse(url="https://shop.example/", body=PRODUCT.encode())
    loader = ItemLoader(response=response)
    assert loader.selector is response.selector
    loader.add_css("name", "h1::text", MapCompose(str.strip))
    assert loader.load_item() == {"name": ["Oak Lamp"]}


# --- re= -------------------------------------------------------------------------


def test_re_keeps_matches_or_groups():
    loader = ItemLoader(response=page())
    assert loader.get_css(".price::text", re=r"£([\d,.]+)") == ["1,234.50", "1,500"]
    assert loader.get_css(".price::text", TakeFirst(), re=r"£[\d,.]+") == "£1,234.50"
    assert loader.get_value(["a1 b2", "c3"], re=r"(\w)(\d)") == ["a", "1", "b", "2", "c", "3"]
    assert loader.get_value("x: 1, y: 2", re=r"y: (?P<extract>\d)") == ["2"]
    assert loader.get_value("name: foo", TakeFirst(), str.upper, re="name: (.+)") == "FOO"
    assert loader.get_value("&pound;5 &amp; &lt;", re=".+") == ["£5 &amp; &lt;"]


def test_re_takes_a_compiled_pattern():
    loader = ItemLoader(response=page())
    loader.add_css("sku", ".sku::text", re=re.compile(r"(\d+)"))
    loader.replace_xpath("price", "//p[@class='price']/text()", re=re.compile(r"\((.+)\)"))
    assert loader.load_item() == {"sku": ["4417"], "price": ["was £1,500"]}


def test_re_that_matches_nothing_adds_nothing():
    loader = ItemLoader()
    loader.add_value("x", "y", re="z")
    assert loader.load_item() == {}


# --- processors ----------------------------------------------------------------


def test_take_first_skips_none_and_empty_strings():
    assert TakeFirst()(["", None, "one", "two"]) == "one"
    assert TakeFirst()([0, "x"]) == 0
    assert TakeFirst()([]) is None
    assert TakeFirst()(["", None]) is None


def test_identity_and_join():
    assert Identity()(["a", "b"]) == ["a", "b"]
    assert Join()(["a", "b"]) == "a b"
    assert Join("<br>")(["a", "b"]) == "a<br>b"
    assert Join()([]) == ""


def test_compose_stops_at_none_unless_told_not_to():
    assert Compose(lambda v: v[0], str.upper)(["hello", "world"]) == "HELLO"
    assert Compose(lambda v: None, str.upper)(["x"]) is None
    assert Compose(lambda v: None, lambda v: "ran", stop_on_none=False)(["x"]) == "ran"
    assert Compose(str.upper)(None) is None


def test_map_compose_flattens_and_drops_none():
    def drop_world(v):
        return None if v == "world" else v

    assert MapCompose(drop_world, str.upper)(["hello", "world", "x"]) == ["HELLO", "X"]
    assert MapCompose(str.split)(["a b", "c"]) == ["a", "b", "c"]
    assert MapCompose(lambda v: (v, v))(["a"]) == ["a", "a"]
    assert MapCompose()(["a"]) == ["a"]
    assert MapCompose(str.upper)("one") == ["ONE"]
    assert MapCompose(str.upper)(None) == []
    # A dict is one value, not its keys.
    assert MapCompose(lambda d: d)([{"a": 1}]) == [{"a": 1}]


def test_processor_errors_say_where():
    with pytest.raises(ValueError, match=r"Error in MapCompose .* error='ValueError: could not"):
        MapCompose(float)(["x"])
    with pytest.raises(ValueError, match=r"Error in Compose .* error='TypeError"):
        Compose(float)(["x"])


def test_select_jmes():
    pytest.importorskip("jmespath")
    assert SelectJmes("foo")({"foo": {"bar": 1}}) == {"bar": 1}
    assert SelectJmes("foo[0]")({"foo": [1, 2]}) == 1
    assert Compose(json.loads, MapCompose(SelectJmes("a")))('[{"a": 1}, {"b": 2}]') == [1]


def test_select_jmes_without_the_package(monkeypatch):
    import builtins

    real = builtins.__import__

    def no_jmespath(name, *args, **kwargs):
        if name == "jmespath":
            raise ImportError(name)
        return real(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", no_jmespath)
    with pytest.raises(ImportError, match=r"netweir\[jmespath\]"):
        SelectJmes("a")


def test_processors_given_to_add_run_before_the_input_processor():
    class Loader(ItemLoader):
        name_in = MapCompose(lambda v: v + "!")

    loader = Loader()
    loader.add_value("name", ["a", "b"], Compose(lambda vs: vs[::-1]))
    assert loader.get_collected_values("name") == ["b!", "a!"]


def test_an_input_processor_gets_a_list():
    seen = []

    class Loader(ItemLoader):
        def name_in(self, values):
            seen.append(values)
            return values[0]

    loader = Loader()
    loader.add_value("name", "only")
    loader.add_value("name", ["a", "b"])
    assert seen == [["only"], ["a", "b"]]
    assert loader.get_collected_values("name") == ["only", "a"]


def test_values_an_input_processor_empties_are_not_collected():
    class Loader(ItemLoader):
        name_in = MapCompose(lambda v: None)
        flag_in = TakeFirst()

    loader = Loader()
    loader.add_value("name", "x")
    loader.add_value("flag", 0)  # falsy: dropped, as itemloaders drops it
    loader.add_value("other", 0)
    loader.add_value("none", None)
    assert loader.load_item() == {"other": [0]}


def test_set_and_zero_and_dict_values():
    loader = ItemLoader()
    loader.add_value("s", {"b"})
    loader.add_value("z", 0)
    loader.add_value("d", {"k": "v"})
    loader.add_value("nested", [[1, 2]])
    loader.add_value(None, {"many": 1, "more": [2, 3]})
    assert loader.load_item() == {
        "s": ["b"],
        "z": [0],
        "d": [{"k": "v"}],
        "nested": [[1, 2]],
        "many": [1],
        "more": [2, 3],
    }


# --- field processors ------------------------------------------------------------


def clean(values):
    return [v.strip().title() for v in values]


class CleanLoader(ItemLoader):
    name_in = clean  # a plain function, not a method
    default_output_processor = Join("+")


def test_field_processors_from_the_loader_class():
    loader = CleanLoader()
    loader.add_value("name", [" ann ", "bob"])
    loader.add_value("other", ["x", "y"])
    assert loader.load_item() == {"name": "Ann+Bob", "other": "x+y"}


def test_processors_are_inherited_and_overridden():
    class Child(CleanLoader):
        default_input_processor = MapCompose(str.upper)
        other_out = TakeFirst()

    loader = Child()
    loader.add_value("name", " cy ")
    loader.add_value("other", ["x", "y"])
    loader.add_value("third", ["p", "q"])
    assert loader.load_item() == {"name": "Cy", "other": "X", "third": "P+Q"}


def test_a_method_processor_can_read_the_loader():
    class Loader(ItemLoader):
        url_out = TakeFirst()

        def img_out(self, values):
            return (self.get_output_value("url") or "") + values[0]

    loader = Loader()
    loader.add_value("url", "https://a.test/")
    loader.add_value("img", "1.png")
    assert loader.load_item() == {"url": "https://a.test/", "img": "https://a.test/1.png"}


def test_processors_on_an_instance():
    loader = ItemLoader()
    loader.default_input_processor = MapCompose(str.upper)
    loader.default_output_processor = TakeFirst()
    loader.add_value("a", ["x", "y"])
    assert loader.load_item() == {"a": "X"}


def test_output_none_leaves_the_field_alone():
    def digits(v):
        return v if v.isdigit() else None

    class Loader(ItemLoader):
        name_out = Compose(lambda vs: vs[0])
        sku_out = Compose(TakeFirst(), digits)

    loader = Loader()
    loader.add_value("sku", ["SKU: n/a"], re="SKU: (.+)")
    loader.add_value("name", "")
    assert loader.load_item() == {"name": ""}
    loader.replace_value("sku", ["SKU: 12"], re="SKU: (.+)")
    assert loader.load_item()["sku"] == "12"


def test_processor_errors_name_the_field():
    class Loader(ItemLoader):
        name_in = MapCompose(float)
        price_out = Compose(Join(), float)

    loader = Loader()
    with pytest.raises(ValueError, match=r"input processor MapCompose: field='name'"):
        loader.add_value("name", "x")
    loader.add_value("price", "y")
    with pytest.raises(ValueError, match=r"output processor: field='price'"):
        loader.load_item()
    with pytest.raises(ValueError, match=r"Error with processor Compose"):
        loader.add_value("other", ["x"], Compose(float))


def test_a_processor_must_be_callable():
    with pytest.raises(TypeError, match="callable"):
        ItemLoader().add_value("a", "b", "not a function")


# --- context ---------------------------------------------------------------------


def test_processors_that_ask_for_the_context_get_it():
    p = page()

    def absolute(value, loader_context):
        return loader_context["response"].urljoin(value)

    loader = ItemLoader(response=p)
    loader.add_css("image", "a.img::attr(href)", MapCompose(absolute))
    assert loader.load_item() == {"image": ["https://shop.example/catalogue/img/lamp.jpg"]}


def test_context_from_the_processor_the_loader_and_later_assignment():
    def unit(value, loader_context):
        return f"{value} {loader_context['unit']}"

    class Loader(ItemLoader):
        size_in = MapCompose(unit, unit="cm")

    loader = Loader()
    loader.add_value("size", "3")
    loader = Loader(unit="mm")
    loader.add_value("size", "3")
    loader.context["unit"] = "in"
    loader.add_value("size", "4")
    assert loader.get_collected_values("size") == ["3 mm", "4 in"]
    assert Loader().get_value("5", Compose(unit, unit="m")) == "5 m"


def test_the_item_is_in_the_context():
    def from_item(value, loader_context):
        return f"{loader_context['item']['brand']} {value}"

    loader = ItemLoader(item={"brand": "Acme"})
    loader.add_value("name", "Lamp", MapCompose(from_item))
    assert loader.get_collected_values("name") == ["Acme Lamp"]


def test_partials_and_callable_objects_get_context_by_their_signature():
    def join(values, sep=None, loader_context=None):
        if sep is not None:
            return sep.join(values)
        return (loader_context or {}).get("sep", "").join(values)

    class Joiner:
        def __call__(self, values, loader_context):
            return loader_context["sep"].join(values)

    loader = ItemLoader(sep="-")
    assert loader.get_value(["a", "b"], partial(join, sep="+")) == "a+b"
    assert loader.get_value(["a", "b"], partial(join, loader_context={"sep": "."})) == "a.b"
    assert loader.get_value(["a", "b"], join) == "a-b"
    assert loader.get_value(["a", "b"], Joiner()) == "a-b"


def test_builtins_work_as_processors():
    loader = ItemLoader()
    assert loader.get_value([" a "], MapCompose(str.strip), TakeFirst(), str.upper) == "A"
    assert loader.get_value(["3"], TakeFirst(), int) == 3


# --- items -----------------------------------------------------------------------


def test_a_dataclass_takes_processors_from_field_metadata():
    @dataclasses.dataclass
    class Item:
        price: Any = dataclasses.field(
            default=None,
            metadata={
                "input_processor": MapCompose(netweir.parse_price),
                "output_processor": TakeFirst(),
            },
        )
        name: Any = None

    loader = ItemLoader(item=Item(), response=page())
    loader.add_css("price", ".price::text")
    loader.add_css("name", "h1::text")
    item = loader.load_item()
    assert item.price == netweir.parse_price("£1,234.50")
    assert item.name == ["  Oak Lamp "]


def test_loader_attributes_win_over_field_metadata():
    @dataclasses.dataclass
    class Item:
        name: Any = dataclasses.field(default=None, metadata={"input_processor": str.upper})

    class Loader(ItemLoader):
        name_in = MapCompose(str.lower)

    loader = Loader(item=Item())
    loader.add_value("name", "MiXed")
    assert loader.load_item().name == ["mixed"]


def test_a_dataclass_rejects_fields_it_does_not_have():
    loader = ItemLoader(item=Product())
    with pytest.raises(KeyError, match="colour"):
        loader.add_value("colour", "red")


def test_a_dataclass_built_by_the_loader_keeps_none_for_fields_not_loaded():
    loader = ProductLoader()
    loader.add_value("name", " x ")
    item = loader.load_item()
    assert item == Product(name="x")
    assert ProductLoader().load_item() == Product()


def test_an_items_existing_values_count_as_collected():
    loader = ItemLoader(item={"name": "first", "tags": ["a"], "gone": None})
    loader.add_value("name", "second")
    loader.add_value("tags", "b")
    assert loader.get_collected_values("name") == ["first", "second"]
    # None is no value: the field isn't turned into an empty list.
    assert loader.load_item() == {"name": ["first", "second"], "tags": ["a", "b"], "gone": None}


def test_a_given_dataclass_keeps_its_defaults_as_values():
    @dataclasses.dataclass
    class Item:
        name: Any = "default"
        price: Any = None

    loader = ItemLoader(item=Item())
    loader.add_value("name", "x")
    assert loader.load_item() == Item(name=["default", "x"], price=None)


def test_attrs_items():
    attr = pytest.importorskip("attr")

    @attr.s
    class Item:
        name = attr.ib(default=None, metadata={"output_processor": TakeFirst()})
        tags = attr.ib(default=None)

    loader = ItemLoader(item=Item(), response=page())
    loader.add_css("name", "h1::text", MapCompose(str.strip))
    loader.add_css("tags", "li::text")
    assert loader.load_item() == Item(name="Oak Lamp", tags=["oak", "lamp"])
    with pytest.raises(KeyError, match="other"):
        loader.add_value("other", 1)


def test_scrapy_items():
    scrapy = pytest.importorskip("scrapy")

    class Item(scrapy.Item):
        name = scrapy.Field(input_processor=MapCompose(str.strip), output_processor=TakeFirst())
        tags = scrapy.Field()

    loader = ItemLoader(item=Item(), response=page())
    loader.add_css("name", "h1::text")
    loader.add_css("tags", "li::text")
    item = loader.load_item()
    assert isinstance(item, Item)
    assert dict(item) == {"name": "Oak Lamp", "tags": ["oak", "lamp"]}
    with pytest.raises(KeyError):
        loader.add_value("other", 1)
    loader = ItemLoader(item=Item(name="kept"))
    assert loader.get_collected_values("name") == ["kept"]


def test_dict_subclasses_and_items_as_values():
    class Record(dict):
        pass

    @dataclasses.dataclass
    class Inner:
        a: int

    class Loader(ItemLoader):
        default_item_class = Record

    loader = Loader()
    loader.add_value("inner", Inner(1))
    item = loader.load_item()
    assert type(item) is Record
    assert item == {"inner": [Inner(1)]}


def test_items_of_other_kinds_are_refused():
    with pytest.raises(TypeError, match="takes a dict, a dataclass"):
        ItemLoader(item=["not", "an", "item"])
    with pytest.raises(TypeError, match="takes a dict, a dataclass"):
        ItemLoader(item=Product)  # the class, not an instance

    class Book(netweir.Item):
        title = netweir.css("h1::text")

    with pytest.raises(TypeError, match=r"Book.extract\(page\)"):
        ItemLoader(item=Book())
    with pytest.raises(TypeError, match="netweir.Item"):
        ItemLoader(item=Book)


def test_a_declarative_item_can_start_a_loader():
    class Book(netweir.Item):
        title = netweir.css("h1::text", strip=True)

    p = page()
    loader = ItemLoader(item=Book.extract(p), response=p)
    loader.default_output_processor = TakeFirst()
    loader.add_css("sku", ".sku::text", re=r"\d+")
    assert loader.load_item() == {"title": "Oak Lamp", "sku": "4417"}


# --- getting and replacing ---------------------------------------------------------


def test_get_methods_collect_nothing():
    loader = ItemLoader(response=page())
    assert loader.get_css("h1::text", MapCompose(str.strip)) == ["Oak Lamp"]
    assert loader.get_xpath("//h1/text()", TakeFirst(), str.strip, str.upper) == "OAK LAMP"
    assert loader.get_value(None, TakeFirst()) is None
    assert loader.get_value("x") == "x"
    assert loader.load_item() == {}
    assert loader.get_output_value("missing") == []
    assert loader.get_collected_values("missing") == []


def test_replace_methods_throw_away_what_was_collected():
    loader = ItemLoader(response=page())
    loader.add_value("name", "old")
    loader.replace_css("name", "h1::text", MapCompose(str.strip))
    assert loader.get_collected_values("name") == ["Oak Lamp"]
    loader.replace_xpath("name", "//footer//a/text()")
    assert loader.get_collected_values("name") == ["About"]
    loader.replace_value("name", None)  # None replaces nothing
    assert loader.get_collected_values("name") == ["About"]
    loader.add_value(None, {"a": 1, "b": 2})
    loader.replace_value(None, {"a": 3})
    assert loader.load_item() == {"name": ["About"], "a": [3], "b": [2]}


# --- JMESPath --------------------------------------------------------------------


def test_jmes_on_a_json_page():
    pytest.importorskip("jmespath")
    loader = ItemLoader(response=json_page({"items": [{"n": "a"}, {"n": "b"}], "total": 2}))
    loader.add_jmes("names", "items[].n")
    loader.add_jmes("total", "total")
    loader.add_jmes("nothing", "missing")
    loader.add_jmes("first", "items[0]")
    assert loader.load_item() == {"names": ["a", "b"], "total": [2], "first": [{"n": "a"}]}
    assert loader.get_jmes(["total", "items[1].n"]) == [2, "b"]
    assert loader.get_jmes("items[0].n", TakeFirst(), re="(.)") == "a"


def test_jmes_takes_lists_apart_and_keeps_objects_whole():
    pytest.importorskip("jmespath")
    loader = ItemLoader(response=page()).nested_css("#data::text")
    assert loader.get_jmes("product.sizes") == ["S", "M", "L"]
    # parsel would give the object's keys here: ['w'].
    assert loader.get_jmes("product.dims") == [{"w": 20}]
    loader.replace_jmes("stock", "product.stock")
    assert loader.load_item() == {"stock": [3]}


def test_jmes_on_a_node():
    pytest.importorskip("jmespath")
    node = page().find("script", id="data")
    assert ItemLoader(selector=node).get_jmes("product.sku") == ["4417"]


# --- nested loaders --------------------------------------------------------------


def test_nested_loaders_fill_the_same_item():
    loader = ItemLoader(response=page())
    product = loader.nested_css("div.product")
    footer = loader.nested_xpath("//footer")
    product.add_css("name", "h1::text", MapCompose(str.strip))
    footer.add_css("about", "a::attr(href)")
    loader.add_value("url", "u")
    product.add_xpath("first_tag", ".//li[1]/text()")
    assert product.item is loader.item
    assert footer.load_item() == {
        "name": ["Oak Lamp"],
        "about": ["/about"],
        "url": ["u"],
        "first_tag": ["oak"],
    }
    assert loader.load_item() is product.load_item()


def test_nested_loaders_see_replacements_in_order():
    loader = ItemLoader(response=page())
    product = loader.nested_css("div.product")
    loader.add_value("name", "a")
    product.add_value("name", "b")
    product.replace_value("name", "c")
    loader.add_value("name", "d")
    assert loader.get_collected_values("name") == ["c", "d"]


def test_a_nested_loader_on_nothing_adds_nothing():
    loader = ItemLoader(response=page())
    loader.nested_css("#missing").add_css("x", "p::text")
    loader.nested_xpath("//missing").add_xpath("y", ".//p/text()")
    assert loader.load_item() == {}


def test_nested_loaders_inherit_the_context():
    p = page()
    marker = object()
    loader = ItemLoader(response=p, marker=marker, prefix="parent")
    child = loader.nested_css("footer", prefix="child")
    sibling = loader.nested_xpath("//footer")
    grandchild = child.nested_css("a")
    for nested in (child, sibling, grandchild):
        assert nested.context["marker"] is marker
        assert nested.context["item"] is loader.item
        assert nested.context["response"] is p
        assert nested.context["selector"] is nested.selector
    assert child.context["prefix"] == grandchild.context["prefix"] == "child"
    assert sibling.context["prefix"] == "parent"
    child.context["new"] = True
    assert "new" not in loader.context

    grandchild.default_input_processor = MapCompose(
        lambda v, loader_context: loader_context["response"].urljoin(v)
    )
    grandchild.add_css("about", "::attr(href)")
    assert loader.load_item() == {"about": ["https://shop.example/about"]}


def test_nesting_does_not_add_the_items_values_again():
    # itemloaders reads a given item's values into the shared list once per
    # nested loader, so {"a": 1} loads as {"a": [1, 1]}.
    loader = ItemLoader(item={"a": 1}, response=page())
    loader.nested_css("div").nested_css("h1")
    assert loader.load_item() == {"a": [1]}


def test_nested_loaders_keep_the_loader_class():
    loader = ProductLoader(response=page())
    nested = loader.nested_css("div.product")
    assert type(nested) is ProductLoader
    nested.add_css("name", "h1::text")
    assert loader.load_item().name == "Oak Lamp"


# --- memory --------------------------------------------------------------------


def test_a_dropped_loader_and_its_page_are_let_go():
    # Methods as processors are bound to their loader; remembering whether
    # they take the context mustn't keep the loader, and its page, alive.
    class Loader(ItemLoader):
        def name_in(self, values):
            return values

        def clean(self, value, loader_context):
            return value

        def unused(self, value):
            return value

    refs = []
    for _ in range(5):
        p = page()
        loader = Loader(response=p)
        loader.add_css("name", "h1::text")
        loader.add_css("tag", "li::text", MapCompose(loader.clean), Compose(loader.unused))
        loader.add_value("x", "y", partial(Loader.unused, loader))
        loader.load_item()
        refs.append(weakref.ref(loader))  # a Page can't be weakly referenced
        del loader, p
    gc.collect()
    assert [r() for r in refs] == [None] * 5


# --- rule statistics -------------------------------------------------------------


class Stats:
    def __init__(self):
        self.values = {}

    def inc_value(self, key, count=1):
        self.values[key] = self.values.get(key, 0) + count


def test_stats_count_which_queries_found_something():
    stats = Stats()
    loader = ItemLoader(response=page(), stats=stats)
    loader.add_css("name", ["h1::text", "h2::text"])
    loader.nested_xpath("//footer").get_xpath(".//nothing")
    loader.get_css("li::text")
    assert stats.values == {
        "parser/name/css/h1::text": 1,
        "parser/name/css/h2::text": 0,
        "parser/<none>/xpath/.//nothing": 0,
        "parser/<none>/css/li::text": 1,
    }


# --- against itemloaders ---------------------------------------------------------


def test_the_same_answers_as_itemloaders():
    """Where itemloaders and parsel are installed (Scrapy brings both), the
    same calls on the same page give the same item."""
    itemloaders = pytest.importorskip("itemloaders")
    processors = pytest.importorskip("itemloaders.processors")
    parsel = pytest.importorskip("parsel")

    def run(loader_class, mapc, take_first, join, selector):
        class Loader(loader_class):
            default_output_processor = take_first()
            name_in = mapc(str.strip, str.title)
            tags_out = join("|")

            def first_tag_out(self, values):
                return values[-1] if values else None

        loader = Loader(selector=selector, shop="A")
        loader.add_css("name", "h1::text")
        loader.add_xpath("price", "//p[@class='price']/text()", re=r"£([\d,.]+)")
        loader.add_css("tags", ["ul.tags li::text", "footer a::text"])
        loader.add_value("tags", ["x", ""])
        loader.add_value(None, {"shop": "A", "empty": []})
        nested = loader.nested_css("div.product")
        nested.add_xpath("first_tag", ".//li/text()")
        nested.replace_css("img", "a.img::attr(href)", mapc(lambda v, loader_context: v + "?"))
        loader.add_value("ctx", "v", mapc(lambda v, loader_context: loader_context["shop"]))
        got = loader.get_css(".sku::text", take_first(), re=r"\d+")
        return loader.load_item(), got, loader.get_collected_values("tags")

    theirs = run(
        itemloaders.ItemLoader,
        processors.MapCompose,
        processors.TakeFirst,
        processors.Join,
        parsel.Selector(text=PRODUCT),
    )
    ours = run(ItemLoader, MapCompose, TakeFirst, Join, page())
    assert ours == theirs


# --- in a crawl --------------------------------------------------------------------


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/":
            body = "".join(f'<a class="item" href="/p/{i}">p</a>' for i in range(6))
        else:
            n = self.path.rsplit("/", 1)[1]
            body = f'<h1> Lamp {n} </h1><p class="price">£{n}.50</p><li>a</li><li>b</li>'
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def crawl(base, **kw):
    items = []
    spider = worker_spiders.Loads()
    spider.start_urls = [base + "/"]
    spider.settings = netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, **kw
    )
    spider.run(output=items.append)
    return sorted(items, key=lambda i: i.url)


def test_a_spider_loads_items_in_one_process_and_in_workers(base):
    one = crawl(base)
    assert len(one) == 6
    assert one[0] == worker_spiders.Listing(
        name="Lamp 0",
        price=netweir.parse_price("£0.50"),
        url=base + "/p/0",
        tags=["a", "b"],
    )
    assert crawl(base, workers=2) == one
