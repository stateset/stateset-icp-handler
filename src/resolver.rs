//! Principal DID resolution.
//!
//! A [`PrincipalResolver`] maps a Decentralized Identifier (DID) to the
//! set of verifying keys the principal controls. The mandate verifier
//! consults the resolver to locate the correct key for a given JWS
//! header `kid`, then verifies the Ed25519 signature over the compact
//! serialization.
//!
//! # DID methods supported
//!
//! ## `did:key:z6Mk...`
//!
//! Fully self-contained: the public key is *embedded in the DID itself*
//! using multibase/multicodec encoding. No network resolution required.
//! Ed25519 keys use multicodec prefix `0xed 0x01`, yielding 34 bytes
//! after base58btc decoding (2 prefix + 32 key). See [W3C did:key spec].
//!
//! ## `did:web:example.com[:path:segments]`
//!
//! Dereferences to an HTTPS URL serving a W3C DID document, then
//! extracts Ed25519 verifying keys from `verificationMethod` entries.
//! Supports both `publicKeyMultibase` (z6Mk… form) and `publicKeyJwk`
//! (`{kty:OKP, crv:Ed25519, x:…}`) encodings. URL mapping per spec:
//!
//! ```text
//! did:web:example.com               → https://example.com/.well-known/did.json
//! did:web:example.com:users:alice   → https://example.com/users/alice/did.json
//! ```
//!
//! Results are TTL-cached (default 10 minutes) to avoid hammering
//! external endpoints. The HTTP base scheme is configurable so test
//! harnesses can use `http://` against a local mock server.
//!
//! ## Other (`did:stateset:buyer:…`)
//!
//! Pluggable via the [`PrincipalResolver`] trait. Deployments can
//! register custom resolvers and compose them with [`CompositeResolver`].
//!
//! [W3C did:key spec]: https://w3c-ccg.github.io/did-method-key/

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("unsupported DID method: `{0}`")]
    UnsupportedMethod(String),
    #[error("malformed DID: {0}")]
    MalformedDid(String),
    #[error("key extraction failed: {0}")]
    KeyExtraction(String),
    #[error("network error fetching `{url}`: {source}")]
    Network {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("DID document at `{url}` malformed: {message}")]
    DidDocument { url: String, message: String },
    #[error("refusing to resolve `{url}`: {message}")]
    Blocked { url: String, message: String },
}

/// A verifying key advertised by a principal, suitable for JWS
/// verification.
#[derive(Debug, Clone)]
pub struct VerifyingKeyEntry {
    /// Optional `kid` the principal advertises for this key. For
    /// `did:key` the kid is canonically the DID itself; for `did:web`
    /// it's the `verificationMethod.id`.
    pub kid: Option<String>,
    /// Algorithm name as it would appear in a JWS header.
    pub alg: &'static str,
    pub key: VerifyingKey,
}

/// Resolves a principal DID to its verifying keys.
///
/// Async because some DID methods (notably `did:web`) require HTTP I/O.
#[async_trait]
pub trait PrincipalResolver: Send + Sync {
    async fn resolve(&self, did: &str) -> Result<Vec<VerifyingKeyEntry>, ResolveError>;
}

// --------------------------------------------------------------------------
// did:key
// --------------------------------------------------------------------------

/// Resolves `did:key:z…` URIs by decoding the embedded key.
pub struct DidKeyResolver;

#[async_trait]
impl PrincipalResolver for DidKeyResolver {
    async fn resolve(&self, did: &str) -> Result<Vec<VerifyingKeyEntry>, ResolveError> {
        let fingerprint = did
            .strip_prefix("did:key:")
            .ok_or_else(|| ResolveError::UnsupportedMethod(did.to_string()))?;
        let key = decode_did_key(fingerprint)?;
        Ok(vec![VerifyingKeyEntry {
            kid: Some(did.to_string()),
            alg: "EdDSA",
            key,
        }])
    }
}

