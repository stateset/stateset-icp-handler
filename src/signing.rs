//! Receipt signing (Ed25519).
//!
//! Each state-changing response carries a **receipt** — a compact JWS
//! (header `{"alg":"EdDSA","typ":"JWT","kid":"<kid>"}`) whose payload is
//! the receipt claims described in ICP §9. The signed bytes are
//! `base64url(header) + "." + base64url(payload)`; the signature is the
//! Ed25519 signature over those bytes.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Receipt claims as laid out in ICP §9.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptClaims {
    pub iss: String,
    pub aud: String,
    pub iat: i64,
    pub jti: String,
    pub icp: ReceiptIcp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptIcp {
    pub version: String,
    pub intent: String,
    pub transaction_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mandate_jti: Option<String>,
    pub body_digest: String,
    pub body_canonicalization: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwsHeader {
    pub alg: String,
    pub typ: String,
    pub kid: String,
}

/// Wrapper around an Ed25519 keypair used for receipt signing.
#[derive(Clone)]
pub struct ReceiptSigner {
    pub kid: String,
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
}

impl std::fmt::Debug for ReceiptSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptSigner")
            .field("kid", &self.kid)
            .finish()
    }
}

impl ReceiptSigner {
    /// Generate an ephemeral signer (useful for dev; reject in prod).
    pub fn generate(kid: impl Into<String>) -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        Self {
            kid: kid.into(),
            signing_key,
            verifying_key,
        }
    }

    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }

    /// JWKS entry for `/.well-known/icp/jwks.json`.
    pub fn jwk(&self) -> serde_json::Value {
        let x = URL_SAFE_NO_PAD.encode(self.verifying_key_bytes());
        serde_json::json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": self.kid,
            "alg": "EdDSA",
            "use": "sig",
            "x": x,
        })
    }

    /// Produce a compact-JWS receipt over the supplied response body bytes.
    ///
    /// `body_json_bytes` MUST be the JCS-canonicalized bytes of the response
    /// body so that verifiers independently canonicalizing the same payload
    /// arrive at the same digest.
    pub fn sign_receipt(
        &self,
        aud_agent_id: &str,
        iss_handler_url: &str,
        intent: &str,
        transaction_id: &str,
        order_id: Option<&str>,
        mandate_jti: Option<&str>,
        body_json_bytes: &[u8],
    ) -> Result<SignedReceipt, SigningError> {
        let body_digest = body_sha256(body_json_bytes);
        let claims = ReceiptClaims {
            iss: iss_handler_url.to_string(),
            aud: aud_agent_id.to_string(),
            iat: Utc::now().timestamp(),
            jti: format!("rcpt_{}", uuid::Uuid::new_v4().simple()),
            icp: ReceiptIcp {
                version: crate::constants::ICP_VERSION.to_string(),
                intent: intent.to_string(),
                transaction_id: transaction_id.to_string(),
                order_id: order_id.map(str::to_string),
                mandate_jti: mandate_jti.map(str::to_string),
                body_digest: format!("sha256:{body_digest}"),
                body_canonicalization: "jcs".to_string(),
            },
        };

        let header = JwsHeader {
            alg: "EdDSA".to_string(),
            typ: "JWT".to_string(),
            kid: self.kid.clone(),
        };

        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
        let payload_b64 = URL_SAFE_NO_PAD.encode(
            serde_jcs::to_vec(&claims)
                .map_err(|e| SigningError::Canonicalization(e.to_string()))?,
        );
        let signing_input = format!("{header_b64}.{payload_b64}");
        let signature: Signature = self.signing_key.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        Ok(SignedReceipt {
            jti: claims.jti.clone(),
            kid: self.kid.clone(),
            jws: format!("{signing_input}.{sig_b64}"),
            body_digest: format!("sha256:{body_digest}"),
            claims,
        })
    }
}

pub struct SignedReceipt {
    pub jti: String,
    pub kid: String,
    pub jws: String,
    pub body_digest: String,
    pub claims: ReceiptClaims,
}

pub fn body_sha256(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    hex::encode(hasher.finalize())
}

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("canonicalization failed: {0}")]
    Canonicalization(String),
    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
}
