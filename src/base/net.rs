//! Shared HTTP client construction.
//!
//! Every outbound HTTP(S) client in the crate is built here so that TLS is configured once: rustls
//! with the `ring` crypto provider, which needs neither OpenSSL headers nor cmake at build time.

use std::sync::Once;
use std::time::Duration;

use super::Result;

/// `User-Agent` sent by Anchor. Deliberately generic: no host, user or project information.
pub const USER_AGENT: &str = concat!("anchor/", env!("CARGO_PKG_VERSION"));

static CRYPTO_PROVIDER: Once = Once::new();

/// Installs `ring` as the process-wide rustls crypto provider. Idempotent and cheap; call it before
/// constructing any TLS-capable client (the constructors in this module already do).
pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER.call_once(|| {
        // An `Err` only means another provider was installed first, which is equally fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A `reqwest` client builder with Anchor's defaults: generic user agent, short connect timeout and
/// no request timeout (callers set per-request deadlines, since model calls can be long).
pub fn http_client_builder() -> reqwest::ClientBuilder {
    ensure_crypto_provider();
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(5))
}

/// Builds a `reqwest` client with Anchor's defaults and an optional overall request timeout.
pub fn http_client(timeout: Option<Duration>) -> Result<reqwest::Client> {
    let mut builder = http_client_builder();
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_and_without_timeout() {
        assert!(http_client(None).is_ok());
        assert!(http_client(Some(Duration::from_secs(1))).is_ok());
        ensure_crypto_provider();
    }
}
