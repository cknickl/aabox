//! TLS configuration for the AAP handshake.
//!
//! The car (head unit) acts as TLS server-or-client depending on protocol
//! direction; the canonical aasdk handshake puts the **head unit's** cert/key
//! pair on the source side (counter-intuitive, but that's how the protocol's
//! mutual handshake works given the car expects to validate "an Android phone
//! identifying as a certified head unit accessory" — a pattern Google never
//! intended for public use, and now a long-standing reverse-engineering
//! workaround).
//!
//! The cert + key are vendored from aasdk's `Cryptor.cpp` under
//! `embedded-certs/`. See LEGAL note in README.md for the gray-zone caveat.

use anyhow::{anyhow, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, ServerConfig};
use std::sync::Arc;

const HEADUNIT_CRT: &[u8] = include_bytes!("../embedded-certs/headunit.crt");
const HEADUNIT_KEY: &[u8] = include_bytes!("../embedded-certs/headunit.key");

/// Build a rustls ClientConfig for connecting to the head unit over TLS.
///
/// - Presents the embedded JVC Kenwood ("headunit") cert as our client identity
/// - Trusts ANY server cert the car presents (the car self-signs; we have no
///   trust anchor for it). We use a custom verifier that accepts everything,
///   which is correct for AAP — the protocol's "auth" model is the shared
///   knowledge of the headunit private key, not server-cert validation.
pub fn build_client_config() -> Result<Arc<ClientConfig>> {
    // ring is the configured crypto provider via workspace features.
    let provider = rustls::crypto::ring::default_provider();

    // Parse cert chain
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut std::io::Cursor::new(
        HEADUNIT_CRT,
    ))
    .collect::<std::result::Result<Vec<_>, _>>()
    .context("parse headunit cert")?;
    if certs.is_empty() {
        return Err(anyhow!("no certificates found in embedded headunit.crt"));
    }

    // Parse private key (RSA PKCS#1)
    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(HEADUNIT_KEY))
        .context("parse headunit key")?
        .ok_or_else(|| anyhow!("no private key found in embedded headunit.key"))?;

    // Note: we use a custom server-cert verifier (NoVerification) below, which
    // accepts any cert the car presents. AAP's auth model is the shared
    // knowledge of the headunit private key, not server-cert trust.

    let cfg = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS12])
        .context("rustls protocol_versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerification))
        .with_client_auth_cert(certs, key_into_der(key))
        .context("rustls with_client_auth_cert")?;

    Ok(Arc::new(cfg))
}

fn key_into_der(key: PrivateKeyDer<'static>) -> PrivateKeyDer<'static> {
    key
}

/// Test-only: build a ServerConfig using the same embedded cert+key. Used by
/// the in-process fake head unit in our integration tests so we can self-test
/// the full TLS-over-AAP handshake.
pub fn build_test_server_config() -> Result<Arc<ServerConfig>> {
    let provider = rustls::crypto::ring::default_provider();
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut std::io::Cursor::new(
        HEADUNIT_CRT,
    ))
    .collect::<std::result::Result<Vec<_>, _>>()
    .context("parse headunit cert (server)")?;
    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(HEADUNIT_KEY))
        .context("parse headunit key (server)")?
        .ok_or_else(|| anyhow!("no private key (server)"))?;

    let cfg = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS12])
        .context("server protocol_versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("server with_single_cert")?;
    Ok(Arc::new(cfg))
}

/// Custom server-cert verifier: accept anything. AAP doesn't use server-cert
/// validation in the conventional sense.
#[derive(Debug)]
struct NoVerification;

impl rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_and_key_load_cleanly() {
        let cfg = build_client_config().expect("build_client_config");
        // Smoke: ClientConfig has an Arc so equality isn't checked; just hold it.
        assert!(Arc::strong_count(&cfg) >= 1);
    }
}
