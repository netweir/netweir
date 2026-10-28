# Security: what Netweir trusts, and why that holds

This is Netweir's security assurance case: what it treats as hostile, what
stands between that and your machine, and the limits it still has. To
report a problem, see [SECURITY.md](../../SECURITY.md).

## What's trusted and what isn't

| Input | Trusted? |
|---|---|
| Your spider, your settings, your machine | Yes. Netweir runs your code as you wrote it. |
| Pages, responses, headers, cookies, structured data and script text from the web | No. Every byte is hostile until parsed. |
| A page's JavaScript | No. It only ever runs inside Chrome's sandbox. |
| Proxies you configure | Partly. They see your traffic; TLS keeps them from reading or changing HTTPS. |
| Clients of `netweir serve` | No, unless they're on the loopback interface or present its password. |
| The Redis server behind a shared queue | Yes. It belongs on a network you control. |

## Hostile pages and responses

**Memory safety.** Parsing happens in Rust. `netweir-core` (fetching) and
`netweir-fingerprint` forbid `unsafe` code outright. The crates that need
it, for the document tree, the Python bindings and Chrome's pipes, keep it
to small blocks with a `SAFETY:` comment saying why each holds.

**Bounded work.** Pages built to be slow are part of the threat:

- The HTML parser refuses input over 512 MiB and takes a time budget; a
  crawl gives each page 10 seconds (`parse_timeout`). Its work grows with
  the page, not with its square: tests feed it 200,000 unclosed `<div>`s
  and misnested formatting tags, and its serializer is iterative, so deep
  trees don't exhaust the stack.
- XML nesting stops at 2,048 levels, and entities nest 16 deep at most.
- CSS selectors nest 32 deep at most; JavaScript literals, read for app
  state, 400.
- Every auto-extraction reader and `page.records()` have tests that time
  them on hostile layouts and fail if they slow down.

**No page code outside Chrome.** Netweir has no JavaScript engine. App
state in a page's scripts (`__NEXT_DATA__` and the like) is read by a
parser for object and array literals; nothing in it is executed.

**Fuzzing.** Eleven fuzz targets (the HTML parser, CSS, XPath, XML, JS
literals, Markdown, chunks, text, editing and auto-extraction) run every
night in CI for five minutes each.

**Response size.** A crawl reads at most 64 MiB of a response, counted
after decompression, so a small compressed answer that unpacks to
gigabytes is stopped too (`max_response_size`). A one-off
`netweir.get()` has no cap unless you pass `max_size`.

## The network

**TLS** is BoringSSL, through wreq and btls pinned to reviewed commits.
Certificates are verified by default, TLS 1.2 is the oldest version
offered, and `verify=False` is the only way to turn checking off. `verify`
also takes a CA bundle for a private certificate authority.

**Secrets in logs.** Proxy logins are masked (`http://***@host:port`) in
logs, stats and errors, a request's Basic-auth password prints as `***`,
a queue URL's password is hidden in messages, and a login in a URL is
dropped when a redirect leaves that URL. Tests check the proxy and queue
masking.

**Proxy logins stay out of checkpoints.** A checkpoint or shared queue
keeps a pending request's proxy without its login, and a resumed crawl
takes the login from its own proxy settings
([proxies](proxies.md#one-request-one-proxy)). Opening an older
checkpoint hides the logins already in it, and SQLite's `secure_delete`
overwrites the old values in the file. Tests read the checkpoint's files
for the password. The SHA-256 kept beside each proxy is unkeyed: it
hides a long random password, not one a guess would find. A request's
own Basic-auth login is still saved whole, in a file readable by its
owner only (`0600`); dropping its password is tracked in
[#183](https://github.com/netweir/netweir/issues/183).

## Chrome

Chrome's sandbox stays on: Netweir never passes `--no-sandbox` itself. On
Linux distributions that restrict user namespaces, the browser guide gives
the one system setting that keeps the sandbox working, and says what
`--no-sandbox` costs if you choose it anyway. On Windows, installing Chrome
grants its sandbox read access to the install folder, as Chrome's own
installer does. Downloads are named by Chrome with a random id, not the
name a page suggests, inside a temporary folder per context.

## `netweir serve`

It listens on `127.0.0.1` by default. Bound anywhere else, it refuses to
start without a password unless told `--insecure-no-auth`, and warns when
it is. Passwords are compared in constant time, browser requests (any
`Origin` header) are refused, and on loopback the `Host` header must name
a loopback address, which blocks DNS rebinding. Request bodies stop at
1 MiB, each request has 15 seconds, and connections are limited per
address and in total. The metrics endpoint has no password and also
defaults to `127.0.0.1`.

## `netweir.llm`

Nothing calls a model unless the user calls `netweir.llm.extract`, with
their own key. The page's Markdown, URL and title go to the model as data,
and the system prompt says that text on the page addressing the model is
part of the page. The API holds the answer to the schema's fields and types,
and a pydantic model's answer is validated, so a hostile page can mislead
the values but not which fields come back. The page's Markdown can't close
its own `<page>` delimiter, and the user's instructions go in a block of
their own after it.

## Files Netweir writes

The files and images pipelines name each file by a SHA-1 of its URL under
the store's root, so a hostile URL can't choose a path. Checkpoints and
caches are created owner-readable only.

## Third parties

A crawl obeys robots.txt and TDMRep unless you turn that off, and Netweir
logs when you have. It keeps 8 requests in flight per site, starts at a
one-second delay, and backs off as a site slows down, up to a minute.

## The supply chain

- `cargo deny` allows only listed licences, only crates.io and the two
  pinned git repositories as sources, and checks RustSec advisories.
- Dependabot proposes updates to Rust, Python and GitHub Actions
  dependencies weekly. The TLS and HTTP/2 crates are updated by hand,
  because a change there changes the fingerprint.
- Every GitHub Action is pinned to a commit hash, and workflow tokens get
  the least permission each job needs.
- Releases are built in GitHub Actions, published to PyPI by trusted
  publishing (no token is stored anywhere) and carry build provenance you
  can check with `gh attestation verify`.
- OSV-Scanner's view of the lockfile, with the advisories that don't reach
  Netweir and why, is in [`osv-scanner.toml`](../../osv-scanner.toml).

## Known limits

- One-off fetches have no response size cap by default.
- The HTML tree has no depth limit. Deep trees are handled without
  recursion, and the time budget bounds the rest.
- `unsafe` remains in the DOM, Python binding, HTML and browser crates.
- Checkpoints and shared queues keep a request's Basic-auth password,
  protected by file permissions only (#183).
