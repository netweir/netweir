//! The root certificates each profile's browser trusts out of the box:
//! the Chrome Root Store for Chrome and Safari, Mozilla's for Firefox
//! (docs/design/roots.md).

use crate::profile::Roots;

/// The bundled roots of `roots`, DER.
pub(crate) fn bundled(roots: Roots) -> impl Iterator<Item = &'static [u8]> {
    let certs = match roots {
        Roots::Chromium => chromium_roots::TLS_SERVER_ROOT_CERTS,
        Roots::Mozilla => webpki_root_certs::TLS_SERVER_ROOT_CERTS,
    };
    certs.iter().map(|c| c.as_ref())
}

/// `roots` as a trust store for TCP connections, with `extra` (DER) in
/// it too: None when that is wreq's own default, the Chrome Root Store.
pub(crate) fn store(
    roots: Roots,
    extra: &[Vec<u8>],
) -> Result<Option<wreq::tls::trust::CertStore>, String> {
    if roots == Roots::Chromium && extra.is_empty() {
        return Ok(None);
    }
    let mut certs: Vec<&[u8]> = extra.iter().map(Vec::as_slice).collect();
    for der in bundled(roots) {
        certs.push(der);
    }
    wreq::tls::trust::CertStore::from_der_certs(certs)
        .map(Some)
        .map_err(|e| format!("the bundled root certificates: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mozillas_roots_are_nss_3_132s() {
        // NSS 3.132 (Firefox 160) has 118 server-auth trust anchors in
        // certdata.txt, and webpki-root-certs 1.0.11 is all of them.
        assert_eq!(bundled(Roots::Mozilla).count(), 118);
        assert_eq!(bundled(Roots::Chromium).count(), 115);
        // The two stores differ: some of each are the other's not.
        let mozilla: Vec<&[u8]> = bundled(Roots::Mozilla).collect();
        let chromium: Vec<&[u8]> = bundled(Roots::Chromium).collect();
        assert!(mozilla.iter().any(|m| !chromium.contains(m)));
        assert!(chromium.iter().any(|c| !mozilla.contains(c)));
    }
}
