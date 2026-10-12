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

`netweir.parse_date(text, *, day_first=None, now=None)` gives a
`datetime`, or None when it isn't a date it knows.

- ISO 8601 and RFC 3339, RFC 2822 (`Mon, 12 Oct 2026 09:30:00 GMT`),
  and HTTP dates.
- Numbers: `12/10/2026`, `12-10-2026`, `12.10.2026`, `2026/10/12`, with
  two-digit years read as 19xx or 20xx, whichever is nearer `now`. When
  both the first and second numbers could be the day, `day_first` says
  which; left as None, dots mean day first (as everywhere that writes
  them) and slashes or dashes month first (as US English, and dateparser's
  English, read them). A first number over 12 is a day whatever
  `day_first` says.
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

## As built

The three readers live in netweir-dom, as `price`, `date` and `js`
modules beside `metadata`. They're text parsers rather than DOM code, but
netweir-dom is where reading what a page says already lives, it has no
network dependencies, and its fuzz harness covers them (`js` for script
text, `text` for prices and dates). netweir-py only converts: `Price` is a
frozen dataclass in `netweir/_helpers.py`, so the exporters' dataclass and
`Decimal` handling give `{"amount": "1234.50", "currency": "£"}` with no
special case, and `parse_date` builds its `datetime` in Python from the
fields Rust returns. No new crates.

Decisions the outline above left open:

- Prices: three digits after a lone separator are a thousands group only
  when the grouping is well formed: a leading group of one to three
  digits that isn't zero, then groups of three. `0.500` (Bahraini dinar
  have three decimals) and `12345.678` have no thousands reading, so they
  keep the decimal one. Codes are recognised in capitals from a list of
  57 common ISO 4217 codes; symbols from a table that also gives `code`
  (`kr`, `Rs`, `$` and `¥` stay None). With `,`, Indian grouping (a lead
  of one or two digits, groups of two, a final group of three) is
  thousands grouping too.
- Prices keep their sign: a `-` or `−` straight before the number,
  before a currency straight before it, or between the currency and the
  number (`£-5`, `€ -5`), makes the amount negative. Before the number
  or the currency it counts only at the start or after whitespace, `(`
  or `[`: not a hyphen joined to a word (`2-for-$5`), nor the `:-` that
  labels Indian prices (`MRP:-₹499`). A leading point is a
  decimal (`$.99`) unless a letter comes before it (`Rs.499`). The `,-`
  and `.–` endings for whole amounts are skipped before looking for a
  currency after the number. Full-width digits, `，`, `．` and `－`
  are read as ASCII, and U+3000 (ideographic space) groups thousands.
- Dates: zone names are UTC, UT, GMT, Z, EST, EDT, CST, CDT, MST, MDT,
  PST, PDT, AKST, AKDT, HST, WET, WEST, BST, CET, CEST, MET, MEST, MEZ,
  MESZ, EET, EEST, MSK, JST, KST, HKT, SGT, PHT, AWST, ACST, ACDT, AEST,
  AEDT, NZST and NZDT. CST is US Central as RFC 2822 has it (China
  Standard Time loses); IST is left out (India, Ireland or Israel).
  `UTC+1` and `GMT-05:00` work, and a repeated zone that agrees
  (`-0400 (EDT)`) is fine; two that disagree are None, including a lone
  zone name in brackets after an offset (`-05:00 (CST)`). Once an offset
  is read, a trailing bracket with anything other than one zone name in
  it is a comment, which is how `Date.toString()` ends. An offset with no
  time is read only as the last thing and after a space
  (`12 Oct 2026 +0100`), so a date's own dashes are never taken for one.
- German `am` ("am 12. Oktober") is accepted only before the date's
  first part, never after a date or time, and a time reads its own am/pm
  first, so `9 am` is still a time and `12 Oct 2026 am` is None.
  `um 9 Uhr` is a time without minutes, and dotted dates may have spaces
  after their dots (`12. 10. 2026`), as German writes them.
- Relative dates: hours, minutes and seconds are elapsed time and days,
  weeks, months and years are on the calendar. Rust returns the calendar
  part as fields and the elapsed part as seconds, and Python adds those
  in UTC, so `1 hour ago` is an hour across a change to summer time while
  `1 day ago` is the same clock time yesterday. A `date` given as `now` is
  its midnight; anything else that isn't a datetime is a TypeError. When
  the calendar part is nothing (`now`, `3 hours ago`), Python starts from
  `now` itself, so its `fold` (which of a repeated hour's two 01:30s it
  is) is kept.
- When only one reading of a numeric date is real, it's taken whatever
  `day_first` says, both ways round (`04/13/2026` with `day_first=True`
  is 13 April). Dotted dates (`12.10.2026`) are day first unless
  `day_first=False` says otherwise: they're nearly always day first in
  practice, and the outline's US default for them read real dates wrongly.
- A date without a year (`12 Oct`) is None: filling in this year would
  be a guess. So is a date with unknown words around it (`Posted 12 Oct
  2026`); filler words that join a date's parts (`the`, `of`, `de`, `à`,
  `um`, `Uhr`, `om`, `alle`, `às` and a few more) are allowed.
- `mar` is March, or Tuesday in French, Spanish and Italian. It's read
  as March unless another month name is there.
- `today`, `yesterday` and `tomorrow` are midnight unless a time is given
  (`yesterday at 9:30`), and take `now`'s zone like other relative dates.
- Text over 200 characters isn't a date.
- Script data: empty `{}` and `[]` are skipped when searching, as in code
  they're nearly always function bodies. Beyond the outline it reads
  minified `!0`, `!1` and `void 0`, and `JSON.parse('...')`, whose
  string is decoded before it's read (also when searching without a
  name, where the brackets inside the string aren't tried again). `new
  Date` with several arguments gives them as a list. Integers past 64 bits
  keep every digit. A `[` straight after an identifier character, `]` or
  `)` is indexing and isn't tried, unless the identifier is one of the
  keywords minifiers write against an array (`return`, `case`, `of`,
  `in`, `typeof`, `void`, `else`, `do`, `yield`, `await`, `delete`,
  `throw`) and isn't a property (`obj.in[1]`). A call counts as a level
  of nesting.
- Not read, by design: Nuxt 2's function-wrapped state, shorthand,
  spread, functions, regex literals, BigInt and numeric separators. The
  literal holding one fails; literals nested inside it can still be found
  on their own by a search without a name.
- `page.js_data(name)` also takes a JSON script whose id is `name`, which
  is how Next.js ships `__NEXT_DATA__`. A script type ending `+json`
  counts as JSON, and templates are skipped. `Node.js_data` is the same
  on a parsed document or any element.
- Hostile script text: nesting stops at 400 levels; a literal that failed
  is never tried again from the same byte; and failed attempts together
  may read at most 16 times the text plus a megabyte, after which the
  search stops. Known limit: text built to exhaust that budget can hide
  literals after it.
