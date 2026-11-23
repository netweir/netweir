# HTTP/3: Firefox through neqo

A request over HTTP/3 has to look like the browser's from its first UDP
datagram: the QUIC Initial carries the ClientHello, and anyone on the path
can read it. For Firefox, Netweir doesn't imitate that. It runs the code
Firefox runs.

This is issue #387. Learning an origin's HTTP/3 alternative from Alt-Svc,
racing TCP and falling back to it are the fetcher's, in #389; until then
nothing but tests sends HTTP/3, through `Fetcher::use_http3`.

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
  `use_http3` refuses a Fetcher that has one.

## Building it

The `http3` feature of netweir-core is off by default, and the wheels are
built without it until #389 uses it. With it, NSS has to come first:

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
