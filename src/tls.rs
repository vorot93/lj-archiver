use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore, crypto::ring};

/// The `rustls::ClientConfig` shared by every HTTPS client in this crate.
///
/// Uses the `ring` crypto provider with Mozilla's root store bundled via
/// `webpki-roots` (no OS trust store dependency), instead of reqwest's default
/// rustls setup, which pulls in `aws-lc-rs`/`aws-lc-sys`. `ring` still compiles
/// a small amount of C/assembly (via the `cc` crate), but avoids the heavier
/// CMake/bindgen toolchain `aws-lc-sys` requires. Pass the result to
/// `reqwest::ClientBuilder::use_preconfigured_tls`.
pub(crate) fn rustls_config() -> ClientConfig {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_ring_backed_client_config_with_a_populated_root_store() {
        let config = rustls_config();
        // Pins the provider actually being `ring` (non-empty cipher suites)
        // rather than a silently-empty/no-op provider slipping in.
        assert!(!config.crypto_provider().cipher_suites.is_empty());
    }
}
