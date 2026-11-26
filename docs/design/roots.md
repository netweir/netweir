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

Where it still differs from NSS:

- Distrust-after dates. NSS 3.132 keeps Izenpe.com trusted with a server
  distrust-after date of 2026-04-15: certificates it issued after that are
  refused, the ones before still trusted (`CKA_NSS_SERVER_DISTRUST_AFTER`).
  A bundled list has no such date, so Netweir trusts Izenpe's
  certificates whenever they were issued. CCADB drops such a root 398
  days after the date, which is when the crate does too.
- What NSS adds in code: its own name constraints for some roots and
  other checks `mozilla::pkix` makes that a list of certificates doesn't
  carry. Certificates are checked by BoringSSL, not `mozilla::pkix`.
- Enterprise roots and the system's store. Firefox can also trust the
  roots the operating system has (`security.enterprise_roots.enabled`),
  as third-party ones. Netweir doesn't read the system's store.

## Safari

Safari trusts Apple's root store, which no vetted crate bundles. Safari's
profile keeps the Chrome Root Store, the nearest list Netweir has; a
server whose root only one of the two has is treated differently from
Safari.
