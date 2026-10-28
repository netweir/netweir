# Security

## Reporting a problem

Please don't open a public issue. Report it privately through
[GitHub's form](https://github.com/netweir/netweir/security/advisories/new)
instead, and include what you found, how to reproduce it, and what an
attacker could do with it.

You'll hear back within a week. Once there's a fix, it ships in a release
with an advisory crediting you, unless you'd rather not be named.

## What counts

Anything that lets a page, a server, or a proxy that Netweir talks to do
more than it should. For example:

- a crafted page or response that crashes the parser, reads memory it
  shouldn't, or runs code;
- a way for a site to reach files, the network, or a local service through
  Netweir that the crawl didn't ask for;
- certificate checks that pass when they shouldn't;
- the Chrome driver leaking a page's data to another page or context;
- secrets, like proxy passwords, ending up in logs, checkpoints, or errors.

A site that can tell Netweir isn't a real browser is a bug, not a security
problem. Please open an ordinary issue for that.

## Supported versions

Fixes go into the newest release. Until Netweir reaches 1.0, older releases
don't get backports.

## How Netweir defends itself

[docs/design/security.md](docs/design/security.md) says what Netweir trusts,
what stands between a hostile page and your machine, and the limits it
still has.
