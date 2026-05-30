//! SigV4 signing for the io_uring fast path, using the runtime-agnostic
//! `aws-sigv4` crate (no SDK / hyper involved).

use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    sign, PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings,
};
use aws_sigv4::sign::v4;

/// Holds credentials + region for signing many requests on one thread.
#[derive(Clone)]
pub struct Signer {
    creds: Credentials,
    region: String,
}

impl Signer {
    pub fn new(
        access_key: String,
        secret_key: String,
        session_token: Option<String>,
        region: String,
    ) -> Signer {
        let creds = Credentials::new(access_key, secret_key, session_token, None, "rs5cmd-fast");
        Signer { creds, region }
    }

    /// Signs `req` in place for S3, adding `Authorization`, `x-amz-date`, and
    /// `x-amz-content-sha256`. `body` is the full payload bytes (empty for GET).
    /// The request URI must be absolute (scheme://host/path) and the `host`
    /// header must be set before calling.
    pub fn sign_s3(&self, req: &mut http::Request<()>, body: &[u8]) -> anyhow::Result<()> {
        let identity = self.creds.clone().into();

        let mut settings = SigningSettings::default();
        // S3 requires the payload hash header.
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;

        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()?
            .into();

        let headers: Vec<(String, String)> = req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let header_refs = headers.iter().map(|(k, v)| (k.as_str(), v.as_str()));

        // UNSIGNED-PAYLOAD avoids hashing the body into the signature on every
        // request (the body is still sent; integrity is covered by TLS/ETag).
        // This removes a SHA-256 over each payload from the hot path.
        let _ = body;
        let signable = SignableRequest::new(
            req.method().as_str(),
            req.uri().to_string(),
            header_refs,
            SignableBody::UnsignedPayload,
        )?;

        let (instructions, _sig) = sign(signable, &params)?.into_parts();
        instructions.apply_to_request_http1x(req);
        Ok(())
    }
}
