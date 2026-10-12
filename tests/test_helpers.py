"""Prices, dates and the data in a page's scripts."""

import csv
import dataclasses
import datetime
import json
import math
import threading
import time
from decimal import Decimal
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
from netweir import export
from netweir._native import _response

UTC = datetime.timezone.utc
NOW = datetime.datetime(2026, 10, 12, 15, 45, 30)


def page(html, url="https://shop.example/lamp"):
    return netweir.Page(
        _response(url, 200, "HTTP/1.1", [("content-type", "text/html")], html.encode())
    )


# --- prices --------------------------------------------------------------------


@pytest.mark.parametrize(
    ("text", "amount", "currency"),
    [
        ("£1,234.50", "1234.50", "£"),
        ("1.234,50 €", "1234.50", "€"),
        ("$ 12", "12", "$"),
        ("12,99 zł", "12.99", "zł"),
        ("From USD 1,099", "1099", "USD"),
        ("1'234.50 CHF", "1234.50", "CHF"),
        ("1 234,50 €", "1234.50", "€"),
        ("From £10 to £20", "10", "£"),
        ("19.99", "19.99", None),
    ],
)
def test_a_price_is_its_amount_and_currency(text, amount, currency):
    price = netweir.parse_price(text)
    assert price == netweir.Price(Decimal(amount), currency)
    assert isinstance(price.amount, Decimal)
    assert str(price.amount) == amount, "the digits are kept as written"


@pytest.mark.parametrize(
    ("text", "amount", "currency"),
    [
        ("$.99", "0.99", "$"),
        ("€ .50", "0.50", "€"),
        ("Rs. 1,23,456", "123456", "Rs."),
        ("₹1,00,000", "100000", "₹"),
        ("-£12.00", "-12.00", "£"),
        ("−5 €", "-5", "€"),
        ("-12,50 €", "-12.50", "€"),
        ("1 299,- Kč", "1299", "Kč"),
        ("49,– €", "49", "€"),
        ("￥１，２３４", "1234", "￥"),
        ("MRP:-₹499", "499", "₹"),
        ("Cost:-500", "500", None),
        ("£-5", "-5", "£"),
        ("€ -5", "-5", "€"),
        ("Rs.-499", "-499", "Rs."),
        ("－５円", "-5", "円"),
        ("１　２３４円", "1234", "円"),
    ],
)
def test_harder_prices(text, amount, currency):
    assert netweir.parse_price(text) == netweir.Price(Decimal(amount), currency)


def test_a_negative_price_exports_with_its_sign(tmp_path):
    path = tmp_path / "items.jsonl"
    out = export.jsonl(str(path))
    out({"refund": netweir.parse_price("-£12.00")})
    out.close()
    assert json.loads(path.read_text(encoding="utf-8")) == {
        "refund": {"amount": "-12.00", "currency": "£"}
    }


@pytest.mark.parametrize("text", ["", "Free", "Sold out", "Price on request"])
def test_no_number_is_no_price(text):
    assert netweir.parse_price(text) is None


def test_a_price_knows_its_iso_code_when_it_has_one():
    assert netweir.parse_price("£5").code == "GBP"
    assert netweir.parse_price("5 €").code == "EUR"
    assert netweir.parse_price("US$5").code == "USD"
    assert netweir.parse_price("EUR 5").code == "EUR"
    assert netweir.parse_price("$5").code is None, "dollars of which country?"
    assert netweir.parse_price("¥500").code is None, "yen or yuan?"
    assert netweir.parse_price("5").code is None
    assert netweir.Price(Decimal("5"), "zł").code == "PLN"


def test_a_price_is_a_frozen_dataclass():
    price = netweir.parse_price("£5")
    with pytest.raises(dataclasses.FrozenInstanceError):
        price.amount = Decimal(6)
    assert dataclasses.asdict(price) == {"amount": Decimal("5"), "currency": "£"}
    assert hash(price) == hash(netweir.Price(Decimal("5"), "£"))


