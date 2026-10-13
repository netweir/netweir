# Validating items and checking a crawl

A crawl that finishes isn't a crawl that worked. A site's redesign can
leave every price empty, a block can cut a crawl to a tenth of its pages,
and a callback can fail on half of them, and the crawl still ends with
"finished". People running spiders on a schedule add Spidermon to Scrapy
for this: validate each item against a schema, and check the run as a
whole at the end. Netweir does both itself.

## Validating items

`netweir.validate(schema)` is a pipeline stage. Each item is checked
against `schema`, which is either:

- a JSON Schema, as a dict. This needs the `jsonschema` package
  (`pip install jsonschema`), as Spidermon's does; making the stage
  without it says what to install. Any draft the package supports, by
  `$schema`, or its latest by default.
- a dataclass. A field without a default must be present, and not None
  unless its type admits None (`X | None`, `Any`);
  a value must be of the field's annotated type, for the types items hold:
  `str`, `int`, `float` (an int is a float too), `bool`, `list`, `dict`,
  `X | None`, and `list[X]`/`dict[str, X]`, whose contents are checked
  too. Any other annotation isn't checked. No package needed.

An item that fails is dropped by default and counted as `items_invalid`
(and not as `items_dropped`, so the two say different things), and
`item_dropped` listeners are told, with no exception. `items_validated`
and `items_invalid` count an item once however many validate stages it
meets. The `_id` a checkpoint gives an item, and `_errors`, are left out
of what's validated. With
`drop=False` it's kept, and a dict item gets its errors as a list of
strings in an `_errors` field (a dataclass item has no room for one), for
a crawl that would rather keep everything and sort it out later. Either
way, each error is counted against its field in `spider.invalid_fields`
(`{"price": 12, "title": 1}`), and the first error for each field is
logged with the item's page.

Errors name the field by its path (`price`, `offers.0.price`) and say what
was wrong in a few words: `price: required`, `stock: expected int, got
str`, or the message jsonschema gives. In `invalid_fields` a list position
is `*`, so an item with fifty offers missing a price is one field, not
fifty; a numeric dict key is `*` too. Each field's annotation is resolved
on its own, so one that can't be (a type imported only for type checkers,
or defined inside a function) leaves the others checked; the stage logs
which once, when it's made.

## Checking the crawl

`Spider.checks` is a list of checks that run once the crawl has ended,
whether it finished or a limit stopped it; not when it ended with an
exception, which is its own failure. A check is a `netweir.Checks` or any
callable taking `(stats, spider)`, sync or async (an awaitable result is
awaited). It passes by returning None or True (so a predicate works), and fails by returning False, a message, messages
(each made a string), or anything else, which is named in the failure. A
crawl that finished is never turned into an error by its checks.
`checks` may be one check rather than a list.

Checks run after the exporters are closed, so a check can read the output
files, and before `closed()` and `spider_closed`.

`netweir.Checks` holds the common ones, each off unless given:

- `min_items`: at least this many items.
- `max_error_rate`: at most this fraction of requests going wrong, where
  wrong is a failed fetch or a callback error, out of pages fetched plus
  failed fetches.
- `max_invalid`: at most this fraction of items failing validation, out of
  the items a validate stage checked (`items_validated` in the stats).
- `coverage`: `{field: fraction}`, each field present and not empty
  (None, "", [] and {} are empty) in at least that fraction of items.
- `finish_reasons`: the crawl ended for one of these reasons; `("finished",)`
  says a limit stopping it is a failure.

The stats are the run's: a resumed crawl's checks see only what the
resumed run did.

Field coverage is counted for every crawl, since it costs little: in
`spider.coverage`, `{field: items with it}`, beside `stats["items"]`.

The results are in `spider.check_failures`, a list of messages, empty when
every check passed, and the count is `stats["checks_failed"]`. Each
failure is logged as a warning, and a crawl whose checks all passed says
so at info. They're there before `closed()` and `spider_closed` run, so a
signal handler can send them on (an email, a chat message): that's left
to the handler, not built in. `netweir crawl` exits with status 1 when a
check failed, so a scheduler or CI job sees it.

A check that raises counts as failed, with the exception in its message:
a check that can't tell isn't a pass.

## Not in this version

Spidermon's actions (email, Slack, Sentry) and its HTML reports: a
`closed()` method or a `spider_closed` handler does the sending, with
`check_failures` in hand. Periodic checks while a crawl runs.
