//! The server's certificate and the client's pin (ADR-0031).
//!
//! The server makes a self-signed certificate on its first start; its key
//! is sealed with DPAPI like the database passwords. A client trusts
//! exactly one certificate — the one whose SHA-256 it was configured with
//! — and nothing else: not a public CA, not a certificate for the right
//! name. A different certificate is `server_identity_changed`, an error,
//! never a warning to click through.

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

use crate::db::embedded::protect;

const CERT: &str = "server-cert.der";
const KEY: &str = "server-key.bin";

/// What a pin mismatch says, so the client can tell it from a network
/// failure in the error chain reqwest hands back.
pub const IDENTITY_CHANGED: &str = "server_identity_changed";

pub struct Identity {
    pub cert: CertificateDer<'static>,
    key:      PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// The server's identity from `dir`, made on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let (cert_path, key_path) = (dir.join(CERT), dir.join(KEY));
        if cert_path.exists() && key_path.exists() {
            let cert = std::fs::read(&cert_path).with_context(|| format!("reading {}", cert_path.display()))?;
            let key = protect::open(&std::fs::read(&key_path)?).context("unsealing the server's key")?;
            return Ok(Self { cert: CertificateDer::from(cert), key: PrivatePkcs8KeyDer::from(key) });
        }
        let made = Self::generate()?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(&key_path, protect::seal(made.key.secret_pkcs8_der())?)?;
        std::fs::write(&cert_path, made.cert.as_ref())?;
        Ok(made)
    }

    pub fn generate() -> Result<Self> {
        let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(vec!["enclave".to_string()])?;
        Ok(Self {
            cert: cert.der().clone(),
            key:  PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
        })
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }

    pub fn server_config(&self) -> Result<rustls::ServerConfig> {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], PrivateKeyDer::Pkcs8(self.key.clone_key()))?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// Lowercase hex SHA-256 of the certificate's DER.
pub fn fingerprint(cert: &[u8]) -> String {
    Sha256::digest(cert).iter().map(|b| format!("{b:02x}")).collect()
}

/// A TLS client that accepts only the certificate with this fingerprint.
pub fn pinned_client_config(fingerprint: &str) -> rustls::ClientConfig {
    let provider = Arc::new(ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { fingerprint: fingerprint.to_lowercase(), provider }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

/// The pin. The name is not checked — the client reaches the server by
/// whatever name or address the office uses — because the fingerprint
/// already names exactly one certificate. Signatures are checked as usual:
/// presenting the certificate proves nothing without its key.
#[derive(Debug)]
struct Pinned {
    fingerprint: String,
    provider:    Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(IDENTITY_CHANGED.into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_survives_a_restart_and_its_key_is_not_stored_plain() {
        let dir = std::env::temp_dir().join(format!("enclave-tls-{}", uuid::Uuid::new_v4()));
        let first = Identity::load_or_create(&dir).unwrap();
        let again = Identity::load_or_create(&dir).unwrap();
        assert_eq!(first.fingerprint(), again.fingerprint());
        assert_eq!(first.fingerprint().len(), 64);
        let stored = std::fs::read(dir.join(KEY)).unwrap();
        #[cfg(windows)]
        assert_ne!(stored, first.key.secret_pkcs8_der());
        let _ = stored;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