def test_a_price_exports_as_amount_and_currency(tmp_path):
    path = tmp_path / "items.jsonl"
    out = export.jsonl(str(path))
    out({"name": "Lamp", "price": netweir.parse_price("£1,234.50")})
    out({"name": "Mug", "price": netweir.parse_price("4")})
    out.close()
    lines = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
    assert lines == [
        {"name": "Lamp", "price": {"amount": "1234.50", "currency": "£"}},
        {"name": "Mug", "price": {"amount": "4", "currency": None}},
    ]


def test_a_price_in_a_csv_cell(tmp_path):
    path = tmp_path / "items.csv"
    out = export.csv(str(path))
    out({"name": "Lamp", "price": netweir.parse_price("£12.50")})
    out.close()
    with open(path, newline="", encoding="utf-8") as f:
        row = next(csv.DictReader(f))
    assert json.loads(row["price"]) == {"amount": "12.50", "currency": "£"}


# --- dates ---------------------------------------------------------------------


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("2026-10-12T09:30:00Z", datetime.datetime(2026, 10, 12, 9, 30, tzinfo=UTC)),
        ("12 Oct 2026", datetime.datetime(2026, 10, 12)),
        ("October 12th, 2026", datetime.datetime(2026, 10, 12)),
        ("Mon, 12 Oct 2026 09:30:00 GMT", datetime.datetime(2026, 10, 12, 9, 30, tzinfo=UTC)),
        ("10/12/2026", datetime.datetime(2026, 10, 12)),
        ("12 de octubre de 2026", datetime.datetime(2026, 10, 12)),
        ("12. Oktober 2026, 09:30 Uhr", datetime.datetime(2026, 10, 12, 9, 30)),
        ("lundi 12 octobre 2026 à 21h30", datetime.datetime(2026, 10, 12, 21, 30)),
        ("12 Oct 2026 9:30 PM", datetime.datetime(2026, 10, 12, 21, 30)),
        ("2026-10-12T09:30:00.25", datetime.datetime(2026, 10, 12, 9, 30, 0, 250000)),
    ],
)
def test_dates_as_pages_write_them(text, expected):
    got = netweir.parse_date(text, now=NOW)
    assert got == expected
    assert (got.tzinfo is None) == (expected.tzinfo is None)


def test_a_zone_makes_an_aware_datetime():
    plus_one = netweir.parse_date("2026-10-12T09:30:00+01:00")
    assert plus_one.utcoffset() == datetime.timedelta(hours=1)
    est = netweir.parse_date("12 Oct 2026 09:30 EST")
    assert est.utcoffset() == datetime.timedelta(hours=-5)
    assert est == datetime.datetime(2026, 10, 12, 14, 30, tzinfo=UTC)
    assert netweir.parse_date("2026-10-12T09:30:00Z").tzinfo is UTC
    assert netweir.parse_date("2026-10-12T09:30:00").tzinfo is None


def test_day_first_settles_which_number_is_the_day():
    assert netweir.parse_date("03/04/2026") == datetime.datetime(2026, 3, 4)
    assert netweir.parse_date("03/04/2026", day_first=True) == datetime.datetime(2026, 4, 3)
    # Over 12 can only be the day, whatever day_first says.
    assert netweir.parse_date("13/04/2026") == datetime.datetime(2026, 4, 13)
    assert netweir.parse_date("04/13/2026", day_first=True) == datetime.datetime(2026, 4, 13)


def test_two_digit_years_are_read_near_now():
    assert netweir.parse_date("10/12/26", now=NOW).year == 2026
    assert netweir.parse_date("10/12/99", now=NOW).year == 1999
    assert netweir.parse_date("10/12/99", now=datetime.datetime(2090, 1, 1)).year == 2099


def test_relative_dates_count_from_now():
    assert netweir.parse_date("3 days ago", now=NOW) == NOW - datetime.timedelta(days=3)
    assert netweir.parse_date("in 2 weeks", now=NOW) == NOW + datetime.timedelta(weeks=2)
    assert netweir.parse_date("2 hours, 5 minutes ago", now=NOW) == NOW - datetime.timedelta(
        hours=2, minutes=5
    )
    assert netweir.parse_date("an hour ago", now=NOW) == NOW - datetime.timedelta(hours=1)
    assert netweir.parse_date("now", now=NOW) == NOW
    assert netweir.parse_date("yesterday", now=NOW) == datetime.datetime(2026, 10, 11)
    assert netweir.parse_date("tomorrow", now=NOW) == datetime.datetime(2026, 10, 13)
    assert netweir.parse_date("today at 9:15 am", now=NOW) == datetime.datetime(2026, 10, 12, 9, 15)


