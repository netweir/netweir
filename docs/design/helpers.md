# Prices, dates and the data in a page's scripts

Three things turn up on nearly every scraping job, and each costs a
regex that works on the first site and breaks on the second. A price:
`£1,234.50`, `1.234,50 €`, `$ 12`, `12,99 zł`, `From USD 1,099`. A date:
`2026-10-12T09:30:00Z`, `12 Oct 2026`, `October 12th, 2026`,
`12/10/2026`, `3 days ago`, `Mon, 12 Oct 2026 09:30:00 GMT`. And data a
page keeps in a script rather than its HTML: `window.__INITIAL_STATE__ =
{products: [...], 'total': 42,}` — JavaScript, not JSON, so `json.loads`
fails on the unquoted keys, single quotes and trailing comma. Scrapy
users reach for price-parser, dateparser and chompjs. netweir does all
three in Rust.

## Prices

`netweir.parse_price(text)` gives a `Price(amount, currency)`, or None
when there's no number in the text.

- `amount` is a `Decimal`, never a float: a price is money.
- `currency` is the symbol or code as written (`"£"`, `"€"`, `"USD"`,
  `"zł"`), or None. `Price.code` maps the unambiguous symbols to ISO
  codes (`£` → GBP, `€` → EUR, `¥` stays ambiguous between JPY and CNY,
  `$` is ambiguous and stays None, `US$` → USD).
- The decimal separator is worked out from the text: the last of `.` and
  `,` is the decimal point if one or two digits follow it, or if the
  other also appears before it; three digits after a lone separator make
  it a thousands separator (`1,234` is 1234, `1.234` is 1234, `12,99` is
  12.99, `1.234,56` is 1234.56). Spaces, thin spaces and apostrophes
  (`1'234.50`) group thousands too.
- The first number in the text is the price (`From £10 to £20` is 10).
  `Free` and similar words aren't a price: None.

## Dates

`netweir.parse_date(text, *, day_first=False, now=None)` gives a
`datetime`, or None when it isn't a date it knows.

- ISO 8601 and RFC 3339, RFC 2822 (`Mon, 12 Oct 2026 09:30:00 GMT`),
  and HTTP dates.
- Numbers: `12/10/2026`, `12-10-2026`, `12.10.2026`, `2026/10/12`, with
  two-digit years read as 19xx or 20xx, whichever is nearer `now`. When
  both the first and second numbers could be the day, `day_first` says
  which (US order by default, as dateparser's English is); a first number
  over 12 is a day whatever `day_first` says.
- Month names, full or short, with or without a weekday and ordinals:
  `12 Oct 2026`, `October 12th, 2026`, `Oct. 12, 2026`. In English,
  French, German, Spanish, Italian, Portuguese and Dutch.
- Times: `09:30`, `9:30:15`, `9.30pm`, `9:30 PM`, with a zone if there's
  one (`Z`, `+01:00`, `UTC`, `GMT`, and the common US and European
  abbreviations).
- Relative, in English: `now`, `today`, `yesterday`, `tomorrow`,
  `3 days ago`, `an hour ago`, `in 2 weeks`, `2 hours, 5 minutes ago`,
  counted from `now` (the current time by default; pass the time the
  page was fetched to read it as the page meant).

A date with a zone gives an aware datetime; one without gives a naive
one; a date without a time is midnight. Anything that isn't fully
understood is None rather than a guess: a wrong date is worse than none.

## Data in scripts

`netweir.js_data(text)` finds the first JavaScript object or array
literal in `text` and gives it as Python: dicts, lists, str, int, float,
bool, None. It reads what JSON doesn't allow: unquoted and single-quoted
keys, single-quoted and backtick strings (a template with `${...}` is
kept as written), trailing commas, comments, `undefined` (None), `NaN`
and `Infinity`, hexadecimal numbers, and `new Date(...)` (its argument).
`js_data(text, all=True)` gives every one in the text, in order.

`page.js_data(name=None)` reads the page's `<script>` elements: with a
`name`, the value assigned to it (`window.__STATE__ = {...}`,
`var data = [...]`, `"name": {...}`), or None; without, every object and
array in every script, in order. A `<script type="application/json">` is
read as it is.

## In items

`into=netweir.parse_price` and `into=netweir.parse_date` work in an
`Item`'s fields like any other function. A `Price` exports as
`{"amount": "1234.50", "currency": "£"}`; a datetime as ISO 8601.
