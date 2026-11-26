# HTTP/3: Firefox through neqo, Chrome and Safari through quiche

A request over HTTP/3 has to look like the browser's from its first UDP
datagram: the QUIC Initial carries the ClientHello, and anyone on the path
can read it. For Firefox, Netweir doesn't imitate that. It runs the code
Firefox runs.

This is issue #387, and Chrome's, further down, is
[#388](#chrome-through-netweirs-quiche). Safari's, after it, is
[#390](#safari-the-closest-match). When the fetcher moves to it,
races TCP and falls back, the way Firefox does, is #389, under
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
  pre_shared_key last, as Firefox's did;
- 0-RTT: `profiles/firefox-160-macos.h3-0rtt.capture.json` recorded
  Firefox 160.0.1 opening the fingerprint server's `/zero-rtt` with
  session tickets that allow early data. Its script asks until a
  response comes over HTTP/3, lets that connection time out, and goes to
  `/again`. Firefox sent that GET in 0-RTT on the connection that
  resumed, in its first flight: the 0-RTT packet coalesced after the
  second Initial, filling that datagram. Netweir's first flight has the
  same packets in the same datagrams, and the request comes in 0-RTT.

Random per connection, and checked to fall where Firefox's do: the
Destination Connection ID (8 to 20 bytes), the first packet number (under
1024), where the CRYPTO frames cut the server name, the GREASE values.

After the handshake, loss recovery, congestion control, pacing and ACK
timing are neqo's code with Firefox's settings, and the UDP socket is
quinn-udp as in Firefox, ECN marking included. The algorithms are the
same; the timing isn't, since it depends on the machine and on the event
loop driving neqo, which is Netweir's and not Firefox's socket thread.

## What doesn't match yet

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
  accepts is treated differently from Firefox. The same goes for what
  counts as a built-in root. Firefox ends HTTP/3 when a chain that checks
  out ends at a root it didn't ship
  (`network.http.http3.disable_when_third_party_roots_found`,
  `Http3Session::Authenticated`): HTTP/3 to the host stops, and the
  request goes over TCP as a retry, whatever its method. Netweir does the
  same, with the Chrome Root Store as its built-in roots, so a chain to a
  CA from `FetchOptions::ca_certificates` that isn't one of them stays on
  TCP.
- NSS's tarball bundles NSPR 4.39, where Firefox has 4.40. NSPR is NSS's
  portability layer and puts nothing on the wire.
- Proxies. Firefox never sends QUIC through an HTTP or SOCKS proxy, and
  nor does a Fetcher with one (below).

## When a request goes over HTTP/3

Only a profile with an `[http3]` section moves to HTTP/3: today that's
`firefox-160-macos`, `chrome-157-macos` and `safari-27-macos`. The older
Firefox and Chrome profiles have no HTTP/3 recording and stay on TCP.
This section is Firefox's, following Firefox 160.0.1's source and
default preferences; Chrome's rules are
[their own](#when-chrome-moves-to-http3), and so are
[Safari's](#when-safari-moves-to-http3).

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
  match (`network.dns.https_rr.check_record_with_cname`). It's the same
  lookup the connections use: `network.dns.always_ai_canonname` makes
  every address lookup getaddrinfo with `AI_CANONNAME`, as NSPR calls it
  on Linux, macOS and Windows alike, and the host cache keeps one entry
  for both, for `network.dnsCacheExpiration` (60 s, or the TTL on
  Windows, which a second lookup asks `DnsQuery_A` for, as
  `network.dns.get-ttl` has Firefox do there), then serves it for
  `network.dnsCacheExpirationGracePeriod` (600 s) while it looks the name
  up again; a failed lookup is kept the same way. The answer is
  parsed, sorted and picked as `DNSPacket`, `HTTPSSVC.cpp` and
  `GetServiceModeRecordWithCname` do (`src/https_rr.rs`), and kept for
  its TTL and then the 600 s grace, or 300 s if there was none. The
  request doesn't wait for it: one whose TCP connection is still being
  made when a record comes in takes the record's route, as a
  transaction not yet on a connection does
  (`nsHttpTransaction::OnHTTPSRRAvailable`), the connection going on to
  wait in the pool, and later requests use the record at once. The
  route isn't proved first, carries no `Alt-Used`, and takes the
  record's ECH configuration in place of ECH GREASE
  (`CloneAndAdoptHTTPSSVCRecord`). Over TCP, BoringSSL builds both
  ClientHellos where Firefox has NSS, so `ech_inner_like_nss` in the
  profile makes it write the ClientHelloInner as NSS does
  (`SSL_CTX_set_ech_inner_like_nss`, in netweir/btls): the
  `encrypted_client_hello` marker first, then `server_name` and
  `supported_versions`, the ClientHelloOuter's cipher suites, TLS 1.2
  ones included, and every other extension, `delegated_credentials` and
  `record_size_limit` among them, in `ech_outer_extensions` in the outer
  hello's order. NSS takes a configuration's first cipher suite, and so
  does BoringSSL once `aes_hw_override` tells it the machine has AES
  hardware; without that, BoringSSL prefers ChaCha20-Poly1305 on a
  machine without it. The same option makes ECH GREASE NSS's
  (`tls13_MaybeGreaseEch`): AES-128-GCM or ChaCha20-Poly1305 drawn at
  random for each connection, where BoringSSL takes AES-128-GCM on any
  machine with AES hardware, and a payload as long as the
  EncodedClientHelloInner would be, padded for a `maximum_name_length`
  of 100, where BoringSSL draws one of four lengths. The padding counts
  the host's name even for an IP address, which gets ECH GREASE but no
  `server_name`, and a second ClientHello after a HelloRetryRequest
  repeats the first one's ECH GREASE.
  `profiles/firefox-160-macos.ech.capture.json` recorded Firefox
  160.0.1's outer and inner ClientHellos, decrypted with a key
  `examples/ech_capture.rs` holds, for `maximum_name_length` 0 and 64
  and for cipher suites with ChaCha20-Poly1305 first and last, and its
  ECH GREASE for a short and a long host name, 127.0.0.1 and ::1.
  `tests/ech_tcp.rs` holds Netweir's to them: the same bytes but the
  randoms, key shares and encrypted payload, padding included. Since
  rounding to 32 bytes hides the name's length at those hosts, it also
  holds the GREASE payload to NSS's padding formula for addresses and
  names on both sides of each 32-byte step. It checks every Firefox
  profile's ECH GREASE, and finishes handshakes with a BoringSSL server
  that holds the key, ECH accepted, after a HelloRetryRequest too. A
  record that offers h3 sends the request over HTTP/3, racing TCP after
  `network.dns.httpssvc.http3_fast_fallback_timeout`, 50 ms; that TCP
  connection goes to the best record without h3, which must have ECH if
  the h3 one did, else to the origin, or nowhere if ECH was used and no
  such record has it (`PrepareFastFallbackConnInfo`). Any other record
  sends it over TCP to the record's target and port, with its ECH, and
  a connection that doesn't agree on the ALPN the record names is
  dropped (`nsHttpConnection`'s NPN token check). A connection from a
  record that fails goes on as `PrepareConnInfoForRetry` does: without
  ECH, to the origin; with it, again with the configurations a server
  that rejected it sends back, or without ECH if it sent none, else to
  the next record with ECH, to the origin only if some record has none,
  and nowhere once they're all tried
  (`network.dns.echconfig.fallback_to_origin_when_all_failed` is off).
  A failed handshake of any kind goes on this way, an alert included,
  and every connection after a failed one leaves `compress_certificate`
  out of its ClientHello (`IS_RETRY`, which `nsNSSIOLayer` reads).
  Moving to a record that came in while the request's own connection was
  being made isn't a retry (`OnHTTPSRRAvailable`), so the record's first
  connection keeps it. A record that sends TCP to the origin as
  it was still makes the request one from records, so its failed
  connection is made again once at the origin. And a record that keeps
  the origin's host and port has no routed host to compare, so the entry
  that failed is tried again (`nsHttpConnectionInfo`'s constructor):
  for `. alpn=h2,http/1.1 ech=...` Firefox 160.0.1 made three
  connections. Such a record's connections go as the origin's do, with
  no route of their own. Under the failed connection's name, an entry
  from a record that named no ALPN is left out as well
  (`SVCBRecord::GetSelectedAlpn` fails, `nsHttpTransaction.cpp:1227`).
  The records to go on to are listed once, at the first such failure
  (`PrepareSVCBRecordsForRetry`): h3 entries stay on the list unless
  HTTP/3 has already failed for the request in a way that excludes its
  host (`NS_HTTP_DISALLOW_HTTP3`), or HTTP/3 is excluded from their
  target. A QUIC socket that can't be set up excludes its host but
  leaves the request's other h3 records
  (`DnsAndConnectSocket::SetupConn`). An h3 record taken from the list
  gets its own HTTP/3 connection, with a fast fallback as above, and the
  request makes at most 10 connections in all
  (`network.http.request.max-attempts`). An
  http URL isn't upgraded to https from a record. Firefox's
  `network.dns.upgrade_with_https_rr` is on, but
  `nsHttpChannel::MaybeUseHTTPSRRForUpgrade` skips the upgrade unless TRR
  is in use (`canUseHTTPSRRonNetwork`, with
  `network.dns.force_use_https_rr` off), and with TRR off it isn't.
- `h2` alternatives. Firefox takes those too, each one (`h2` on the
  origin's own host and port is none, and clears the origin's others, as
  `AltSvcMapping`'s constructor makes it one with no lifetime). Once one
  is proved by a TCP connection to it, with no request, the origin's
  requests go there over HTTP/2, for the origin's name, with `Alt-Used`
  (`nsHttpChannel::BeginConnect`), on connections of their own. A
  connection to it that's reset or closed during its handshake sends the
  request back to the origin's direct route, without `Alt-Used`
  (`nsHttpTransaction::Restart`), as a retry: Firefox made one connection
  to the alternative and then nine to the origin, none of them offering
  `compress_certificate`. A handshake the alternative refuses with an
  alert ends the request there. An `h3`
  alternative comes first (`AltSvcCache::GetAltServiceMapping`).
- Proving it. As `AltSvcCache::UpdateAltServiceMapping` does, the
  fetcher connects to a new alternative at once, with no request, and
  only uses it once that connection is up with a certificate that checks
  out, and, for an `h2` one, has agreed on HTTP/2
  (`AltSvcTransaction::MaybeValidate`); one that agreed on anything else
  is closed. Requests meanwhile stay on TCP. The connection then
  carries the origin's next requests, as Firefox's does. A header that
  repeats an alternative already in use only renews its lifetime. One
  that names a new alternative for an origin that has a proved one is
  proved the same way, and the old one carries the origin's requests
  until it is: Firefox stores the replacement only once it validates
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
  a TCP connection too (`nsHttpTransaction::OnHttp3BackupTimer`, a
  `FallbackTransaction`), with the profile's ClientHello. The request
  goes over HTTP/3 if that connection came up while the TCP one was still
  being made. Once the TCP connection's handshake is over, whether it
  worked or not, Firefox moves the request to TCP
  (`OnBackupConnectionReady`, when the `FallbackTransaction` the
  connection drove its handshake with closes: `nsHttpConnection` lets it
  read only once `EnsureNPNComplete` says the handshake is done, and
  `SpeculativeTransaction::Close` takes that as success): it restarts the transaction on the direct
  route (`IS_RETRY`), whose connections pool apart from the backup's (a
  fallback connection info, `SetFallbackConnection`). So the request
  goes over a connection of its own, without `compress_certificate`, and
  the backup's waits in its own entry, unused. Recorded from Firefox
  160.0.1 with `network.http.http3.alt-svc-mapping-for-testing` and an
  alternative whose QUIC went unanswered: two TCP connections, the
  answer on the second. The HTTP/3 connection keeps going for the next
  requests. A request whose HTTP/3 fails goes to TCP the same way, as a
  retry. A TCP connection that never connects leaves the request to
  HTTP/3.
- Racing TCP from an HTTPS record. The fast fallback's backup and the
  restarted request share an entry, so a backup that's up carries the
  request and nothing more is made. One whose handshake failed is made
  again (`IS_RETRY`), and then the request goes on as
  `PrepareConnInfoForRetry` says: for `. alpn=h3,h2` without ECH,
  Firefox made the backup, the same record again and the origin, the
  last two without `compress_certificate`, and didn't wait for HTTP/3.
- Failing. A connection that never comes up stops HTTP/3 to its
  alternative's host (`Http3Session::Shutdown`,
  `nsHttpHandler::ExcludeHttp3`), unless the certificate check failed
  (the only NSS error Firefox's session sees) or the server rejected ECH
  and sent configurations to retry with. A server that rejected ECH with
  nothing to retry with closes with `ech_required`, which is
  `NS_ERROR_NET_HTTP3_PROTOCOL_ERROR` and excludes the host like any
  other failure. A connection from an HTTPS record whose ECH the server
  rejected with configurations is made again to the same record with
  them (`SSL_ERROR_ECH_RETRY_WITH_ECH` in `PrepareConnInfoForRetry`). So does one that was up and closes with
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
  and `https_proxy` that is set (`HTTP_PROXY` and `http_proxy` for an
  http URL), else of `ALL_PROXY` and `all_proxy`, if wreq can use it, and
  none under CGI. Behind one, only the hosts
  `NO_PROXY` exempts, which wreq sends direct, move to HTTP/3, matched by
  wreq's rules (addresses, networks, and domains with their subdomains).
- Off. `FetchOptions::http3 = false` keeps a Fetcher on TCP whatever its
  profile.

`Fetcher::use_http3(origin, alternative)` stays, for tests that need
HTTP/3 from the first request: it routes the origin to the alternative
as if proved, ignores Alt-Svc for it, and never races or falls back.

What's not the same as Firefox:

- ECH over TCP is held to recordings of configurations without
  extensions of their own; how NSS and BoringSSL treat a configuration
  extension isn't compared.
- The host cache, like the HTTPS records', is keyed by name alone;
  Firefox keys both by the request's top-level site too.

### 0-RTT

Firefox's, as Netweir follows it:

- A connection resumed with a ticket that allows early data is in 0-RTT
  (`network.http.http3.enable_0rtt`, Http3Session::Init's ZERORTT) and
  takes requests at once. A request whose method is safe (GET, HEAD,
  OPTIONS, TRACE, PROPFIND, REPORT, SEARCH:
  nsHttpRequestHead::IsSafeMethod) goes in 0-RTT; any other waits for
  the handshake (Http3Session's mCannotDo0RTTStreams). The request the
  connection was made for goes in its first flight.
- It keeps the backup timer: if the handshake isn't done when that
  fires, TCP races the request as for a connection still being made
  (MaybeStartHttp3BackupTimer; Finish0RTT cancels it).
- A server that turns the early data down has the requests sent in 0-RTT
  sent again once connected, without 0-RTT (ZeroRttRejected,
  Http3Session::Finish0Rtt).
- A connection still in 0-RTT after `network.http.http3.0rtt_timeout`
  (5 s) fails. A failure before connecting after a request went in 0-RTT
  doesn't exclude the host, since the next connection makes a full
  handshake (Http3Session::Shutdown, mHad0RttStream).

Chrome's (Chromium 157, net/quic): every session's SSL context has early
data on. A session doesn't wait for confirmation once QUIC has worked on
the network and the alternative isn't broken or recently broken
(QuicSessionPool::CreateSessionSync, QuicSessionAttempt), so a request
whose method is safe (HttpUtil::IsMethodSafe, HttpNetworkTransaction's
`can_send_early_data_`) goes as soon as the session can send early data;
any other waits for the handshake (QuicHttpStream::DoRequestStream,
`requires_confirmation`). Chrome's QUIC job is done then, so nothing
races it. Chrome isn't recorded doing this: headless Chrome for Testing
157 stalled its first request before it reached the network on the
machine these were made on.

Safari's isn't recorded, so a Safari profile resumes and waits for the
handshake.

## Chrome through netweir's quiche

Chrome's QUIC is Google's QUICHE, in C++, and building it into the
wheels would mean abseil and bazel on six targets. Netweir runs
Cloudflare's quiche instead, in Rust, forked to
[netweir/quiche](https://github.com/netweir/quiche) on the `chrome-157`
branch, pinned by commit in netweir-core's Cargo.toml. The fork does two
things.

It links btls, the patched BoringSSL Netweir's TCP connections already
use, in place of the `boring` crate. Both declare `links = "boringssl"`,
so only one of them can be in a build, and with btls the ClientHello in
the QUIC Initial comes from the same library, set up from the same
profile, as the TCP one.

And it adds what quiche did differently from Chrome, each a commit on
that branch, each ported from or checked against QUICHE's source:

- transport parameters in a new random order on every connection, one
  GREASE parameter (31 × N + 27, 0 to 15 random bytes), version
  information with a GREASE version at a random place, and integer
  parameters left out at their defaults
  (`SerializeTransportParameters`);
- the Initial flight: a port of QUICHE's chaos protection
  (`QuicChaosProtector` and `MultiPacketChaosProtect`), which spreads the
  ClientHello over two packets and splits each share into CRYPTO frames
  shuffled among PING frames and runs of PADDING; packets filled to 1250
  bytes with PADDING frames, packet numbers from 1;
- the BoringSSL handle per connection, for ECH GREASE, key shares and
  ALPS;
- HTTP/3: SETTINGS in identifier order with a GREASE setting, a GREASE
  frame after them, no QPACK or GREASE streams until there's something to
  send on one, and Chrome's `u=0, i` in PRIORITY_UPDATE;
- QPACK: quiche dropped whatever a server put in the dynamic table, and
  Chrome offers servers a 64 KiB one, so the decoder now fills and reads
  it, holds back headers that wait for entries, and acknowledges them as
  Chromium's `QpackDecoder` does.

`crates/netweir-core/src/h3_chrome.rs` sets each connection up from the
profile, the way Chromium's QUIC sets up its own: TLS 1.3 only, with no
TLS GREASE and BoringSSL's own signature algorithms, as QUICHE sets
none; the profile's groups, key shares, ALPS codepoint, brotli
certificate compression, ECH GREASE, extension shuffling and trust
anchors; ALPN and ALPS `h3`.

### What matches, and how it's tested

`crates/netweir-core/tests/http3_chrome.rs` sends a typed navigation and
an image through it to the fingerprint server and holds each to
`profiles/chrome-157-macos.h3.capture.json` with `compare::differences`.

Exactly as Chrome sends them, on every run:

- the ClientHello: JA4 `q13d0312h3_55b375c5d22e_178839b6cec1`, and
  `q13d0313h3_55b375c5d22e_b0954bf1abdf` on a resumed connection, with
  its ciphers, groups, key shares, signature algorithms, ALPS, brotli,
  trust anchors and extensions;
- QUIC v1, an 8-byte Destination Connection ID and no Source Connection
  ID, no token, two Initial datagrams of 1250 bytes over IPv4 (1230 over
  IPv6) with nothing after the packet, numbered 1 and 2 in one and two
  bytes, no ACK;
- the transport parameters as a set, with Chrome's values, and
  initial_rtt only on a resumed connection;
- HTTP/3: only the control stream, SETTINGS 1:65536, 6:262144, 7:100,
  0x33:1 and a GREASE setting in that order, a GREASE frame, then one
  PRIORITY_UPDATE for each request that repeats its priority;
- each request: `:method :authority :scheme :path`, then the TCP
  profile's headers in its order, lowercase, without Host and
  Connection, and no Alt-Used.

Random per connection, and checked over many connections to vary and to
stay in the range Chrome draws from: the order of the transport
parameters and of the ClientHello's extensions, the GREASE parameter,
version, setting and frame, and the CRYPTO layout (3 to 23 CRYPTO frames
and 4 to 20 PINGs in all, a first piece of 55 to 86 bytes).

### What isn't Chrome's

- After the first round trips, loss recovery, congestion control, pacing
  and ACK timing are quiche's, not Chrome's. Only Google's QUICHE would
  copy them, and it's the build cost above. A server that studies how a
  connection behaves under loss can tell.
- QPACK the other way: Chrome's encoder may put request headers in a
  server's dynamic table; quiche's encodes from the static table only.
  The fingerprint server offers no table, so this isn't in the
  recording, but a server that offers one sees the difference.
- 0-RTT turned down. Chrome's QUICHE marks the data it sent in 0-RTT for
  retransmission as soon as the server turns early data down; quiche
  sends it again once loss detection calls it lost, about a PTO later.
- Packet numbers after the second: Netweir writes them in at least two
  bytes, which is what Chrome's congestion window gives at the start;
  Chrome's own length then follows its window.
- Addresses: the first one a name resolves to, where Chrome would also
  race IPv4 and IPv6 for TCP.
- Certificates are checked after the handshake, against the TCP
  connections' roots, rather than by Chrome's verifier during it.

### When Chrome moves to HTTP/3

Chrome's rules differ from Firefox's, and the Chrome profile follows
Chromium's `net/` source:

- Alt-Svc. Read on every final response from an https origin but a 421
  (`HttpNetworkTransaction::ProcessAltSvcHeader`), parsed as QUICHE's
  `SpdyAltSvcWireFormat::ParseHeaderFieldValue` does. A header it can't
  parse, `ma=0` included, changes nothing. One it can replaces the
  origin's whole list (`HttpServerProperties::SetAlternativeServices`);
  `clear`, or a list of nothing Chrome uses, such as only `h3-29`,
  forgets it. `ma` defaults to a day.
- No proving. Chrome uses an alternative from the next request on: the
  first `h3` entry that hasn't expired, isn't broken, and isn't on a port
  Chrome refuses or a high port for a low one, unless a later one already
  has a connection up (`JobController::GetAdvertisedAltSvcInternal`).
  Other hosts are fine.
- Racing. The QUIC connection and a TCP one start together, and the
  request goes over whichever is ready first. TCP waits only once QUIC
  has worked on this network and the alternative wasn't recently broken,
  and then for one and a half times the server's RTT, or 450 ms without
  one, at most 3 s (`QuicSessionPool::GetTimeDelayForWaitingJob`). A
  QUIC connection that loses keeps going, and carries the next requests.
- Broken. When the QUIC connection fails and the TCP one worked, the
  alternative is broken for 10 s, then 20, 40 and so on up to 30 minutes
  (`BrokenAlternativeServices`, with Chrome's field trial defaults). A
  handshake gives up after 10 s. When the mark expires the alternative is
  gone, and the site has to offer it again; a handshake that completes
  clears the count.
- Falling back. A request that a QUIC error cuts off before its
  response goes again over TCP, whatever its method
  (`HttpNetworkTransaction::HandleIOError`).
- HTTPS records. Chrome 157 asks for an origin's HTTPS record with every
  address lookup, through its own resolver as well as over DoH: on
  macOS, Windows and Linux its built-in client sends the query to the
  system's nameservers (`kAsyncDns`, `kUseDnsHttpsSvcb`, and the
  `AdditionalDnsQueryTypesEnabled` policy, all on by default). It
  falls back to `getaddrinfo`, which asks for no HTTPS record, only
  where it can't read the system's settings: a VPN, a proxy, Name
  Resolution Policy or Zero Trust DNS on Windows, an `nsswitch.conf` it
  doesn't understand on Linux. A record that lists `h3` takes the
  request to QUIC with no Alt-Svc at all: whenever an https request
  has no proxy, and QUIC to the origin isn't marked broken,
  `JobController::DoCreateJobs` starts a `DNS_ALPN_H3` job beside the
  TCP one (`use_dns_https_svcb_alpn` is now always on;
  the `UseDnsHttpsSvcbAlpn` feature is gone). The QUIC connection goes
  to the origin's own host and port. Chrome reads a ServiceMode record
  only if it names no other port and its target is the name the
  addresses' CNAMEs end at, so `.` works on port 443 and not on others.
  It ignores every record once one is AliasMode, and all of them when
  each says `no-default-alpn`. Of the rest, the first by priority that
  lists `h3` is the one QUIC uses (`ExtractHttpsResults`,
  `HostCache::Entry::GetEndpoints`, `QuicSessionPool::SelectQuicVersion`).
- http URLs. Chrome asks for an http URL's HTTPS record too, under the
  https URL's name: port 80 becomes 443, and any other port stays, so
  `http://host:8080/` asks for `_8080._https.host`
  (`dns_util::GetNameForHttpsQuery`). If any record comes back that
  Chrome can read, the lookup fails with `ERR_DNS_NAME_HTTPS_ONLY`
  (`HostResolverDnsTask::ShouldTriggerHttpToHttpsUpgrade`). "Can read"
  means an AliasMode record, or a ServiceMode one whose `mandatory`
  names only keys Chrome knows (`ExtractHttpsResults`,
  `ServiceFormHttpsRecordRdata::IsCompatible`); its target, port and
  ALPN don't matter, so a record Chrome wouldn't use for QUIC still
  upgrades. `URLRequestHttpJob` then answers with a 307 of its own to
  the same URL on https, port and all, with `Location`,
  `Cross-Origin-Resource-Policy: Cross-Origin` and
  `Non-Authoritative-Reason: DNS`
  (`RedirectUtil::SynthesizeRedirectHeaders`). Netweir does the same: the
  request waits for the record as an https one does, and the 307 is a
  hop like any other redirect. It counts against `max_redirects`, and
  `allow_redirects=False` or `once` hands it back. Nothing goes over the
  network for it, so it isn't in the archive; the https request after
  it is. The upgrade doesn't depend on QUIC, but Netweir's record lookup
  lives in the `http3` feature, so it happens only with that feature
  and `http3` on, and never with a proxy, where Chrome looks up the
  proxy and not the host (`ConnectJobParamsFactory`).
- Timing. The HTTPS query goes out with the address ones, and once the
  addresses are in it gets a fifth of the time they took, at least 5 ms
  and at most 50 ms, before Chrome goes on without it
  (`HostResolverDnsTask`). Both connections wait for that. Then QUIC
  and TCP race as for Alt-Svc, with the same delay for TCP, except that
  TCP doesn't wait at all if the origin already has a TCP connection
  open (Netweir counts one as open for 90 seconds after its last TCP
  answer). A record without `h3` sends the request to TCP at once. An
  Alt-Svc alternative, when there is one, is the job TCP waits for, so
  the request goes there. The answer stays in the host cache for its
  TTL, and at least a minute.
- ECH. The record's ECH configuration goes into the QUIC ClientHello,
  which BoringSSL then sends with real ECH in place of GREASE
  (`QuicChromiumClientSession::GetSSLConfig`). The outer hello names
  the configuration's public name.
- Proxies, and `http3 = false`, as for Firefox: no QUIC at all.

Not the same as Chrome's:

- Netweir's network is the Fetcher, so "QUIC has worked" and the broken
  marks live as long as it does, where Chrome keeps them on disk and
  forgets the first on a network change.
- Netweir makes the HTTPS query the way Firefox does on the system
  (`res_nquery`, `DNSServiceQueryRecord`, `DnsQuery_A`), so the
  resolver sees Firefox's query, not Chrome's. Netweir reads Firefox's
  answer too: an unknown key in `mandatory` drops the whole answer, not
  just that record, and records with ECH come first among those of equal
  priority.
- The time the addresses take is timed with `getaddrinfo`, and their TTL
  isn't known, so an answer with no records stands for a minute.
- The TCP connection that loses a race to QUIC finishes its TLS
  handshake and closes. Chrome's socket pool keeps it, idle
  (`ClientSocketHandle::Reset` leaves its connect job running), and sends
  HTTP/2's preface only once a request uses it, which wreq can't do.
- `http3 = false`, or a build without the feature, turns off the HTTPS
  record lookup, and with it the upgrade of http URLs. Chrome with QUIC
  off still upgrades them.
- When the http request carries `Origin`, Chrome's 307 also has
  `Access-Control-Allow-Origin` and `Access-Control-Allow-Credentials`.
  Netweir's doesn't, since it does no CORS checks.
- The records come from Firefox's parser, which knows `odoh` (32769)
  and not `trust_anchor_ids`, where Chrome's knows the reverse. An answer
  whose `mandatory` names `odoh` upgrades in Netweir and not in Chrome;
  one that names `trust_anchor_ids` upgrades in Chrome and not in
  Netweir.
- When both Alt-Svc and a record offer HTTP/3 at different places, Chrome
  races both QUIC connections. Netweir only uses the Alt-Svc one.
- If every record has ECH, Chrome won't fall back to TCP without it.
  Netweir's TCP connections don't use ECH from records, so they connect
  without it.

## Safari: the closest match

Safari's QUIC is Apple's own, in Network.framework, and its source isn't
public. Netweir can't run it, as it runs Firefox's, or port it, as it
ported Chrome's. What it can do is record it and set the Chrome engine up
to send the same thing. That makes Safari's HTTP/3 the closest match
Netweir has, not an exact one: everything below that says "exactly" was
compared with a recording, and nothing was compared with code.

### The recording

`profiles/safari-27-macos.h3.capture.json` is Safari 27.2 on macOS
26.7.2, against `capture --h3`, whose certificate the Mac already
trusted as a root. Safari moved to HTTP/3 by itself, on the server's
`Alt-Svc`, on `localhost` and a port that isn't 443. The recording has
five rounds on fresh ports: `/image?set=nw` over HTTP/2, whose favicon
went on a new QUIC connection, then, with the server restarted, `/image`
and its picture on a QUIC connection that resumed the first. That gives
ten QUIC connections, five fresh and five resumed. 43 more connections,
not committed, were recorded for the transport parameters' order.

The profile is Safari 27.0's headers. 27.2 sent the same TCP ClientHello
(JA4 `t13d2013h2_a09f3c656075_7f0f34a4126d`) and the same headers but
for the version in its User-Agent, which the test takes from the profile.

### The stack

The Chrome engine (`crates/netweir-core/src/h3_chrome.rs`) on the
fork's `safari-27` branch, which builds on `chrome-157` and adds three
options, each off unless asked for: `enable_safari_transport_params`,
`enable_safari_initial` and `h3::Config::enable_safari`. Chrome's
connections don't use them and haven't changed.

The ClientHello comes from the profile's `[tls]`, as the TCP one does:
GREASE, AES-256 first among the TLS 1.3 ciphers, the signature
algorithms with rsa_pss_rsae_sha384 twice, OCSP and SCT requests, zlib
certificate compression, X25519MLKEM768 and X25519 key shares. QUIC
allows TLS 1.3 only, so BoringSSL leaves out extended_master_secret,
renegotiation_info and ec_point_formats, as Safari does, and
`[http3] extension_order` puts quic_transport_parameters where Safari
puts it, before compress_certificate. No ALPS, no ECH GREASE.

### What matches, and how it's tested

`crates/netweir-core/tests/http3_safari.rs` sends a navigation and an
image to the fingerprint server, on fresh and resumed connections, and
holds each to the recording with `compare::differences`, the Initial's
layout included.

Exactly as Safari sent them, on every run:

- the ClientHello: JA4 `q13d0311h3_55b375c5d22e_f2a83c8e78ae`, and
  `q13d0312h3_55b375c5d22e_151122171f7d` resumed, with its cipher order,
  extensions in their order, GREASE in the same places, groups, key
  shares, signature algorithms and certificate compression; a resumed
  hello is the same length as Safari's;
- QUIC v1, an 8-byte Destination Connection ID, no Source Connection
  ID, no token; two Initial datagrams of 1200 bytes, numbered 0 and 1
  in one byte, each with one CRYPTO frame (999 bytes of the hello in the
  first, the rest in the second) and then PADDING frames to the end;
- the transport parameters: initial_max_data 16777216, the three stream
  limits 2097152, initial_max_streams_uni 8, active_connection_id_limit
  64 and an empty initial_source_connection_id, and nothing else: no
  max_idle_timeout, max_udp_payload_size, GREASE or
  version_information;
- HTTP/3: only the control stream, SETTINGS 1:16383, 7:100 and one
  GREASE setting in that order, nothing after them;
- each request: `:method :scheme :authority :path`, then the TCP
  profile's headers in its order, lowercase, without Host and
  Connection, Priority as a header and no PRIORITY_UPDATE.

Random per connection, and checked to vary: where the transport
parameters start (they're always ascending from it, wrapping round), the
GREASE values.

### What isn't Safari's

What the recording can't show, Netweir has had to choose:

- Where the transport parameters start. Safari's 53 recorded
  connections started at every one of the seven, but not evenly
  (initial_max_data 21 times, active_connection_id_limit 14,
  initial_source_connection_id and initial_max_streams_uni 5 each,
  initial_max_stream_data_bidi_remote 4, the other two 2 each). Netweir
  draws the start with those weights, but 53 connections can't say
  whether Safari's choice depends on something else, such as whether
  the connection resumes.
- Everything after the first flight: loss recovery, congestion
  control, pacing, ACK timing, packet sizes once the handshake is done
  (quiche's 1200 bytes), connection ID use and PING keepalives (Apple
  says it pings every second while a request waits, every 20 seconds
  while a body comes in). These are quiche's.
- The idle timeout. Safari advertises none, so neither does Netweir,
  and the connection lasts as long as the server's timeout allows.
  Safari surely closes idle connections itself; when isn't recorded.
- QPACK the other way. The fingerprint server offers no dynamic table,
  so whether Safari's encoder uses one isn't recorded; quiche's encodes
  from the static table.
- 0-RTT, which Safari may send on a resumed connection. It isn't
  recorded, so Netweir waits for the handshake.
- Addresses. Safari tried QUIC to `::1`, then to `127.0.0.1` 250 ms
  later; Netweir tries the first address only.
- The image request over HTTP/1.1. Safari's `[image]` headers are
  recorded over HTTP/2 and HTTP/3; over HTTP/1.1, Host and Connection
  go where Safari's other HTTP/1.1 requests put them.
- Another recording disagrees on one field. curl-impersonate's Safari
  27.0 signature (`tests/signatures/safari270.yaml`) lists
  max_idle_timeout 300000 among the parameters. Safari 27.2 sent none
  on any of its 53 recorded connections, so the profile follows 27.2.

### When Safari moves to HTTP/3

Apple documents that Safari learns HTTP/3 from `Alt-Svc` and from HTTPS
DNS records, races QUIC first, and falls back to TCP. It gives no
timings, so the rules below are what Safari 27.2 was timed doing on
macOS, and a guess where they say so.

- Alt-Svc, read as for Chrome (a guess: Apple says it remembers the
  header for future connections, but not how it parses it or honours
  `ma`). No proving, and another port is fine: Safari took
  `h3=":9483"` from an origin on 8483.
- A new connection only. Safari kept sending over its open HTTP/2
  connection after learning an alternative, and went to HTTP/3 only
  for a new one. Netweir can't see wreq's connection pool, so it takes
  an origin's TCP connection as open for 90 seconds after its last TCP
  response, wreq's idle timeout, and sends the origin's requests over
  TCP until then. A response over HTTP/3 doesn't count: once QUIC
  carried the origin, Safari opened no TCP connection to keep it on
  (#419). An HTTP/3 connection already up is used either way.
- HTTPS records. Apple's Tommy Pauly wrote (DNSOP, September 2020) that
  the stack queries them alongside the addresses on every connection and
  races QUIC first when one offers h3. A Safari request with no
  alternative yet waits for its origin's records, at most 2 seconds (a
  guess), and goes over QUIC on first contact if they offer h3. They're
  looked up as for Firefox, through the system's resolver; that isn't
  verified against Safari, since the capture can't serve DNS.
- Racing. Safari sent its QUIC Initials, then started TCP 250 ms later,
  each of the four times it was timed against a server that never
  answered QUIC; that's also the Connection Attempt Delay of Happy
  Eyeballs v3, an Apple-led draft. A request whose QUIC connection isn't
  up after 250 ms goes over TCP. When QUIC was up first, Safari opened
  no TCP connection at all.
- Broken. Pauly wrote that QUIC goes first only with "no history of
  QUIC failures", but Safari went back to QUIC first on each of three
  new connections over 47 seconds after the last had failed. Netweir
  marks nothing broken; the open-TCP rule above keeps it from trying
  QUIC on every request.
- Falling back. A request a QUIC error cuts off before its response
  goes again over TCP if its method is safe to repeat (a guess: Apple
  only documents falling back for a connection).
- http URLs stay on http whatever their records say. Apple hasn't
  said whether Safari upgrades from a record: Pauly's message says the
  query goes out for `http://` URLs as well, and nothing about what
  comes of it, and Chromium's intent to ship the upgrade called it
  unclear whether Safari did. The capture can't serve DNS, so it isn't
  recorded either.
- Proxies, and `http3 = false`, as for the others: no QUIC at all.

## Building it

The `http3` feature of netweir-core is off by default. The wheels turn
it on (#400); a build from the sdist doesn't unless it's asked for. With
the feature, NSS has to come first:

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

Chrome's quiche needs nothing more: it links the BoringSSL the TCP side
already builds.

CI's `http3` job builds NSS and runs the tests on every system and
architecture a wheel is built for: Linux x86_64 and aarch64 (in release
builds, which link NSS the way a wheel would), macOS arm64 and x86_64
(cross-built, tested under Rosetta) and Windows x64.