def test_relative_dates_keep_the_zone_of_now():
    fetched = datetime.datetime(2026, 10, 12, 15, 0, tzinfo=UTC)
    got = netweir.parse_date("3 hours ago", now=fetched)
    assert got == datetime.datetime(2026, 10, 12, 12, 0, tzinfo=UTC)
    assert got.tzinfo is UTC
    assert netweir.parse_date("3 hours ago", now=NOW).tzinfo is None


def new_york():
    zoneinfo = pytest.importorskip("zoneinfo")
    try:
        return zoneinfo.ZoneInfo("America/New_York")
    except zoneinfo.ZoneInfoNotFoundError:
        pytest.skip("no time zone database")


def test_an_hour_ago_is_an_hour_across_a_clock_change():
    ny = new_york()
    # 8 March 2026: New York's clocks jump from 02:00 EST to 03:00 EDT.
    spring = datetime.datetime(2026, 3, 8, 3, 30, tzinfo=ny)
    hour_ago = netweir.parse_date("1 hour ago", now=spring)
    # Python subtracts datetimes in one zone on the clock face, so compare
    # instants.
    assert spring.timestamp() - hour_ago.timestamp() == 3600
    assert (hour_ago.hour, hour_ago.minute) == (1, 30)
    assert hour_ago.tzinfo is ny
    assert netweir.parse_date("90 minutes ago", now=spring).utcoffset() == datetime.timedelta(
        hours=-5
    )
    # 1 November 2026: back from 02:00 EDT to 01:00 EST.
    autumn = datetime.datetime(2026, 11, 1, 3, 0, tzinfo=ny)
    assert autumn.timestamp() - netweir.parse_date("3 hours ago", now=autumn).timestamp() == 10800
    assert netweir.parse_date("in 2 hours", now=spring).timestamp() - spring.timestamp() == 7200


def test_relative_dates_from_the_repeated_hour():
    ny = new_york()
    # 1 November 2026, 01:30 happens twice; fold=1 is the second, in EST.
    second = datetime.datetime(2026, 11, 1, 1, 30, fold=1, tzinfo=ny)
    assert second.utcoffset() == datetime.timedelta(hours=-5)
    got = netweir.parse_date("now", now=second)
    assert got.timestamp() == second.timestamp()
    for text, seconds in [("in 3 hours", 10800), ("1 hour ago", -3600), ("10 seconds ago", -10)]:
        got = netweir.parse_date(text, now=second)
        assert got.timestamp() - second.timestamp() == seconds, text


def test_days_ago_keep_the_clock_time_across_a_clock_change():
    ny = new_york()
    spring = datetime.datetime(2026, 3, 9, 9, 0, tzinfo=ny)
    day_ago = netweir.parse_date("1 day ago", now=spring)
    assert (day_ago.day, day_ago.hour) == (8, 9), "the same time yesterday, not 24 hours"
    two = netweir.parse_date("1 day, 2 hours ago", now=spring)
    assert (two.day, two.hour) == (8, 7)


def test_now_can_be_a_date():
    assert netweir.parse_date("3 days ago", now=datetime.date(2026, 10, 12)) == datetime.datetime(
        2026, 10, 9
    )
    assert netweir.parse_date("10/12/99", now=datetime.date(2090, 1, 1)).year == 2099
    with pytest.raises(TypeError, match="now= takes a datetime or a date"):
        netweir.parse_date("today", now="2026-10-12")


def test_now_defaults_to_the_current_time():
    before = datetime.datetime.now()
    got = netweir.parse_date("now")
    assert before <= got <= datetime.datetime.now()
    assert got.tzinfo is None


@pytest.mark.parametrize(
    "text",
    ["", "Free", "soon", "12 Oct", "2026-02-30", "31/04/2026", "Posted 12 Oct 2026", "3 days"],
)
def test_what_is_not_fully_understood_is_none(text):
    assert netweir.parse_date(text, now=NOW) is None


