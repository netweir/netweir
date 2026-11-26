# Root certificates

Every browser ships its own list of root certificates, and a server whose
chain ends at a root one browser has and another hasn't is a server one of
them refuses. Netweir bundles two lists and gives each profile its own
browser's:

| Profile | Roots | Source |
|---|---|---|
| Firefox | Mozilla's: the NSS builtins Firefox ships | `webpki-root-certs` 1.0.11 |
| Chrome | the Chrome Root Store | `chromium-roots` 0.1.2 |
| Safari | the Chrome Root Store (see below) | `chromium-roots` 0.1.2 |

A profile names its list in its `[tls]` section (`roots = "mozilla"`, or
`"chromium"`, the default). `FetchOptions::ca_certificates` (`verify=` a
PEM path in Python) still takes the place of either list.

The same list counts as the browser's built-in roots for HTTP/3: Firefox
ends HTTP/3 when a chain that checks out ends at a root it didn't ship
(`network.http.http3.disable_when_third_party_roots_found`), so a Firefox
profile goes over HTTP/3 to a server whose root is Mozilla's and over TCP
to one whose root came from `ca_certificates` (`docs/design/http3.md`).

## How close Mozilla's list is to Firefox 160's

Firefox 160.0.1 ships NSS 3.132. Its `lib/ckfw/builtins/certdata.txt` has
118 certificates whose trust objects make them server-auth trust anchors
(`CKA_TRUST_SERVER_AUTH CKT_NSS_TRUSTED_DELEGATOR`). `webpki-root-certs`
builds its list from CCADB's report of the roots Mozilla includes with the
Websites trust bit, and its 1.0.11 is exactly those 118, by SHA-256. The
release after it, 1.0.12, has dropped six that NSS 3.132 still trusts
(DigiCert Trusted Root G4, QuoVadis Root CA 1 G3 and 3 G3, Buypass Class 2
and Class 3, Certigna), so the crate is pinned to `=1.0.11` until a
Firefox profile on a newer NSS moves it.

## Distrust-after dates

A list of certificates can't say that a root is trusted only for what it
issued before a date, and NSS's can. NSS 3.132 keeps Izenpe.com trusted
with a server distrust-after date of 2026-04-15 23:59:59 UTC
(`CKA_NSS_SERVER_DISTRUST_AFTER`). Firefox refuses a chain that ends at
it when the server certificate's notBefore is later than that, and
trusts one issued on or before it until it expires
(`NSSCertDBTrustDomain::IsChainValid` and `IsDistrustedCertificateChain`,
`security/certverifier/NSSCertDBTrustDomain.cpp`, which fail it with
`ERROR_ISSUER_NO_LONGER_TRUSTED`). A root the user added is exempt, even
one that is also built in. CCADB drops such a root 398 days after its
date, and `webpki-root-certs` drops it then too.

Netweir keeps the dates beside the list. `src/distrust_after.rs` holds
every root in `certdata.txt` that has one, by SHA-256: Izenpe.com, and
three Entrust roots dated 2024-11-30 that 3.132 no longer trusts for
servers anyway. `scripts/gen-distrust-after.py` writes that file from a
given NSS tag:

    cd crates/netweir-core && python3 scripts/gen-distrust-after.py NSS_3_132_RTM

A Firefox profile applies them to a chain that ends at a Mozilla root, the
way NSS does. Over TCP, a check beside BoringSSL's (wreq's
`tls_verify_callback`) fails the handshake. Over HTTP/3, the same check
runs on the chain neqo hands over. Roots from `ca_certificates` are
third-party roots, so their chains are never held to a date, and Chrome's
and Safari's profiles have no dates at all.

One part isn't followed: Firefox also ignores Certificate Transparency
timestamps later than the date (`CTVerifier::Verify` takes the date).
Netweir doesn't check Certificate Transparency.

## Where it still differs from NSS

- What NSS adds in code: its own name constraints for some roots and
  other checks `mozilla::pkix` makes that a list of certificates doesn't
  carry. Certificates are checked by BoringSSL, not `mozilla::pkix`.
- The system's roots. See below.

## Enterprise roots

`security.enterprise_roots.enabled` is `true` in Firefox 160 on every
platform (`modules/libpref/init/StaticPrefList.yaml`, with no
per-platform default). What it reads depends on the platform
(`security/manager/ssl/EnterpriseRoots.cpp`):

- Windows: the `ROOT` and `CA` stores of the local machine, its group
  policy and enterprise stores, the current user, and the user's group
  policy.
- macOS: certificates with trust settings in the user or admin domain.
  Roots the system ships (trust settings in the system domain) are
  skipped, so it's only what someone added: a company's root, or a
  debugging proxy's.
- Android: the user's CA certificates.
- Linux: nothing. There's no code for it.

Firefox counts these as third-party roots, so a chain that ends at one
keeps Firefox off HTTP/3 and isn't held to a distrust-after date.

Netweir doesn't read the system's store, and nothing on the wire depends
on it: the same handshake goes out either way. What changes is whether a
connection through a TLS-intercepting proxy succeeds. Firefox on a
company laptop trusts the company's root, while Netweir refuses it unless
it's given. `FetchOptions::ca_certificates` (`verify=` in Python) is the
way to give it, and Netweir then treats it as Firefox treats an
enterprise root. It's third-party, so the request stays off HTTP/3.
Reading the store would mean code for each platform's API, for a case
where a single option already does the same.

## Safari

Safari trusts Apple's root store, which no vetted crate bundles. Safari's
profile keeps the Chrome Root Store, the nearest list Netweir has; a
server whose root only one of the two has is treated differently from
Safari.
