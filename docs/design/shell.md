# fetch, view and shell

Writing a spider is mostly asking one question over and over: what does
the page Netweir gets look like, and does my selector find the thing on
it? A browser's devtools answer that for the page the browser got, and
that's often not the page Netweir gets. A site can send a different page
to a client it doesn't trust, and much of what a browser shows is built by
JavaScript that Netweir doesn't run. Three commands answer the question
with Netweir's own fetch:

```
netweir fetch URL           # the body, on stdout
netweir view URL            # the page netweir got, opened in your browser
netweir shell URL           # a Python prompt with the page in `page`
```

Scrapy has the same three, and people who know them will look for them.

## Fetching

All three fetch the way `netweir.get` does, through one `Fetcher` (the
native client under `netweir.get` and `Client`), so the shell's pages
share cookies:

- `--profile NAME` picks the browser to look like (default `chrome`).
- `--proxy URL` and `--timeout SECONDS` are passed through.
- `--header "Name: value"` (repeatable) adds a request header.

A block isn't an error here: the point is to see what came back. The page
is shown as usual, and a line on stderr says what blocked it, so a block
never looks like the real page.

`fetch` and `view` also take `--browser`, which loads the page in Chrome,
the way `browser="always"` would in a crawl, and uses the HTML Chrome ended
up with after its scripts ran. That page's `version` is `"browser"`, as
in a crawl, and `view` declares its HTML UTF-8, since that's how it was
saved. `--profile` and `--header` don't apply to Chrome and are refused
with it.

`shell` doesn't take `--browser` in this version: the prompt is
synchronous and the driver is async. A shell with Chrome needs the
driver run on a thread of its own, and that's left for later.

## fetch

`netweir fetch URL` writes the body to stdout as bytes, unchanged, so
`netweir fetch URL > page.html` saves exactly what came back.
`--headers` writes the status line and the response headers instead, in
the order the server sent them.

## view

`netweir view URL` saves the page to a temporary `.html` file and opens it
with the system's browser (Python's `webbrowser`). The file is the page's
decoded text written as UTF-8 behind a byte order mark, which outranks
every other charset signal, with `<meta charset="utf-8">` and a
`<base href>` put first in `<head>`; a page without `<head>` gets them
after its doctype and `<html>` tag, so it isn't pushed into quirks mode.
The first of each wins, which is why they go first:

- The charset the server sent in its Content-Type header doesn't travel
  with a file, and a page's own `<meta charset>` may disagree with the
  UTF-8 the file is written in.
- A page's own `<base href="/">` (Angular's default) would mean the root
  of the disk once the page is a file. The `<base>` written is the page's
  own base resolved against its URL, so links resolve as they did on the
  site, and a `<base target>` of the page's still applies.

The path is printed, so it can be opened again or kept. Scripts in the
saved copy run in your browser, as they would in Scrapy's `view`; the file
is the page as fetched, not a sanitised copy.

## Targets and exit status

All three take a URL, a path to a file or a `file:` URL, or a bare host
(`example.com/books`, `localhost:8000`, `127.0.0.1`), which gets
`https://` as Scrapy's shell would give it a scheme. A name ending in a
file's extension (`page.html`, `notes.txt`) is a file that isn't there,
not a host, and is refused as one. A file is read as a page fetched from its `file:` URL; with
`--browser`, Chrome loads that URL instead.

`fetch` and `view` exit 0 for a page (a 404 is still a page), 1 when the
fetch failed or the page was a block, and 2 for a mistake on the command
line, including a URL, header or proxy the client refuses to use. Ctrl-C
ends a command with 130, and at the shell's prompt cancels the running
fetch and keeps the shell: the native client used to block until the server
answered, and now stops every tenth of a second to let Python handle
signals, which also makes `netweir.get` interruptible. A reader that
closes the pipe early (`| head`) isn't an error.

## shell

`netweir shell URL` fetches the page and opens a Python prompt with:

- `page`: the page, a `netweir.Page`
- `fetch(url, method="GET", ...)`: fetch another page with the same
  session (and its cookies) and make it `page`; it takes `form=`,
  `json=`, `body=`, `headers=` and `referer=`, as `netweir.request` does
- `submit(form)`: submit a form read with `page.form()`, and make the
  answer `page`
- `view(page=None)`: `netweir view` for `page`, or the one given
- `netweir`, the module

IPython is used when it's installed, for completion and history, and
Python's own `code.interact` when it isn't. `netweir shell` without a URL
starts with `page = None`.

A path to a local file (`netweir shell page.html`) reads the file instead
of fetching. `page` is a `netweir.Page` made as if the file had been
fetched from its `file:` URL: status 200, `version` `"file"`, no headers.
That way it decodes, resolves links and offers `metadata()` exactly as a
fetched page does.

`-c CODE` evaluates one expression against the page, prints its result
as `print()` does (as Scrapy's `shell -c` does) and exits, so a selector can be tried from a script or a test:

```
netweir shell https://quotes.toscrape.com/ -c 'page.css("span.text::text").getall()'
```

The shell's fetches are what a crawl would send, but the shell isn't a
crawl: no robots.txt, throttling or duplicate filter, and no cache. Using
the crawl's cache from the shell is a follow-up: the cache lives in the
crawl engine, and a `Client` doesn't have one yet.