# --- data in scripts -----------------------------------------------------------

NEXT = """<html><head>
<script id="__NEXT_DATA__" type="application/json">
{"props": {"pageProps": {"product": {"id": 17, "name": "Desk lamp", "price": 19.99}}},
 "page": "/p/[id]", "buildId": "abc"}
</script>
<script>
  // Set up the store before the bundle loads.
  window.__INITIAL_STATE__ = {
    cart: {items: [], total: 0},
    user: {name: 'Ada', 'logged_in': true, last_seen: undefined,},
    /* prices in pence */
    prices: [1999, 2499, 0x10,],
  };
  var analytics = {id: "UA-1", sample: NaN};
</script>
<script type="text/template"><div>{{ not: data }}</div></script>
</head><body><p>Desk lamp</p></body></html>"""


def test_js_data_reads_what_json_does_not():
    text = "{name: 'Lamp', \"tags\": ['a', `b`], price: 12.5, sold: undefined, n: 0x1F,} // end"
    assert netweir.js_data(text) == {
        "name": "Lamp",
        "tags": ["a", "b"],
        "price": 12.5,
        "sold": None,
        "n": 31,
    }


def test_js_data_finds_the_first_literal_in_code():
    script = "var s = 'x'; window.__STATE__ = {a: [1, 2], b: {c: true}}; var t = [3];"
    assert netweir.js_data(script) == {"a": [1, 2], "b": {"c": True}}
    assert netweir.js_data(script, all=True) == [{"a": [1, 2], "b": {"c": True}}, [3]]
    assert netweir.js_data("no data here") is None
    assert netweir.js_data("no data here", all=True) == []


def test_js_data_types():
    got = netweir.js_data(
        "[1, 1.5, 'x', true, null, NaN, Infinity, 123456789012345678901234567890]"
    )
    assert [type(v) for v in got[:5]] == [int, float, str, bool, type(None)]
    assert got[5] != got[5], "NaN"
    assert got[6] == float("inf")
    assert got[7] == 123456789012345678901234567890


def test_page_js_data_by_name():
    p = page(NEXT)
    state = p.js_data("__INITIAL_STATE__")
    assert state == {
        "cart": {"items": [], "total": 0},
        "user": {"name": "Ada", "logged_in": True, "last_seen": None},
        "prices": [1999, 2499, 16],
    }
    next_data = p.js_data("__NEXT_DATA__")
    assert next_data["props"]["pageProps"]["product"]["name"] == "Desk lamp"
    analytics = p.js_data("analytics")
    assert analytics["id"] == "UA-1"
    assert math.isnan(analytics["sample"])
    assert p.js_data("pageProps") == {"product": {"id": 17, "name": "Desk lamp", "price": 19.99}}
    assert p.js_data("nowhere") is None


def test_page_js_data_without_a_name_lists_everything():
    found = page(NEXT).js_data()
    assert [type(v) for v in found] == [dict, dict, dict]
    assert found[0]["buildId"] == "abc", "a JSON script is read whole"
    assert found[1]["user"]["name"] == "Ada"
    assert found[2]["id"] == "UA-1"
    assert page("<p>no scripts</p>").js_data() == []


JSON_PARSE = r"""<script>
window.__S__ = JSON.parse('{"name": "caf\\u00e9", "sizes": [1, 2]}');
var t = JSON.parse("{\"tag\": \"new\"}");
var first = items[0], rest = items.slice(1)[0];
window["__CONFIG__"] = {debug: false};
</script>"""


def test_json_parse_payloads_are_decoded_before_reading():
    script = r"""window.__S__ = JSON.parse('{"name": "caf\\u00e9", "sizes": [1, 2]}');"""
    assert netweir.js_data(script) == {"name": "café", "sizes": [1, 2]}
    assert netweir.js_data(script, all=True) == [{"name": "café", "sizes": [1, 2]}]
    assert page(JSON_PARSE).js_data() == [
        {"name": "café", "sizes": [1, 2]},
        {"tag": "new"},
        {"debug": False},
    ]
    assert page(JSON_PARSE).js_data("__S__") == {"name": "café", "sizes": [1, 2]}


