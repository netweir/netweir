# Contributing to Netweir

Thanks for wanting to help. Bug reports, a site Netweir can't get past, a
browser that's shipped a new version, a fix, a faster loop: all of it is
welcome, and most of it starts with an issue.

## Before you write code

Open an issue first for anything bigger than a small fix, so we can agree
on the shape before you spend an evening on it. For questions, use
[Discussions](https://github.com/netweir/netweir/discussions) rather than an
issue.

If an issue is labelled `good first issue`, it's small, self-contained and
I'll help you through it.

## Getting it running

You need Rust, a C compiler, CMake and [uv](https://docs.astral.sh/uv/).
Chrome is optional; without it the browser tests skip themselves.

```
git clone --recurse-submodules https://github.com/netweir/netweir
cd netweir
uv sync --group dev
uv run maturin develop --uv
```

After you change any Rust, run `uv run maturin develop --uv` again before
testing from Python.

## Checking your change

These are what CI runs, so running them first saves a round trip:

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --exclude netweir-py
uvx ruff check .
uvx ruff format .
uv run pytest
```

`netweir-py` is left out of `cargo test` because it's a Python extension
and only links inside Python; `pytest` covers it.

A bug fix comes with the test that would have caught it. A new feature
comes with tests and, if you can use it from Python, a line in
[docs/](docs/README.md).

## The one hard rule: write it yourself

Netweir is clean-room. Please don't copy code, or translate it line by line,
from another scraping library or browser automation tool, however
permissive its licence. Depending on a published crate or package without
modifying it is fine; vendoring or pasting its source isn't. If you learnt
how something works from reading another project, say so in the pull
request and write your own version from the idea, not the text.

Browser profiles are the same: they come from captures of the real browser,
made with `netweir-fingerprint`'s capture tool, never from another
library's tables. The capture goes in `profiles/` beside the profile.

## Commits and pull requests

Commit subjects take a conventional prefix and describe the problem rather
than the action, in 72 characters or fewer:

```
fix(fetch): a redirect to another port lost the session's cookies
feat(profiles): requests could look like Chrome or Firefox, never Safari
```

The body, wrapped at 72 columns, says what was wrong and what changed.

Keep a pull request to one change. The template will ask what it fixes and
how you tested it.

## The contributor licence agreement

Netweir is AGPL-3.0, and it's also offered under a commercial licence to
companies that can't use AGPL code. That only works if every line can be
licensed both ways, so before your first pull request is merged you'll be
asked to agree to the [CLA](CLA.md). You keep the copyright in your work;
the CLA gives me the right to license it under the AGPL and commercially.

A check on your pull request will ask you to post one comment:

> I have read the CLA and I agree to it.

You only do this once.

## Being decent

Everyone taking part agrees to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

Please don't open a public issue for a security problem. See
[SECURITY.md](SECURITY.md).
