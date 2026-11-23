//! With the http3 feature, NSS has to be the build scripts/nss.sh makes:
//! the release Firefox ships, linked statically, its cryptography's
//! symbols kept apart from BoringSSL's. Without NSS_DIR, nss-rs would
//! use whatever NSS pkg-config finds, or clone NSS's tip, and either
//! would put another ClientHello on the wire.

fn main() {
    println!("cargo:rerun-if-env-changed=NSS_DIR");
    if std::env::var_os("CARGO_FEATURE_HTTP3").is_some() && std::env::var_os("NSS_DIR").is_none() {
        panic!(
            "netweir-core's http3 feature needs NSS from scripts/nss.sh: run it, \
             then set the NSS_DIR and NSS_PREBUILT it prints"
        );
    }
}