def test_index_expressions_are_not_data():
    assert netweir.js_data("var x = arr[0]; y = m[1][2]; z = f()[3];", all=True) == []
    assert netweir.js_data('window["__S__"] = {a: 1};') == {"a": 1}
    assert netweir.js_data("return [1, 2]") == [1, 2]
    minified = "var f=function(){return[{id:1},{id:2}]};for(x of[3,4])g(x[0]);"
    assert netweir.js_data(minified, all=True) == [[{"id": 1}, {"id": 2}], [3, 4]]


def test_parsed_html_has_js_data_too():
    root = netweir.parse(NEXT)
    assert root.js_data("__INITIAL_STATE__")["user"]["name"] == "Ada"
    assert len(root.css("head")[0].js_data()) == 3
    assert root.css("p")[0].js_data() == []


def test_hostile_script_text_finishes_fast():
    cases = [
        "[" * 1_000_000,
        "{a:" * 300_000,
        "[1," * 300_000,
        "{/*" * 300_000,
        "x=[" * 300_000,
        "['['" * 250_000,
        "[" + "0," * 500_000,
    ]
    for text in cases:
        started = time.monotonic()
        netweir.js_data(text)
        netweir.js_data(text, all=True)
        page(f"<script>{text}</script>").js_data("x")
        assert time.monotonic() - started < 10, text[:10]


def test_deep_nesting_is_cut_off_not_a_crash():
    deep = "[" * 400 + "]" * 400
    assert netweir.js_data(f"x = {deep}") is not None
    too_deep = "[" * 5000 + "]" * 5000
    # The innermost few hundred levels are a literal of their own.
    got = netweir.js_data(too_deep)
    assert isinstance(got, list)


def test_megabytes_of_ordinary_text_finish_fast():
    blob = "lorem ipsum dolor sit amet, " * 100_000
    started = time.monotonic()
    assert netweir.js_data(blob) is None
    assert netweir.parse_price(blob) is None
    assert netweir.parse_date(blob) is None
    assert time.monotonic() - started < 5


# --- in items ------------------------------------------------------------------

PRODUCT = """<html><body>
<h1>Desk lamp</h1>
<p class="price">Now only 1.234,50 €</p>
<time>12 Oct 2026</time>
<span class="updated">3 days ago</span>
</body></html>"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = (PRODUCT if self.path == "/lamp" else "<a class='p' href='/lamp'>lamp</a>").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


class Product(netweir.Item):
    name = netweir.css("h1::text")
    price = netweir.css(".price::text", into=netweir.parse_price)
    published = netweir.css("time::text", into=netweir.parse_date)
    missing = netweir.css(".nowhere::text", into=netweir.parse_price)


def test_helpers_convert_item_fields():
    item = Product.extract(page(PRODUCT))
    assert item == {
        "name": "Desk lamp",
        "price": netweir.Price(Decimal("1234.50"), "€"),
        "published": datetime.datetime(2026, 10, 12),
        "missing": None,
    }


def test_a_declarative_spider_exports_prices_and_dates(base, tmp_path):
    path = tmp_path / "items.jsonl"
    spider = type(
        "Products",
        (netweir.Spider,),
        {
            "start_urls": [f"{base}/"],
            "settings": netweir.Settings(
                throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False
            ),
            "rules": [netweir.Follow("a.p", extract=Product)],
            "pipelines": [export.jsonl(str(path))],
        },
    )()
    stats = spider.run()
    assert stats["items"] == 1
    item = json.loads(path.read_text(encoding="utf-8"))
    assert item == {
        "name": "Desk lamp",
        "price": {"amount": "1234.50", "currency": "€"},
        "published": "2026-10-12T00:00:00",
        "missing": None,
    }


def test_dots_are_day_first_unless_told_otherwise():
    assert netweir.parse_date("03.04.2026") == datetime.datetime(2026, 4, 3)
    assert netweir.parse_date("03/04/2026") == datetime.datetime(2026, 3, 4)
    assert netweir.parse_date("03.04.2026", day_first=False) == datetime.datetime(2026, 3, 4)
