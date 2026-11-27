//! The root certificates each profile's browser trusts out of the box:
//! the Chrome Root Store for Chrome and Safari, Mozilla's for Firefox
//! (docs/design/roots.md).

use btls::asn1::Asn1Time;
use btls::stack::StackRef;
use btls::x509::{X509, X509StoreContextRef, X509VerifyError};

use crate::profile::Roots;

#[path = "distrust_after.rs"]
mod table;

/// Roots (DER) and, for each, the date (seconds since the Unix epoch)
/// after which a certificate issued under it isn't trusted.
pub(crate) type DistrustAfter = [(Vec<u8>, i64)];

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

/// `root`'s server distrust-after date: from `extra`, or from NSS's
/// builtins (src/distrust_after.rs, made by
/// scripts/gen-distrust-after.py from certdata.txt's
/// CKA_NSS_SERVER_DISTRUST_AFTER).
fn distrust_after(root: &[u8], extra: &DistrustAfter) -> Option<i64> {
    if let Some((_, at)) = extra.iter().find(|(der, _)| der == root) {
        return Some(*at);
    }
    let digest = btls::sha::sha256(root);
    table::SERVER_DISTRUST_AFTER
        .iter()
        .find(|(d, ..)| *d == digest)
        .map(|(_, at, _)| *at)
}

/// Whether Firefox refuses `chain`, as verified (leaf first, the root it
/// ends at last), because the root has a server distrust-after date and
/// the leaf's notBefore is later (NSSCertDBTrustDomain::IsChainValid and
/// IsDistrustedCertificateChain: ERROR_ISSUER_NO_LONGER_TRUSTED). Only
/// Mozilla's roots carry the dates, and Firefox skips them for a
/// third-party root, so callers apply it to the bundled store alone.
pub(crate) fn distrusted(chain: &StackRef<X509>, extra: &DistrustAfter) -> bool {
    let (Some(leaf), Some(root)) = (chain.iter().next(), chain.iter().next_back()) else {
        return false;
    };
    let Some(after) = root
        .to_der()
        .ok()
        .and_then(|der| distrust_after(&der, extra))
    else {
        return false;
    };
    #[allow(
        clippy::useless_conversion,
        reason = "time_t is narrower on some targets"
    )]
    let after = after
        .try_into()
        .ok()
        .and_then(|at| Asn1Time::from_unix(at).ok());
    after.is_some_and(|after| leaf.not_before() > after)
}

/// BoringSSL's check of each certificate of a TCP connection's chain with
/// Firefox's distrust-after dates on top: the leaf, checked last, fails
/// when its root is distrusted for it. What it returns replaces
/// BoringSSL's verdict, so it only ever adds a refusal: a certificate
/// BoringSSL failed stays failed.
pub(crate) fn verify_step(ok: bool, ctx: &mut X509StoreContextRef, extra: &DistrustAfter) -> bool {
    if !ok {
        return false;
    }
    if ctx.error_depth() != 0 {
        return true;
    }
    let refused = ctx.chain().is_some_and(|chain| distrusted(chain, extra));
    if refused {
        ctx.set_error(Err(X509VerifyError::APPLICATION_VERIFICATION));
    }
    !refused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mozillas_roots_are_nss_3_133s() {
        // NSS 3.133 (Firefox 161) has 118 server-auth trust anchors in
        // certdata.txt, the same file as 3.132's, and webpki-root-certs
        // 1.0.11 is all of them.
        assert_eq!(bundled(Roots::Mozilla).count(), 118);
        // chromium-roots 0.1.3: Chrome Root Store version 45, the PKI
        // Metadata component Chrome 158 takes in place of its built-in 43.
        assert_eq!(bundled(Roots::Chromium).count(), 110);
        // The two stores differ: some of each are the other's not.
        let mozilla: Vec<&[u8]> = bundled(Roots::Mozilla).collect();
        let chromium: Vec<&[u8]> = bundled(Roots::Chromium).collect();
        assert!(mozilla.iter().any(|m| !chromium.contains(m)));
        assert!(chromium.iter().any(|c| !mozilla.contains(c)));
    }

    #[test]
    fn izenpe_is_distrusted_after_15_april_2026() {
        // NSS 3.133's one trusted root with a server distrust-after date.
        let izenpe: Vec<&[u8]> = bundled(Roots::Mozilla)
            .filter(|der| distrust_after(der, &[]).is_some())
            .collect();
        assert_eq!(izenpe.len(), 1);
        let cert = X509::from_der(izenpe[0]).unwrap();
        let name = format!("{:?}", cert.subject_name());
        assert!(name.contains("Izenpe"), "{name}");
        // 260415235959Z
        assert_eq!(distrust_after(izenpe[0], &[]), Some(1_776_297_599));
        // A date given for a root comes first.
        assert_eq!(
            distrust_after(izenpe[0], &[(izenpe[0].to_vec(), 7)]),
            Some(7)
        );
    }
}
