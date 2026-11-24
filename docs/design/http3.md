# HTTP/3: Firefox through neqo

A request over HTTP/3 has to look like the browser's from its first UDP
datagram: the QUIC Initial carries the ClientHello, and anyone on the path
can read it. For Firefox, Netweir doesn't imitate that. It runs the code
Firefox runs.

This is issue #387. When the fetcher moves to it, races TCP and falls
back, the way Firefox does, is #389, under
[When a request goes over HTTP/3](#when-a-request-goes-over-http3).
Both are in netweir-core's `http3` feature, which netweir-py's own
`http3` feature turns on. The wheels have it (#400); a build from the
source distribution doesn't unless it's asked for, since it needs NSS
built first. In Python it's the `http3=` setting, on by default.

## The stack

Firefox 160.0.1 builds its HTTP/3 from three pieces, and Netweir pins the
same three:

- neqo, Mozilla's QUIC and HTTP/3 in Rust, at tag v0.33.0
  (`netwerk/socket/neqo_glue/Cargo.toml` at FIREFOX_160_0_1_RELEASE);
- nss-rs, its binding to NSS, at commit ba8a3f6 (Firefox's `Cargo.lock`);
- NSS 3.132 (`security/nss/TAG-INFO`), built by `scripts/nss.sh` from
  Mozilla's release tarball, checked against its SHA-256.

`crates/netweir-core/src/h3.rs` sets them up the way
`NeqoHttp3Conn::new` in Firefox's `neqo_glue/src/lib.rs` does, line by
line and in the same order, with Firefox 160's values for every
preference it reads: version negotiation on, Cubic with classic slow
start, 24 MiB connection and 12 MiB stream flow control, a 30 s idle
timeout, SNI slicing, no QUIC GREASE, ML-KEM768+X25519, X25519 and P-256
key shares, zlib, zstd and brotli certificate compression, ECH GREASE on
every connection, a 64 KiB QPACK table and 20 blocked streams. Before the
first connection it also sets the process-wide NSS defaults Firefox's
`nsNSSComponent` sets at startup, two of which show in the ClientHello:
delegated credentials on, ML-DSA off.

## What matches, and how it's tested

`crates/netweir-core/tests/http3.rs` runs the fingerprint server's HTTP/3
side, sends requests through the new path, and holds every capture to
`profiles/firefox-160-macos.h3.capture.json` with `compare::differences`,
which now compares the QUIC flight and the HTTP/3 connection as well.

Exactly as Firefox sends them, on every run:

- the ClientHello: JA4 `q13d0315h3_55b375c5d22e_dc5437974b47`, its cipher
  suites, groups, key shares, signature algorithms, delegated credentials,
  certificate compression, record size limit and extensions, shuffled per
  connection with quic_transport_parameters and encrypted_client_hello
  last;
- the Initial flight: QUIC v1, a 3-byte Source Connection ID, no token,
  two datagrams of 1252 bytes over IPv4 (1232 over IPv6) padded with zero
  bytes after the packet, the ClientHello in three CRYPTO frames (the end
  first, then the start, both in the first packet; the middle in the
  second), packet numbers in sequence;
- the transport parameters: the same fourteen, the same values, in the
  same order;
- HTTP/3: the control, QPACK encoder and QPACK decoder streams in that
  order, SETTINGS (1:65536, 7:20, 0x2b603742:0, 0xffd277:1, 0x33:1, 8:1)
  in order, one GREASE frame on the control stream;
- each request: `:method :scheme :authority :path`, then the TCP
  profile's headers in the TCP profile's order, lowercase, without Host,
  Connection and te, with Alt-Used where Connection was, and Priority
  `u=0, i` for a navigation;
- a second connection to the server resumes the first one's session, with
  pre_shared_key last, as Firefox's did.

Random per connection, and checked to fall where Firefox's do: the
Destination Connection ID (8 to 20 bytes), the first packet number (under
1024), where the CRYPTO frames cut the server name, the GREASE values.

After the handshake, loss recovery, congestion control, pacing and ACK
timing are neqo's code with Firefox's settings, and the UDP socket is
quinn-udp as in Firefox, ECN marking included. The algorithms are the
same; the timing isn't, since it depends on the machine and on the event
loop driving neqo, which is Netweir's and not Firefox's socket thread.

## What doesn't match yet

- 0-RTT. With a ticket that allows early data, Firefox sends a GET in
  0-RTT. Netweir resumes the session but waits for the handshake.
- Where Alt-Used goes in a request that also carries Authorization: the
  recording has no login over HTTP/3, so Alt-Used takes Connection's place
  and Authorization stays before it, as over TCP.
- Images. Firefox gives an image Priority `u=4, i` or `u=5, i` depending on
  the page's layout; the profile sends `u=4, i`, as Firefox did over TCP.
- Addresses. Netweir connects to the first address the alternative's name
  resolves to; Firefox races IPv4 and IPv6.
- Certificates are checked with BoringSSL against the roots the TCP
  connections use, not by NSS against Mozilla's. Nothing about that is on
  the wire, but a server whose chain only one of the two root stores
  accepts is treated differently from Firefox.
- NSS's tarball bundles NSPR 4.39, where Firefox has 4.40. NSPR is NSS's
  portability layer and puts nothing on the wire.
- Proxies. Firefox never sends QUIC through an HTTP or SOCKS proxy, and
  nor does a Fetcher with one (below).

## When a request goes over HTTP/3

Only a profile with an `[http3]` section moves to HTTP/3: today that's
`firefox-160-macos`. Chrome and Safari stay on TCP until their own stacks
are in (#388, #390), and so do the older Firefox profiles, which have no
HTTP/3 recording. The rest follows Firefox 160.0.1's source and default
preferences.

- Alt-Svc. An `Alt-Svc` header on an https response that isn't a 421 or
  a 5xx (`nsHttpChannel::ProcessAltService`) is read as
  `AltSvcMapping::ProcessHeader` reads it: `clear` forgets the origin's
  alternative, `ma=` gives an alternative's lifetime in seconds, a day
  without one, and `ma=0` clears too. Of the `h3` alternatives the first
  is kept, unless its host is one HTTP/3 has failed to reach or its port
  is one Firefox never connects to (the 82 in `gBadPortList`,
  `netwerk/base/nsIOService.cpp`, which `NS_CheckPortSafety` reads);
  draft versions such as `h3-29` are ignored, as Firefox 160 ignores them.
- HTTPS records. As Firefox 160 does with TRR off
  (`network.dns.native_https_query`, `use_https_rr_as_altsvc`), each
  request to an https origin also asks the system's resolver for its
  HTTPS record, the way Firefox asks on that system:
  `res_nquery` on Linux, `DNSServiceQueryRecord` on macOS, `DnsQuery_A`
  on Windows 11 (not 10, where Firefox doesn't), under `_PORT._https.host`
  for a port other than 443, and never for an address, `localhost`,
  `.local`, a name in the hosts file or under a search domain. An http URL
  is asked about too, as Firefox's DNS prefetch asks. Alongside goes an
  address lookup for the canonical name, which a record's target has to
  match (`network.dns.https_rr.check_record_with_cname`). The answer is
  parsed, sorted and picked as `DNSPacket`, `HTTPSSVC.cpp` and
  `GetServiceModeRecordWithCname` do (`src/https_rr.rs`), and kept for
  its TTL and then the 600 s grace, or 300 s if there was none. The
  request doesn't wait for it: one whose TCP connection is still being
  made when a record offering h3 comes in moves to HTTP/3, as a
  transaction not yet on a connection does
  (`nsHttpTransaction::OnHTTPSRRAvailable`), and later requests use the
  record at once. The route isn't proved first, carries no `Alt-Used`,
  takes the record's ECH configuration in place of ECH GREASE, and races
  TCP after `network.dns.httpssvc.http3_fast_fallback_timeout`, 50 ms. An
  http URL isn't upgraded to https from a record: with TRR off, Firefox
  doesn't (`nsHttpChannel::MaybeUseHTTPSRRForUpgrade`).
- `h2` alternatives. Firefox takes those too, each one (`h2` on the
  origin's own host and port is none, and clears the origin's others, as
  `AltSvcMapping`'s constructor makes it one with no lifetime). Once one
  is proved by a TCP connection to it, with no request, the origin's
  requests go there over HTTP/2, for the origin's name, with `Alt-Used`
  (`nsHttpChannel::BeginConnect`), on connections of their own. An `h3`
  alternative comes first (`AltSvcCache::GetAltServiceMapping`).
- Proving it. As `AltSvcCache::UpdateAltServiceMapping` does, the
  fetcher connects to a new alternative at once, with no request, and
  only uses it once that connection is up with a certificate that checks
  out. Requests meanwhile stay on TCP. The connection then carries the
  origin's next requests, as Firefox's does. A header that repeats an
  alternative already in use only renews its lifetime. One that names a
  new alternative for an origin that has a proved one is proved the same
  way, and the old one carries the origin's requests until it is: Firefox
  stores the replacement only once it validates
  (`AltSvcMapping::SetSyncOnlyOnSuccess`). An alternative whose connection
  fails stays unproved and holds off another offer of it for two seconds
  (`AltSvcMappingValidator::OnTransactionDestroy`).
- Partitions. Alternatives are kept per origin and per top-level site,
  the network partition Firefox keys them by (the origin attributes in
  `AltSvcMapping::MakeHashKey`), and HTTP/3 connections and session
  tickets likewise. They live as long as the Fetcher, as in a private
  window; Firefox's normal windows also keep them on disk
  (AlternateServices.bin) across restarts.
- Racing TCP. A request whose alternative's connection is still being
  made waits `network.http.http3.backup_timer_delay`, 100 ms, then opens
  a TCP connection too (`nsHttpTransaction::OnHttp3BackupTimer`). It goes
  over whichever is up first: TCP, without `Alt-Used`, once that
  connection is made or at once on one already open
  (`OnBackupConnectionReady`); HTTP/3 if its connection came up while the
  TCP one was still being made. Either way the other keeps going: the
  HTTP/3 connection for the next requests, the TCP one to the end of its
  TLS handshake.
- Failing. A connection that never comes up, for any reason but a TLS
  error, stops HTTP/3 to its alternative's host (`Http3Session::Shutdown`,
  `nsHttpHandler::ExcludeHttp3`). So does one that was up and closes with
  an error the server or neqo raised, which `Http3Session::ProcessEvents`
  reads as `NS_ERROR_NET_HTTP3_PROTOCOL_ERROR`: anything but a stateless
  reset, no error at either layer (`H3_NO_ERROR` for HTTP/3), a server
  asking to retry with ECH, or a close Netweir started itself. Firefox has no timer for that: the
  exclusion lasts until the network changes, when every exclusion goes
  (`nsHttpHandler::Observe` on `NS_NETWORK_LINK_TOPIC`). Netweir gets no
  notice of a network change, so it checks, before trusting an
  exclusion, whether the addresses the system would send from over IPv4
  and IPv6 are still the ones it had when the exclusion was made.
- Falling back. A request that the HTTP/3 connection went down under
  before any of its response came is sent again over TCP if its method
  is safe to repeat; a POST fails instead (`nsHttpTransaction::Close`).
  A request that never went out on HTTP/3 always goes over TCP.
- Proxies. With a `proxy`, the Fetcher never learns an alternative and
  sends no UDP at all, as Firefox's `AltSvcMapping::AcceptableProxy` and
  `UpdateAltServiceMapping` refuse HTTP/3 behind one. A system proxy is
  read as wreq reads it, from the environment: the first of `HTTPS_PROXY`
  and `https_proxy` that is set, else of `ALL_PROXY` and `all_proxy`, if
  wreq can use it, and none under CGI. Behind one, only the hosts
  `NO_PROXY` exempts, which wreq sends direct, move to HTTP/3, matched by
  wreq's rules (addresses, networks, and domains with their subdomains).
- Off. `FetchOptions::http3 = false` keeps a Fetcher on TCP whatever its
  profile.

`Fetcher::use_http3(origin, alternative)` stays, for tests that need
HTTP/3 from the first request: it routes the origin to the alternative
as if proved, ignores Alt-Svc for it, and never races or falls back.

What's not the same as Firefox:

- HTTPS records that send TCP somewhere else (another port, or ECH over
  TCP) aren't followed; those requests go to the origin. A record's h3
  route is followed, ECH included.
- The losing TCP connection of a fast fallback goes to the origin, where
  Firefox takes the best record without h3 if there is one
  (`PrepareFastFallbackConnInfo`); with the canonical-name check, that is
  almost always the origin anyway.
- The canonical name the record check needs comes from a lookup of its
  own (getaddrinfo with `AI_CANONNAME`, as NSPR asks for Firefox),
  alongside wreq's; a system with no caching resolver may send the
  address queries twice. On Windows the check is skipped.
- The race's losing TCP connection. When HTTP/3 wins, Firefox's backup
  connection sends HTTP/2's preface and waits in the pool. Netweir's
  finishes its TLS handshake and closes, as wreq can't keep a connection
  it made for a request it no longer sends.
- `h2` alternatives are proved by a TCP connection whose TLS handshake
  and certificate check out; Firefox also insists the connection agreed
  on HTTP/2, which wreq doesn't tell. And an origin whose URL has a port
  can't have its HTTP/2 sent to another port, as wreq connects to the
  port the URL gives, so such an alternative is ignored.

## Building it

The `http3` feature of netweir-core is off by default, and the wheels are
built without it. Turning it on there means building NSS in every wheel
job, inside the manylinux containers included, and in a build from the
sdist; that's its own change. With the feature, NSS has to come first:

```
pip install gyp-next ninja      # NSS builds with gyp and ninja
scripts/nss.sh                  # prints NSS_DIR and NSS_PREBUILT
NSS_DIR=... NSS_PREBUILT=1 cargo test -p netweir-core --features http3 --test http3
```

netweir-core's build script stops the build if NSS_DIR isn't set: left to
itself, nss-rs would take whatever NSS pkg-config finds, or NSS's
development tip, and either would put another ClientHello on the wire.

Two TLS libraries in one binary need a few things done to NSS after it's
built, all in `scripts/nss.sh`:

- NSS's cryptography (freebl) defines some of the names BoringSSL does
  (`SHA256_Update`, `HMAC_Init`, `CMAC_Init` and a few more). Linked
  together, each library calls the other's, and the process crashes. Every
  symbol freebl defines gets a `netweir_nss_` prefix, and every reference
  to it in NSS with it, except `FREEBL_GetVector`, the one nss-rs calls.
- NSS's own SQLite (Linux and Windows; on macOS NSS uses the system's)
  defines every name the SQLite rusqlite links does. Its symbols get the
  same prefix, so NSS keeps a private copy and netweir's databases keep
  theirs.
- nss-rs links NSS's shared libraries in a release build outside Windows,
  and a wheel can't carry them. The script deletes them and puts static
  archives under their names.

CI's `http3` job builds NSS and runs the tests on every system and
architecture a wheel is built for: Linux x86_64 and aarch64 (in release
builds, which link NSS the way a wheel would), macOS arm64 and x86_64
(cross-built, tested under Rosetta) and Windows x64.