/// Decode the fingerprint portion of a `did:key`. Returns an Ed25519
/// verifying key or an error if the DID is not Ed25519.
pub fn decode_did_key(fingerprint: &str) -> Result<VerifyingKey, ResolveError> {
    let rest = fingerprint
        .strip_prefix('z')
        .ok_or_else(|| ResolveError::MalformedDid("expected multibase `z` prefix".into()))?;
    let bytes = bs58::decode(rest)
        .into_vec()
        .map_err(|e| ResolveError::MalformedDid(format!("base58btc: {e}")))?;
    if bytes.len() != 34 {
        return Err(ResolveError::MalformedDid(format!(
            "expected 34 bytes (2 multicodec + 32 key), got {}",
            bytes.len()
        )));
    }
    if bytes[0] != 0xed || bytes[1] != 0x01 {
        return Err(ResolveError::UnsupportedMethod(format!(
            "only Ed25519 (multicodec 0xed 0x01) supported in did:key, got 0x{:02x} 0x{:02x}",
            bytes[0], bytes[1]
        )));
    }
    let key_bytes: [u8; 32] = bytes[2..]
        .try_into()
        .map_err(|_| ResolveError::KeyExtraction("slice→array".into()))?;
    VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| ResolveError::KeyExtraction(format!("ed25519-dalek: {e}")))
}

/// Encode an Ed25519 verifying key as a `did:key` URI (test helper).
pub fn encode_did_key(key: &VerifyingKey) -> String {
    let key_bytes = key.to_bytes();
    let mut body = Vec::with_capacity(34);
    body.push(0xed);
    body.push(0x01);
    body.extend_from_slice(&key_bytes);
    let b58 = bs58::encode(body).into_string();
    format!("did:key:z{b58}")
}

/// Encode an Ed25519 verifying key as a `publicKeyMultibase` value
/// (i.e. the `z…` part with no `did:key:` prefix).
pub fn encode_multibase_key(key: &VerifyingKey) -> String {
    encode_did_key(key)
        .strip_prefix("did:key:")
        .expect("encode_did_key always produces did:key: prefix")
        .to_string()
}

// --------------------------------------------------------------------------
// did:web
// --------------------------------------------------------------------------

/// Resolves `did:web:host[:path-segments]` URIs by HTTP-fetching the
/// W3C DID document and extracting Ed25519 verifying keys.
///
/// Cache TTL controls how often the same DID is re-fetched. A TTL of
/// `Duration::ZERO` disables caching (useful for tests).
pub struct DidWebResolver {
    client: reqwest::Client,
    /// Scheme to use when constructing fetch URLs. Defaults to
    /// `"https"`; tests override with `"http"`.
    scheme: String,
    cache: RwLock<HashMap<String, CachedKeyset>>,
    cache_ttl: Duration,
    /// When false (production default), the resolver refuses to fetch a
    /// `did:web` document whose host resolves to localhost or a private
    /// network — the same SSRF gate the webhook worker applies. The
    /// `iss` of a mandate is attacker-controlled, so without this a
    /// caller could make the handler issue arbitrary internal GETs.
    allow_insecure: bool,
}

struct CachedKeyset {
    fetched_at: DateTime<Utc>,
    keys: Vec<VerifyingKeyEntry>,
}

impl DidWebResolver {
    pub fn new() -> Self {
        Self::with_options(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("build reqwest client"),
            "https",
            Duration::from_secs(600),
        )
    }

    pub fn with_options(client: reqwest::Client, scheme: &str, cache_ttl: Duration) -> Self {
        // A non-https scheme only ever appears in development/test wiring
        // (production fetches `did:web` over https), so treat it as the
        // insecure-allowed mode — this lets the existing http-based tests
        // resolve against a localhost mock without tripping the guard.
        let allow_insecure = !scheme.eq_ignore_ascii_case("https");
        Self {
            client,
            scheme: scheme.to_string(),
            cache: RwLock::new(HashMap::new()),
            cache_ttl,
            allow_insecure,
        }
    }

    /// Override the SSRF guard explicitly. Production wiring passes the
    /// handler's `allow_insecure_urls` flag here so the resolver and the
    /// webhook worker honor the same operator setting.
    pub fn allowing_insecure(mut self, allow: bool) -> Self {
        self.allow_insecure = allow;
        self
    }

    fn url_for(&self, did: &str) -> Result<String, ResolveError> {
        let body = did
            .strip_prefix("did:web:")
            .ok_or_else(|| ResolveError::UnsupportedMethod(did.to_string()))?;
        if body.is_empty() {
            return Err(ResolveError::MalformedDid("did:web: empty body".into()));
        }
        let mut parts = body.split(':');
        // First segment is host (URL-decode percent-encoded port).
        let host = parts.next().expect("split has at least one element");
        let host_decoded = host.replace("%3A", ":");
        let path: Vec<&str> = parts.collect();
        let path_part = if path.is_empty() {
            "/.well-known/did.json".to_string()
        } else {
            format!("/{}/did.json", path.join("/"))
        };
        Ok(format!("{}://{}{}", self.scheme, host_decoded, path_part))
    }

