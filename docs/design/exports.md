# More ways out: JSON, XML, cloud storage and batches

A crawl's items leave as JSON Lines, CSV or Parquet, written to a local
file. Scrapy's feed exports go further, and people moving over use the
rest: a single JSON array for a tool that won't read JSON Lines, XML for
an older system, a bucket instead of a disk, and a new file every so many
items so a long crawl doesn't grow one huge file. This adds those four.

## JSON

`netweir.export.json(path)`, and `.json` for `-o`, writes one JSON array:

```
[
{"title": "A Light in the Attic", "price": "£51.77"},
{"title": "Tipping the Velvet", "price": "£53.74"}
]
```

One item per line, as Scrapy writes it, so a file is still readable a line
at a time by eye and by `grep`. Serialising is the JSON Lines writer's: the
same dates, decimals and big integers. A file closed with no items holds
an empty array.

The array is only valid once closed. A crawl resumed from a checkpoint
cuts the file back to its recorded length, as it does a JSON Lines file;
that length may be with or without the closing `]` (recorded mid-crawl,
or after the file was closed), so the writer, opened to add to a file,
takes a closing `]` off the end if there is one, and goes on after the
last item with a comma, or with the first item if there was none. The
closing `]` is on a line of its own, which no item's last line is, so an
item that ends with `]` (a list) isn't taken for it. A file that doesn't
start with `[` isn't one this writer made, and adding to it is refused.

## XML

`netweir.export.xml(path, root="items", item="item")`, and `.xml` for
`-o`, writes Scrapy's layout:

```
<?xml version="1.0" encoding="utf-8"?>
<items>
<item><title>A Light in the Attic</title><price>£51.77</price></item>
</items>
```

- A dict's keys become elements in order; a list's values become
  `<value>` elements, as Scrapy's do; `None` is an empty element.
- Text is escaped, and characters XML 1.0 doesn't allow (control
  characters other than tab, newline and carriage return) are dropped.
- `true` and `false` are written lowercase, as XML Schema writes
  booleans. (Scrapy writes Python's `True` and `False`.)
- A key that isn't a valid XML name has each character that can't be in
  one replaced with `_`, and `_` put in front if it starts with something
  a name can't start with: `"unit price"` becomes `<unit_price>`. "Valid"
  is XML 1.0's fourth edition: the fifth allows many more characters, but
  expat (Python's ElementTree, among others) keeps to the fourth, and a
  name it rejects makes the whole file unreadable to it. The fourth
  edition's characters beyond ASCII are a table in the writer, read off
  expat character by character; a test checks a key made of every
  character of the Basic Multilingual Plane parses.
- `root` and `item` must differ: the file's closing tag would otherwise be
  an item's.

Resuming works as for JSON: the writer takes a closing `</items>` (the
root given) off the end and goes on, and refuses a file that doesn't
start with this root's opening.

Before a resumed crawl cuts an output file back to its recorded length, it
checks the file is still the one it wrote: at least that long (cutting a
shorter file "back" would pad it with zero bytes), and, for JSON and XML,
starting as the format starts. A file changed since is refused and left as
it is.

## Cloud storage

A path with a scheme other than `file://`, such as `s3://bucket/books.jsonl`
or `gs://bucket/books.csv`, is written through fsspec, which has to be
installed with the filesystem's own package (`s3fs`, `gcsfs`, ...). The
file is written to a temporary local file as usual, in Rust, and uploaded
when it's closed, so a crawl's speed doesn't depend on the bucket's.

Making the exporter resolves the URL through fsspec, so a missing fsspec,
a missing `s3fs` or a scheme nobody knows fails there, before the crawl,
not when the upload is tried at the end. An upload that fails keeps the
local file and names it in the error, so a crawl's output is never lost
to a bucket that said no; the crawl's other exporters and pipelines are
still closed, and the first such error is raised once the crawl is wound
up.

A crawl with a checkpoint can't write to cloud storage: resuming cuts the
output file back to its recorded length and adds to it, which an object in
a bucket can't do. That's refused when the crawl starts, with a message
saying to write locally and upload after. Parquet uploads the finished file
once the crawl closes it.

## Batches

`batch=N` on any exporter starts a new file every `N` items. The path
names where each goes, with `{batch}` (1, 2, 3 ...) and, if wanted,
`{time}`, the UTC time the batch started (`2026-10-12T03-04-05Z`, safe in
file names on every system). `{batch}` is required: two batches can start
in the same second, so `{time}` alone could give two the same name. Other
braces in the path are written twice (`{{` and `}}`), as in Python's
`format`; a path with any other placeholder is refused when the exporter
is made:

```python
netweir.export.jsonl("books-{batch}.jsonl", batch=1000)
```

`-o books-{batch}.jsonl --batch 1000` does the same from the command line.
A batch is finished (and uploaded, for cloud
storage) when it's full, so files appear as the crawl goes; the last is
finished when the crawl ends. A crawl with no items still writes the first
batch, empty, as a crawl without batches writes an empty file.

Each run numbers its batches from 1, so a shorter run leaves a longer
earlier run's later files where they were; `{time}` keeps runs apart.

Batches and checkpoints don't mix in this version: resuming would need to
know which batch was being written and how full it was.
That's refused when the crawl starts.
