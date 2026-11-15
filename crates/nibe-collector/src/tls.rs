//! Certificate pinning for the pump's self-signed TLS certificate.
//!
//! The pump's certificate cannot be verified against a CA, so instead of turning verification
//! off, the collector can accept exactly one certificate: the one whose SHA-256 fingerprint
//! is configured in `NIBE_TLS_FINGERPRINT`. The handshake signature is still verified with the
//! certificate's public key, so a host that merely copies the pump's certificate cannot
//! complete the handshake.
//!
//! The signature is checked against the raw public key rather than through webpki's
//! certificate parser, because that parser only accepts X.509 v3 and self-signed device
//! certificates are often v1.

use anyhow::{bail, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature_with_raw_key, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::{DigitallySignedStruct, PeerMisbehaved, SignatureScheme};
use std::sync::Arc;

/// Reads one DER element: returns (tag, the whole element, its contents, the rest).
fn der_next(input: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        (rest[..n].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize), &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    let header = input.len() - rest.len();
    Some((tag, &input[..header + len], &rest[..len], &rest[len..]))
}

/// The SubjectPublicKeyInfo of an X.509 certificate (any version).
fn cert_spki(cert: &[u8]) -> Option<&[u8]> {
    let (0x30, _, cert, _) = der_next(cert)? else { return None };
    let (0x30, _, mut tbs, _) = der_next(cert)? else { return None };
    // optional [0] version, then serial, signature algorithm, issuer, validity, subject
    if tbs.first() == Some(&0xa0) {
        tbs = der_next(tbs)?.3;
    }
    for _ in 0..5 {
        tbs = der_next(tbs)?.3;
    }
    match der_next(tbs)? {
        (0x30, spki, _, _) => Some(spki),
        _ => None,
    }
}

fn bad_cert() -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
}

/// Parses a SHA-256 fingerprint as printed by `openssl x509 -fingerprint -sha256`
/// (`sha256 Fingerprint=AB:CD:...`), or as plain hex. Case and separators are ignored.
pub fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    let value = s.rsplit('=').next().unwrap_or(s);
    let hex: String = value.chars().filter(|c| !matches!(c, ':' | ' ' | '-')).collect();
    if hex.len() != 64 {
        bail!("NIBE_TLS_FINGERPRINT must be a SHA-256 fingerprint (64 hex digits), got {} digits", hex.len());
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| anyhow::anyhow!("NIBE_TLS_FINGERPRINT is not hex"))?;
    }
    Ok(out)
}

pub fn fmt_fingerprint(fp: &[u8]) -> String {
    fp.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

#[derive(Debug)]
struct PinnedCert {
    fingerprint: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        // Comparing a public fingerprint needs no constant-time check.
        if got.as_ref() == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint {} does not match NIBE_TLS_FINGERPRINT {}",
                fmt_fingerprint(got.as_ref()),
                fmt_fingerprint(&self.fingerprint)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let spki = SubjectPublicKeyInfoDer::from(cert_spki(cert.as_ref()).ok_or_else(bad_cert)?);
        verify_tls12_with_raw_key(message, &spki, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let spki = SubjectPublicKeyInfoDer::from(cert_spki(cert.as_ref()).ok_or_else(bad_cert)?);
        verify_tls13_signature_with_raw_key(message, &spki, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// The TLS 1.2 counterpart of rustls's `verify_tls13_signature_with_raw_key`: TLS 1.2 schemes
/// can map to several algorithms, so each candidate is tried in turn (as rustls does for
/// certificates).
fn verify_tls12_with_raw_key(
    message: &[u8],
    spki: &SubjectPublicKeyInfoDer<'_>,
    dss: &DigitallySignedStruct,
    algs: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    let candidates = algs
        .mapping
        .iter()
        .find(|(scheme, _)| *scheme == dss.scheme)
        .map(|(_, a)| *a)
        .ok_or(rustls::Error::PeerMisbehaved(PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme))?;
    let key = webpki::RawPublicKeyEntity::try_from(spki).map_err(|_| bad_cert())?;
    for alg in candidates {
        match key.verify_signature(*alg, message, dss.signature()) {
            Ok(()) => return Ok(HandshakeSignatureValid::assertion()),
            Err(webpki::Error::UnsupportedSignatureAlgorithmForPublicKeyContext(_)) => continue,
            Err(_) => break,
        }
    }
    Err(rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature))
}

/// A rustls client configuration that accepts only the certificate with this fingerprint.
pub fn pinned_client_config(fingerprint: [u8; 32]) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCert { fingerprint, provider: provider.clone() });
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_formats() {
        let colon = "AB:".repeat(31) + "AB";
        assert_eq!(parse_fingerprint(&colon).unwrap(), [0xAB; 32]);
        assert_eq!(parse_fingerprint(&"ab".repeat(32)).unwrap(), [0xAB; 32]);
        assert_eq!(parse_fingerprint(&format!("sha256 Fingerprint={colon}")).unwrap(), [0xAB; 32]);
        assert!(parse_fingerprint("ABCD").is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn public_key_is_found_in_v1_and_v3_certificates() {
        // both certificates were made from the same key; spki.der is that key's public part
        let spki: &[u8] = include_bytes!("../testdata/spki.der");
        assert_eq!(cert_spki(include_bytes!("../testdata/cert-v1.der")), Some(spki));
        assert_eq!(cert_spki(include_bytes!("../testdata/cert-v3.der")), Some(spki));
        assert_eq!(cert_spki(&[0x30, 0x03, 0x02, 0x01]), None);
        assert_eq!(cert_spki(&[]), None);
    }
}