    fn cached(&self, did: &str) -> Option<Vec<VerifyingKeyEntry>> {
        if self.cache_ttl.is_zero() {
            return None;
        }
        let guard = self.cache.read().ok()?;
        let entry = guard.get(did)?;
        let age = Utc::now() - entry.fetched_at;
        let ttl = chrono::Duration::from_std(self.cache_ttl).ok()?;
        if age <= ttl {
            Some(entry.keys.clone())
        } else {
            None
        }
    }

    fn store(&self, did: &str, keys: &[VerifyingKeyEntry]) {
        if self.cache_ttl.is_zero() {
            return;
        }
        if let Ok(mut guard) = self.cache.write() {
            guard.insert(
                did.to_string(),
                CachedKeyset {
                    fetched_at: Utc::now(),
                    keys: keys.to_vec(),
                },
            );
        }
    }
}

impl Default for DidWebResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PrincipalResolver for DidWebResolver {
    async fn resolve(&self, did: &str) -> Result<Vec<VerifyingKeyEntry>, ResolveError> {
        if let Some(keys) = self.cached(did) {
            return Ok(keys);
        }
        let url = self.url_for(did)?;
        if !self.allow_insecure {
            crate::webhook::url::ensure_public_url(&url)
                .await
                .map_err(|message| ResolveError::Blocked {
                    url: url.clone(),
                    message,
                })?;
        }
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/did+json, application/json")
            .send()
            .await
            .map_err(|e| ResolveError::Network {
                url: url.clone(),
                source: Box::new(e),
            })?;
        if !resp.status().is_success() {
            return Err(ResolveError::DidDocument {
                url: url.clone(),
                message: format!("status {}", resp.status()),
            });
        }
        let doc: DidDocument = resp.json().await.map_err(|e| ResolveError::DidDocument {
            url: url.clone(),
            message: format!("json: {e}"),
        })?;
        let keys = parse_verification_methods(&doc, did, &url)?;
        if keys.is_empty() {
            return Err(ResolveError::DidDocument {
                url,
                message: "DID document advertises no Ed25519 verification methods".into(),
            });
        }
        self.store(did, &keys);
        Ok(keys)
    }
}

#[derive(Debug, Deserialize)]
struct DidDocument {
    #[serde(default)]
    verification_method: Vec<VerificationMethod>,
    #[serde(default, rename = "verificationMethod")]
    verification_method_camel: Vec<VerificationMethod>,
}

#[derive(Debug, Deserialize)]
struct VerificationMethod {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    type_: Option<String>,
    #[serde(default)]
    public_key_multibase: Option<String>,
    #[serde(default, rename = "publicKeyMultibase")]
    public_key_multibase_camel: Option<String>,
    #[serde(default)]
    public_key_jwk: Option<Jwk>,
    #[serde(default, rename = "publicKeyJwk")]
    public_key_jwk_camel: Option<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    #[serde(default)]
    kty: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
}

fn parse_verification_methods(
    doc: &DidDocument,
    did: &str,
    url: &str,
) -> Result<Vec<VerifyingKeyEntry>, ResolveError> {
    let mut out = Vec::new();
    let methods = doc
        .verification_method
        .iter()
        .chain(doc.verification_method_camel.iter());
    for vm in methods {
        // Multibase form takes precedence (smaller, simpler).
        let multibase = vm
            .public_key_multibase
            .as_deref()
            .or(vm.public_key_multibase_camel.as_deref());
        let jwk = vm
            .public_key_jwk
            .as_ref()
            .or(vm.public_key_jwk_camel.as_ref());

        let key = if let Some(mb) = multibase {
            match decode_did_key(mb) {
                Ok(k) => k,
                Err(_) => continue,
            }
        } else if let Some(jwk) = jwk {
            if jwk.kty.as_deref() != Some("OKP") || jwk.crv.as_deref() != Some("Ed25519") {
                continue;
            }
            let Some(x_b64) = jwk.x.as_deref() else {
                continue;
            };
            let bytes = URL_SAFE_NO_PAD
                .decode(x_b64)
                .map_err(|e| ResolveError::DidDocument {
                    url: url.to_string(),
                    message: format!("publicKeyJwk.x not base64url: {e}"),
                })?;
            let arr: [u8; 32] =
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| ResolveError::DidDocument {
                        url: url.to_string(),
                        message: "publicKeyJwk.x is not 32 bytes (Ed25519)".into(),
                    })?;
            VerifyingKey::from_bytes(&arr).map_err(|e| ResolveError::DidDocument {
                url: url.to_string(),
                message: format!("publicKeyJwk.x not a valid Ed25519 point: {e}"),
            })?
        } else {
            continue;
        };

        // Validate the type when supplied; tolerate missing.
        if let Some(t) = vm.type_.as_deref() {
            if !matches!(
                t,
                "Ed25519VerificationKey2020"
                    | "Ed25519VerificationKey2018"
                    | "JsonWebKey2020"
                    | "Multikey"
            ) {
                continue;
            }
        }
        out.push(VerifyingKeyEntry {
            kid: vm.id.clone().or_else(|| Some(did.to_string())),
            alg: "EdDSA",
            key,
        });
    }
    Ok(out)
}

