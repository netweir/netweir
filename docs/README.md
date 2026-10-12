# netweir documentation

- [Getting started](getting-started.md): install, fetch a page, write and run a spider.
- [Selecting](selecting.md): CSS, XPath and Beautiful Soup style queries.
- [Trying a page](shell.md): the page netweir gets, on stdout, in your browser or at a Python prompt.
- [Forms, logins and APIs](forms.md): POST and other methods, form submission, JSON APIs.
- [Crawling](crawling.md): spiders, requests, pipelines, middlewares, signals, exporters and the command line.
- [Declarative spiders](declarative.md): items and rules that run in Rust.
- [Self-healing](self-healing.md): blocks, checkpoints, tracked selectors and repair proposals.
- [Browser](browser.md): drive Chrome for pages that need JavaScript.
- [Settings](settings.md): every setting and its default.

The design behind it all is in [design/v0.1.md](design/v0.1.md), the
browser driver's in [design/browser.md](design/browser.md), requests
other than GETs in [design/requests.md](design/requests.md), the
development cache in [design/cache.md](design/cache.md), and the fetch,
view and shell commands in [design/shell.md](design/shell.md), and
middlewares, signals and pipeline lifecycles in
[design/hooks.md](design/hooks.md), and the JSON, XML, cloud and batch
exporters in [design/exports.md](design/exports.md).
