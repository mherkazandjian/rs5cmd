//! TLS crypto for every S3 connection: the SDK path's client (`storage::s3::build_http_client`)
//! and the `--fast` client both take [`provider`].
//!
//! aws-lc-rs, with classic key exchange by default: X25519, then the NIST curves P-256 and
//! P-384 (s5cmd v2.3.0, built with Go 1.22, offers these and P-521). `--tls-post-quantum` puts the
//! post-quantum hybrid X25519MLKEM768 first (aws-lc-rs's own default order): recorded traffic
//! then stays safe from a future quantum computer, for a larger ClientHello and an ML-KEM key
//! pair per handshake. The default offers no post-quantum group at all, so a server that prefers
//! one cannot ask for it with a second round trip (HelloRetryRequest).
//!
//! The credential providers (STS, SSO) keep the SDK's own HTTPS client, which offers
//! X25519MLKEM768 first either way.

use std::sync::Arc;

use rustls::crypto::{aws_lc_rs, CryptoProvider};

/// The notice `--tls-post-quantum` prints on stderr (not with `--json`).
pub const POST_QUANTUM_NOTICE: &str = "note: --tls-post-quantum: post-quantum key exchange \
     (X25519MLKEM768) makes setting up each TLS connection somewhat slower";

/// aws-lc-rs with the key exchange groups for `post_quantum`, most preferred first.
pub fn provider_for(post_quantum: bool) -> CryptoProvider {
    use aws_lc_rs::kx_group::{SECP256R1, SECP384R1, X25519, X25519MLKEM768};
    let mut p = aws_lc_rs::default_provider();
    p.kx_groups = if post_quantum {
        vec![X25519MLKEM768, X25519, SECP256R1, SECP384R1]
    } else {
        vec![X25519, SECP256R1, SECP384R1]
    };
    p
}

/// Makes [`provider_for`] the process's rustls provider. Call before the first TLS client is
/// built: the first provider installed stays.
pub fn install(post_quantum: bool) {
    let _ = provider_for(post_quantum).install_default();
}

/// The process's provider: the one [`install`] chose, else classic key exchange.
pub fn provider() -> Arc<CryptoProvider> {
    if let Some(p) = CryptoProvider::get_default() {
        return p.clone();
    }
    install(false);
    CryptoProvider::get_default().expect("a rustls provider was just installed").clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::NamedGroup;

    fn groups(p: &CryptoProvider) -> Vec<NamedGroup> {
        p.kx_groups.iter().map(|g| g.name()).collect()
    }

    #[test]
    fn classic_by_default_and_post_quantum_first_on_request() {
        use NamedGroup::{secp256r1, secp384r1, X25519, X25519MLKEM768};
        assert_eq!(groups(&provider_for(false)), [X25519, secp256r1, secp384r1]);
        assert_eq!(groups(&provider_for(true)), [X25519MLKEM768, X25519, secp256r1, secp384r1]);
    }

    /// The groups a client on `provider_for(pq)` lists in its ClientHello (supported_groups)
    /// and those it sends a key share for, in order.
    fn offered(pq: bool) -> (Vec<u16>, Vec<u16>) {
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider_for(pq)))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut client = rustls::ClientConnection::new(Arc::new(cfg), name).unwrap();
        let mut hello = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut hello).unwrap();
        }
        let u16_at = |b: &[u8], i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        // Record header (5), handshake header (4), version (2), random (32), then session id,
        // cipher suites and compression methods, each behind its length.
        let mut i = 5 + 4 + 2 + 32;
        i += 1 + hello[i] as usize;
        i += 2 + u16_at(&hello, i) as usize;
        i += 1 + hello[i] as usize;
        let end = i + 2 + u16_at(&hello, i) as usize;
        i += 2;
        let (mut groups, mut shares) = (Vec::new(), Vec::new());
        while i < end {
            let (kind, len) = (u16_at(&hello, i), u16_at(&hello, i + 2) as usize);
            let body = &hello[i + 4..i + 4 + len];
            match kind {
                0x000a => groups.extend((2..body.len()).step_by(2).map(|j| u16_at(body, j))),
                0x0033 => {
                    let mut j = 2;
                    while j < body.len() {
                        shares.push(u16_at(body, j));
                        j += 4 + u16_at(body, j + 2) as usize;
                    }
                }
                _ => {}
            }
            i += 4 + len;
        }
        (groups, shares)
    }

    #[test]
    fn the_client_hello_offers_post_quantum_only_on_request() {
        // IANA TLS group ids.
        const PQ: u16 = 0x11ec; // X25519MLKEM768
        const X25519: u16 = 0x001d;
        const P256: u16 = 0x0017; // secp256r1
        const P384: u16 = 0x0018; // secp384r1
        let (groups, shares) = offered(false);
        assert_eq!(groups, [X25519, P256, P384]);
        assert_eq!(shares, [X25519]);
        let (groups, shares) = offered(true);
        assert_eq!(groups, [PQ, X25519, P256, P384]);
        assert_eq!(shares.first(), Some(&PQ));
    }
}