// --------------------------------------------------------------------------
// Composite
// --------------------------------------------------------------------------

/// Try each registered resolver in order; return the first success.
pub struct CompositeResolver {
    resolvers: Vec<Box<dyn PrincipalResolver>>,
}

impl CompositeResolver {
    pub fn new() -> Self {
        Self {
            resolvers: Vec::new(),
        }
    }

    pub fn with(mut self, resolver: Box<dyn PrincipalResolver>) -> Self {
        self.resolvers.push(resolver);
        self
    }

    /// The default resolver set: `did:key` + `did:web` (HTTPS, SSRF-guarded).
    pub fn default_set() -> Self {
        Self::default_set_with_options(false)
    }

    /// The default resolver set with the `did:web` SSRF guard toggled to
    /// match the handler's `allow_insecure_urls` setting. Production passes
    /// `false`; development wiring may pass `true` to resolve against
    /// localhost registries.
    pub fn default_set_with_options(allow_insecure: bool) -> Self {
        Self::new().with(Box::new(DidKeyResolver)).with(Box::new(
            DidWebResolver::new().allowing_insecure(allow_insecure),
        ))
    }
}

impl Default for CompositeResolver {
    fn default() -> Self {
        Self::default_set()
    }
}

#[async_trait]
impl PrincipalResolver for CompositeResolver {
    async fn resolve(&self, did: &str) -> Result<Vec<VerifyingKeyEntry>, ResolveError> {
        let mut last_err: Option<ResolveError> = None;
        for r in &self.resolvers {
            match r.resolve(did).await {
                Ok(keys) => return Ok(keys),
                Err(e @ ResolveError::UnsupportedMethod(_)) => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| ResolveError::UnsupportedMethod(did.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[tokio::test]
    async fn did_key_round_trip() {
        let signing = SigningKey::generate(&mut OsRng);
        let verifying = signing.verifying_key();
        let did = encode_did_key(&verifying);
        assert!(did.starts_with("did:key:z"));

        let resolver = DidKeyResolver;
        let keys = resolver.resolve(&did).await.expect("resolve");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key.to_bytes(), verifying.to_bytes());
        assert_eq!(keys[0].alg, "EdDSA");
    }

    #[tokio::test]
    async fn did_key_rejects_did_web() {
        let resolver = DidKeyResolver;
        assert!(matches!(
            resolver.resolve("did:web:example.com").await,
            Err(ResolveError::UnsupportedMethod(_))
        ));
    }

    #[tokio::test]
    async fn did_web_url_construction() {
        let r =
            DidWebResolver::with_options(reqwest::Client::new(), "https", Duration::from_secs(60));
        assert_eq!(
            r.url_for("did:web:example.com").unwrap(),
            "https://example.com/.well-known/did.json"
        );
        assert_eq!(
            r.url_for("did:web:example.com:users:alice").unwrap(),
            "https://example.com/users/alice/did.json"
        );
        // Percent-encoded port in DID body.
        assert_eq!(
            r.url_for("did:web:localhost%3A8080").unwrap(),
            "https://localhost:8080/.well-known/did.json"
        );
    }

    #[tokio::test]
    async fn composite_falls_through_unsupported_to_last_resolver() {
        // Even with the default set (which includes did:web), a totally
        // unknown method like `did:nope:` should bubble UnsupportedMethod.
        let resolver = CompositeResolver::default_set();
        assert!(matches!(
            resolver.resolve("did:nope:foo").await,
            Err(ResolveError::UnsupportedMethod(_))
        ));
    }
}
