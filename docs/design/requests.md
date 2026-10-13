# Requests that aren't GETs

Netweir sends GETs and nothing else. A search box, a login, a "load more"
button and most JSON APIs need a POST, so today none of them can be
scraped. This adds request methods and bodies to every layer, and form
submission on top, without giving up what Netweir is for: a request has to
look like the browser its profile names, and a POST looks different from a
GET.

## Two kinds of POST

A browser sends a POST in one of two ways, and they differ on the wire:

- **A form submission** is a navigation. Chrome sends `Cache-Control:
  max-age=0`, `Origin`, `Content-Type`, `Sec-Fetch-Site: same-origin` (or
  whatever the form's page is to the target), `Sec-Fetch-User: ?1`,
  `Sec-Fetch-Mode: navigate`, `Sec-Fetch-Dest: document` and a `Referer`,
  in an order of its own.
- **A script's request** (`fetch()` or XMLHttpRequest) is not a navigation.
  It sends `Accept: */*`, `Sec-Fetch-Mode: cors`, `Sec-Fetch-Dest: empty`,
  `Origin`, a `Referer`, `Content-Type`, and no client hints that a
  navigation would carry, again in its own order.

Both are captured from each browser, as the GET navigations were, and each
profile gains a header list for each. A test replays both and fails on any
difference, as for GETs.

## The capture

The capture server gains two pages and records two more things:

- `/form` serves a page whose script submits a form, `POST` to
  `/submitted`, `application/x-www-form-urlencoded`, two fields.
- `/fetch` serves a page whose script POSTs JSON to `/api` with `fetch()`.
- Each capture records the request method and the body, so a test can
  check Netweir sent the same bytes.

The scenario adds, after the four navigations: open `/form` (the script
submits), then open `/fetch` (the script posts). Chrome and Firefox are
driven as before; Safari needs a person at the Mac, as before.

## Redirects

As browsers do: a 301 or 302 answer to a POST, and any 303, turns into a
GET with no body and no `Content-Type` or `Origin`; 307 and 308 repeat the
method and the body. The header order after a redirect is the profile's
`redirect_header_order`, as now.

## API

```python
netweir.post(url, form={"q": "rust"})          # a form submission
netweir.post(url, json={"query": "..."})       # a script's request
netweir.post(url, body=b"...", headers={"Content-Type": "text/plain"})
netweir.request("PUT", url, json=...)          # any method

client.post(...), client.request(...)          # the same on a Client

Request(url, method="POST", form={...})        # in a crawl
Request.from_form(page, data={"q": "rust"})    # submit a form on a page
```

Exactly one of `form`, `json` and `body` may be given. `form` makes a form
submission (navigation headers, URL-encoded); `json` and `body` make a
script's request. `referer=` names the page the request comes from; it
sets `Referer`, `Origin` and `Sec-Fetch-Site` as the browser would. Without
it, a form submission comes from the target's own origin root and a
script's request from the target's origin.

`Request.from_form(page, ...)` works like Scrapy's `FormRequest.from_response`:
it picks the form (`form=` as a CSS selector, an XPath, an index, or by
`formid`/`formname`; the first form otherwise), collects its successful
controls as the HTML standard defines them (named, not disabled; checked
checkboxes and radios; a select's selected options or its first; textareas),
lets `data` override or add fields, includes the submit button named by
`click=` (or the first submit button unless `click=False`), and takes the
method, action and encoding from the form. A GET form becomes a GET with
the fields in the query string. `multipart/form-data` forms are encoded as
multipart with text fields; file uploads aren't supported yet.

## The crawl

- The duplicate filter's fingerprint includes the method and the body, so
  two searches for different terms are two requests.
- The checkpoint stores the method and the body. The format number goes
  up; an older checkpoint is upgraded with every request a GET.
- A request that isn't a GET is never taken over by Chrome when blocked:
  replaying a POST in the browser could submit it twice. A blocked POST
  goes to `on_block` (and its errback) instead, with a log line saying why.
- Retries repeat a POST only for answers that say it wasn't processed
  (429, 503 with `Retry-After`) and for connection failures before any
  response; a 500 to a POST is not retried by default, since the server may
  have acted on it. `retry_post=True` on the request retries it like a GET.
