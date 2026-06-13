//! `icp-conformance` — an implementation-independent ICP conformance
//! tester.
//!
//! Points at any ICP handler URL and validates its HTTP surface against
//! the spec (see `docs/specification/ICP_SPEC.md`). Deliberately does
//! **not** import anything from the `stateset-icp-handler` library — all
//! types are re-derived from wire observations so passing this suite is
//! evidence that a *different* handler conforms, not that it matches
//! our internals.
//!
//! Usage:
//!
//! ```text
//! icp-conformance --url http://localhost:8082 \
//!                 --api-key icp_demo_key_123 \
//!                 --agent-id did:stateset:agent:conformance
//! ```
//!
//! Optional: `--mandate <compact-jws>` to exercise the mandate-gated
//! scopes (`quote`, `buy`, `return`). Without a mandate, those tests
//! are marked `SKIPPED` with a reason.
//!
//! Exit code is `0` if every test passes, `1` otherwise.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// ICP spec version this conformance suite targets, sent as the
/// `ICP-Version` request header (the handler requires it on every intent
/// request for version negotiation). Deliberately a local constant rather
/// than an import from `stateset-icp-handler` — see the module docs: the
/// suite re-derives everything from the spec/wire so passing it is
/// evidence a *foreign* handler conforms, not that it shares our types.
/// Bump this when testing against a newer spec revision.
const SPEC_ICP_VERSION: &str = "2026-04-21";

// --------------------------------------------------------------------------
// CLI
// --------------------------------------------------------------------------

struct Args {
    url: String,
    api_key: String,
    agent_id: String,
    mandate: Option<String>,
    currency: String,
    sku: String,
    verbose: bool,
}

fn print_help() {
    println!(
        r#"icp-conformance — independent conformance tester for ICP handlers

USAGE:
    icp-conformance --url <URL> --api-key <KEY> --agent-id <DID> [options]

REQUIRED:
    --url <URL>            Base URL of the handler (e.g. http://localhost:8082)
    --api-key <KEY>        Tenant bearer API key
    --agent-id <DID>       Agent identifier (DID, e.g. did:stateset:agent:x)

OPTIONAL:
    --mandate <JWS>        Compact-JWS mandate for scope-gated tests
    --currency <CCY>       Currency for quote tests (default: USD)
    --sku <SKU>            SKU to quote (default: WIDGET-001)
    --verbose              Print every response body on failure
    --help, -h             Show this help

Exit code is 0 if every test passes, 1 otherwise."#
    );
}

fn parse_args() -> Result<Args, String> {
    let mut url = None;
    let mut api_key = None;
    let mut agent_id = None;
    let mut mandate = None;
    let mut currency = "USD".to_string();
    let mut sku = "WIDGET-001".to_string();
    let mut verbose = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => url = it.next(),
            "--api-key" => api_key = it.next(),
            "--agent-id" => agent_id = it.next(),
            "--mandate" => mandate = it.next(),
            "--currency" => currency = it.next().ok_or("--currency needs a value")?,
            "--sku" => sku = it.next().ok_or("--sku needs a value")?,
            "--verbose" | "-v" => verbose = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        url: url
            .ok_or("--url required")?
            .trim_end_matches('/')
            .to_string(),
        api_key: api_key.ok_or("--api-key required")?,
        agent_id: agent_id.ok_or("--agent-id required")?,
        mandate,
        currency,
        sku,
        verbose,
    })
}

// --------------------------------------------------------------------------
// Result tracking
// --------------------------------------------------------------------------

#[derive(Debug)]
enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
}

struct TestResult {
    name: &'static str,
    outcome: Outcome,
    duration: Duration,
}

struct Runner {
    client: reqwest::Client,
    args: Args,
    results: Vec<TestResult>,
}

impl Runner {
    fn new(args: Args) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("build reqwest"),
            args,
            results: Vec::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.args.url, path)
    }

    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "Authorization",
            format!("Bearer {}", self.args.api_key).parse().unwrap(),
        );
        h.insert("ICP-Agent-Id", self.args.agent_id.parse().unwrap());
        // The handler requires ICP-Version on every intent request (spec
        // version negotiation). Send the spec version this suite targets.
        h.insert("ICP-Version", SPEC_ICP_VERSION.parse().unwrap());
        h.insert("Content-Type", "application/json".parse().unwrap());
        if let Some(m) = self.args.mandate.as_deref() {
            h.insert("ICP-Mandate", m.parse().unwrap());
        }
        h
    }
}

// --------------------------------------------------------------------------
// Assertions
// --------------------------------------------------------------------------

fn require_field(v: &Value, key: &str) -> Result<Value, String> {
    v.get(key)
        .cloned()
        .ok_or_else(|| format!("missing field `{key}` in {}", short_json(v)))
}

fn require_str(v: &Value, key: &str) -> Result<String, String> {
    let val = require_field(v, key)?;
    val.as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("`{key}` not a string (got {})", short_json(&val)))
}

fn short_json(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 200 {
        format!("{}…", &s[..200])
    } else {
        s
    }
}

// --------------------------------------------------------------------------
// Individual tests
// --------------------------------------------------------------------------

async fn test_discovery(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .get(r.url("/.well-known/icp"))
        .send()
        .await
        .map_err(|e| format!("GET /.well-known/icp: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }

    let version = resp
        .headers()
        .get("icp-version")
        .ok_or("response missing ICP-Version header")?
        .to_str()
        .map_err(|_| "ICP-Version not utf-8")?
        .to_string();

    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    require_str(&body, "icp_version")?;
    let intents = require_field(&body, "intents")?;
    let intents_arr = intents.as_array().ok_or("`intents` not array")?;
    if intents_arr.is_empty() {
        return Err("discovery `intents` array is empty".into());
    }

    // The header and the body-level version must match.
    if body["icp_version"].as_str() != Some(version.as_str()) {
        return Err(format!(
            "discovery body.icp_version `{}` != ICP-Version header `{}`",
            body["icp_version"], version
        ));
    }

    let keys = body
        .get("signing_keys")
        .and_then(|v| v.as_array())
        .ok_or("missing signing_keys array")?;
    if keys.is_empty() {
        return Err("signing_keys array is empty".into());
    }
    Ok(None)
}

async fn test_jwks(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .get(r.url("/.well-known/icp/jwks.json"))
        .send()
        .await
        .map_err(|e| format!("GET jwks: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let keys = body
        .get("keys")
        .and_then(|v| v.as_array())
        .ok_or("jwks missing `keys` array")?;
    if keys.is_empty() {
        return Err("jwks `keys` array empty".into());
    }
    for (i, k) in keys.iter().enumerate() {
        let alg = require_str(k, "alg")?;
        if alg != "EdDSA" {
            return Err(format!("keys[{i}].alg = {alg} (expected EdDSA)"));
        }
        require_str(k, "kid")?;
        let x = require_str(k, "x")?;
        if URL_SAFE_NO_PAD.decode(&x).map_err(|e| e.to_string())?.len() != 32 {
            return Err(format!("keys[{i}].x does not decode to 32 bytes"));
        }
    }
    Ok(None)
}

async fn test_health(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .get(r.url("/health"))
        .send()
        .await
        .map_err(|e| format!("GET /health: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let status = require_str(&body, "status")?;
    if status != "healthy" {
        return Err(format!("/health.status = {status}"));
    }
    require_str(&body, "icp_version")?;
    Ok(None)
}

async fn test_icp_version_header_on_every_response(r: &Runner) -> Result<Option<String>, String> {
    for path in ["/health", "/.well-known/icp", "/ready"] {
        let resp = r
            .client
            .get(r.url(path))
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        if resp.headers().get("icp-version").is_none() {
            return Err(format!("{path} response is missing ICP-Version header"));
        }
    }
    Ok(None)
}

async fn test_intent_quote(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.quote",
                "agent_id": r.args.agent_id,
                "params": {
                    "items": [
                        { "sku": r.args.sku, "quantity": 1,
                          "unit_price_hint": { "amount_minor": 1000, "currency": r.args.currency } }
                    ]
                },
                "context": { "currency": r.args.currency }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("POST intents: {e}"))?;

    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;

    if status == 401
        && body
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(|v| v.as_str())
            == Some("mandate_invalid")
        && r.args.mandate.is_none()
    {
        return Ok(Some(
            "handler requires mandate; supply --mandate to run".into(),
        ));
    }
    if !status.is_success() {
        return Err(format!(
            "status {status}; body = {}",
            if r.args.verbose {
                body.to_string()
            } else {
                short_json(&body)
            }
        ));
    }

    require_str(&body, "intent")?;
    let txn = require_field(&body, "transaction")?;
    let state = require_str(&txn, "state")?;
    if state != "quoted" {
        return Err(format!("expected txn.state=quoted, got {state}"));
    }
    require_str(&txn, "id")?;
    let receipt = require_field(&body, "receipt")?;
    let jti = require_str(&receipt, "jti")?;
    if !jti.starts_with("rcpt_") {
        return Err(format!("receipt.jti `{jti}` missing rcpt_ prefix"));
    }
    require_str(&receipt, "jws")?;
    require_str(&receipt, "body_digest")?;
    Ok(None)
}

async fn test_receipt_body_digest_matches_jcs(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.quote",
                "agent_id": r.args.agent_id,
                "params": {
                    "items": [
                        { "sku": r.args.sku, "quantity": 1,
                          "unit_price_hint": { "amount_minor": 500, "currency": r.args.currency } }
                    ]
                },
                "context": { "currency": r.args.currency }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("POST: {e}"))?;
    if resp.status() == 401 && r.args.mandate.is_none() {
        return Ok(Some("requires --mandate".into()));
    }
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let advertised = require_str(&body["receipt"], "body_digest")?;

    // Rebuild the canonical bytes the server signed: the response body
    // with the receipt stub cleared, then JCS-encoded + SHA-256.
    let mut unsigned = body.clone();
    unsigned["receipt"] = json!({
        "jti": "",
        "kid": body["receipt"]["kid"],
        "jws": "",
        "body_digest": "",
    });
    let canonical = serde_jcs::to_vec(&unsigned).map_err(|e| format!("jcs: {e}"))?;
    let mut h = Sha256::new();
    h.update(&canonical);
    let computed = format!("sha256:{}", hex::encode(h.finalize()));

    if computed != advertised {
        return Err(format!(
            "body_digest mismatch: computed {computed}, advertised {advertised}"
        ));
    }
    Ok(None)
}

async fn test_receipt_signature_verifies(r: &Runner) -> Result<Option<String>, String> {
    // 1. Fetch JWKS and index by kid.
    let jwks: Value = r
        .client
        .get(r.url("/.well-known/icp/jwks.json"))
        .send()
        .await
        .map_err(|e| format!("jwks: {e}"))?
        .json()
        .await
        .map_err(|e| format!("jwks json: {e}"))?;

    // 2. Produce a receipt by quoting.
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.quote",
                "agent_id": r.args.agent_id,
                "params": {
                    "items": [
                        { "sku": r.args.sku, "quantity": 1,
                          "unit_price_hint": { "amount_minor": 750, "currency": r.args.currency } }
                    ]
                },
                "context": { "currency": r.args.currency }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("intent: {e}"))?;
    if resp.status() == 401 && r.args.mandate.is_none() {
        return Ok(Some("requires --mandate".into()));
    }
    if !resp.status().is_success() {
        return Err(format!("quote status {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let jws = require_str(&body["receipt"], "jws")?;
    let receipt_kid = require_str(&body["receipt"], "kid")?;

    let key_entry = jwks["keys"]
        .as_array()
        .ok_or("jwks.keys missing")?
        .iter()
        .find(|k| k["kid"].as_str() == Some(receipt_kid.as_str()))
        .ok_or_else(|| format!("receipt kid `{receipt_kid}` not present in JWKS"))?;
    let x_b64 = key_entry["x"].as_str().ok_or("jwks entry missing `x`")?;
    let x_bytes = URL_SAFE_NO_PAD.decode(x_b64).map_err(|e| e.to_string())?;
    let x_array: [u8; 32] = x_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "`x` is not 32 bytes".to_string())?;
    let vk = VerifyingKey::from_bytes(&x_array).map_err(|e| e.to_string())?;

    // 3. Verify the compact JWS.
    let mut parts = jws.split('.');
    let h = parts.next().ok_or("jws missing header")?;
    let p = parts.next().ok_or("jws missing payload")?;
    let s = parts.next().ok_or("jws missing signature")?;
    if parts.next().is_some() {
        return Err("jws has extra segments".into());
    }
    let sig_bytes = URL_SAFE_NO_PAD.decode(s).map_err(|e| e.to_string())?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "signature not 64 bytes".to_string())?;
    let sig = Signature::from_bytes(&sig_array);
    let signing_input = format!("{h}.{p}");
    vk.verify(signing_input.as_bytes(), &sig)
        .map_err(|e| format!("signature verify: {e}"))?;

    // 4. Inspect claims for spec-required fields.
    let payload_bytes = URL_SAFE_NO_PAD.decode(p).map_err(|e| e.to_string())?;
    let claims: Value =
        serde_json::from_slice(&payload_bytes).map_err(|e| format!("claims json: {e}"))?;
    for k in ["iss", "aud", "iat", "jti"] {
        if claims.get(k).is_none() {
            return Err(format!("receipt claims missing `{k}`"));
        }
    }
    let icp = require_field(&claims, "icp")?;
    for k in [
        "version",
        "intent",
        "transaction_id",
        "body_digest",
        "body_canonicalization",
    ] {
        if icp.get(k).is_none() {
            return Err(format!("claims.icp missing `{k}`"));
        }
    }
    Ok(None)
}

async fn test_error_shape_on_unknown_intent(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.definitely_not_real",
                "agent_id": r.args.agent_id
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("POST: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    if status.as_u16() != 404 {
        return Err(format!("expected 404, got {status}"));
    }
    let err = require_field(&body, "error")?;
    let type_ = require_str(&err, "type")?;
    if type_ != "intent_not_supported" {
        return Err(format!(
            "expected error.type=intent_not_supported, got {type_}"
        ));
    }
    for k in ["code", "message", "retriable"] {
        require_field(&err, k)?;
    }
    Ok(None)
}

async fn test_error_shape_on_missing_auth(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .header("Content-Type", "application/json")
        .body(
            json!({
                "intent": "intent.quote",
                "agent_id": r.args.agent_id,
                "params": { "items": [] }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("POST: {e}"))?;
    if resp.status().as_u16() != 401 {
        return Err(format!("expected 401, got {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let type_ = require_str(&body["error"], "type")?;
    if type_ != "authentication_failed" {
        return Err(format!("expected authentication_failed, got {type_}"));
    }
    Ok(None)
}

async fn test_intent_lifecycle(r: &Runner) -> Result<Option<String>, String> {
    if r.args.mandate.is_none() {
        return Ok(Some("requires --mandate for authorize + buy".into()));
    }

    // Quote
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.quote",
                "agent_id": r.args.agent_id,
                "params": {
                    "items": [
                        { "sku": r.args.sku, "quantity": 1,
                          "unit_price_hint": { "amount_minor": 1500, "currency": r.args.currency } }
                    ],
                    "buyer": { "email": "conformance@example.com" }
                },
                "context": { "currency": r.args.currency }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("quote: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("quote status {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let txn_id = require_str(&body["transaction"], "id")?;

    // Authorize
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.authorize",
                "agent_id": r.args.agent_id,
                "params": { "transaction_id": txn_id }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("authorize: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("authorize status {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    if body["transaction"]["state"] != "authorized" {
        return Err(format!(
            "post-authorize state != authorized: {}",
            body["transaction"]["state"]
        ));
    }

    // Buy
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.buy",
                "agent_id": r.args.agent_id,
                "params": {
                    "transaction_id": txn_id,
                    "payment": { "method": "card", "token": "tok_conformance" }
                }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("buy: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("buy status {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    if body["transaction"]["state"] != "completed" {
        return Err(format!(
            "post-buy state != completed: {}",
            body["transaction"]["state"]
        ));
    }
    Ok(None)
}

async fn test_receipts_retrievable(r: &Runner) -> Result<Option<String>, String> {
    // Produce a receipt via quote, then fetch it by jti.
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.quote",
                "agent_id": r.args.agent_id,
                "params": {
                    "items": [
                        { "sku": r.args.sku, "quantity": 1,
                          "unit_price_hint": { "amount_minor": 200, "currency": r.args.currency } }
                    ]
                }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("quote: {e}"))?;
    if resp.status() == 401 && r.args.mandate.is_none() {
        return Ok(Some("requires --mandate".into()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let jti = require_str(&body["receipt"], "jti")?;

    let resp = r
        .client
        .get(r.url(&format!("/icp/v1/receipts/{jti}")))
        .headers(r.auth_headers())
        .send()
        .await
        .map_err(|e| format!("fetch receipt: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("receipt fetch status {}", resp.status()));
    }
    let stored: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let stored_jti = require_str(&stored, "jti")?;
    if stored_jti != jti {
        return Err(format!("retrieved jti `{stored_jti}` != expected `{jti}`"));
    }
    require_str(&stored, "jws")?;
    let digest = require_str(&stored, "body_digest")?;
    if !digest.starts_with("sha256:") {
        return Err(format!(
            "body_digest `{digest}` does not start with sha256:"
        ));
    }
    Ok(None)
}

async fn test_transaction_retrievable(r: &Runner) -> Result<Option<String>, String> {
    let resp = r
        .client
        .post(r.url("/icp/v1/intents"))
        .headers(r.auth_headers())
        .body(
            json!({
                "intent": "intent.quote",
                "agent_id": r.args.agent_id,
                "params": {
                    "items": [
                        { "sku": r.args.sku, "quantity": 1,
                          "unit_price_hint": { "amount_minor": 300, "currency": r.args.currency } }
                    ]
                }
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|e| format!("quote: {e}"))?;
    if resp.status() == 401 && r.args.mandate.is_none() {
        return Ok(Some("requires --mandate".into()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let txn_id = require_str(&body["transaction"], "id")?;

    let resp = r
        .client
        .get(r.url(&format!("/icp/v1/transactions/{txn_id}")))
        .headers(r.auth_headers())
        .send()
        .await
        .map_err(|e| format!("fetch txn: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("txn fetch status {}", resp.status()));
    }
    let stored: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    if stored["id"] != txn_id {
        return Err(format!(
            "retrieved id `{}` != expected `{txn_id}`",
            stored["id"]
        ));
    }
    Ok(None)
}

async fn test_discovery_intents_are_live(r: &Runner) -> Result<Option<String>, String> {
    // Every intent advertised in discovery MUST either succeed with
    // well-formed output OR return an in-catalog error type. It MUST NOT
    // return `intent_not_supported` — that would mean the discovery is
    // lying about its catalog.
    let discovery: Value = r
        .client
        .get(r.url("/.well-known/icp"))
        .send()
        .await
        .map_err(|e| format!("discovery: {e}"))?
        .json()
        .await
        .map_err(|e| format!("json: {e}"))?;
    let intents = discovery["intents"]
        .as_array()
        .ok_or("no intents array")?
        .clone();

    for intent_name in intents {
        let name = intent_name.as_str().ok_or("intent name not string")?;
        let resp = r
            .client
            .post(r.url("/icp/v1/intents"))
            .headers(r.auth_headers())
            .body(
                json!({
                    "intent": name,
                    "agent_id": r.args.agent_id,
                    "params": {}
                })
                .to_string(),
            )
            .send()
            .await
            .map_err(|e| format!("POST {name}: {e}"))?;
        // We accept any status; we only care whether the error *type*
        // says this intent isn't in the catalog.
        if let Ok(body) = resp.json::<Value>().await {
            if body["error"]["type"] == "intent_not_supported" {
                return Err(format!(
                    "discovery advertises {name} but handler returns intent_not_supported"
                ));
            }
        }
    }
    Ok(None)
}

// --------------------------------------------------------------------------
// Report
// --------------------------------------------------------------------------

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn print_report(results: &[TestResult]) -> (usize, usize, usize) {
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    for r in results {
        let (tag, color, detail) = match &r.outcome {
            Outcome::Pass => {
                passed += 1;
                ("PASS", GREEN, String::new())
            }
            Outcome::Fail(msg) => {
                failed += 1;
                ("FAIL", RED, format!(" — {msg}"))
            }
            Outcome::Skip(reason) => {
                skipped += 1;
                ("SKIP", YELLOW, format!(" — {reason}"))
            }
        };
        println!(
            "  {color}{tag}{RESET}  {:50}  {DIM}{:>7.2}ms{RESET}{detail}",
            r.name,
            r.duration.as_secs_f64() * 1000.0,
        );
    }
    (passed, failed, skipped)
}

// --------------------------------------------------------------------------
// Main
// --------------------------------------------------------------------------

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            return ExitCode::from(2);
        }
    };

    println!();
    println!("  {DIM}icp-conformance{RESET}  against {}", args.url);
    println!("  {DIM}agent_id{RESET}        {}", args.agent_id);
    println!(
        "  {DIM}mandate{RESET}         {}",
        if args.mandate.is_some() {
            "provided"
        } else {
            "none (scope-gated tests will skip)"
        }
    );
    println!();

    let mut runner = Runner::new(args);

    // Inline each test through a local macro — a generic `run` helper
    // runs into the well-known HRTB issue for `async fn(&Runner)`.
    macro_rules! run_test {
        ($name:expr, $test:ident) => {{
            let started = Instant::now();
            let outcome = match $test(&runner).await {
                Ok(None) => Outcome::Pass,
                Ok(Some(r)) => Outcome::Skip(r),
                Err(r) => Outcome::Fail(r),
            };
            runner.results.push(TestResult {
                name: $name,
                outcome,
                duration: started.elapsed(),
            });
        }};
    }

    run_test!("discovery document", test_discovery);
    run_test!("jwks well-formed", test_jwks);
    run_test!("health endpoint", test_health);
    run_test!(
        "ICP-Version header on every response",
        test_icp_version_header_on_every_response
    );
    run_test!(
        "discovery intents all live (no intent_not_supported)",
        test_discovery_intents_are_live
    );
    run_test!("intent.quote happy path", test_intent_quote);
    run_test!(
        "receipt body_digest matches JCS of response",
        test_receipt_body_digest_matches_jcs
    );
    run_test!(
        "receipt Ed25519 signature verifies against JWKS",
        test_receipt_signature_verifies
    );
    run_test!(
        "transaction retrievable by id",
        test_transaction_retrievable
    );
    run_test!("receipt retrievable by jti", test_receipts_retrievable);
    run_test!(
        "error shape on unknown intent",
        test_error_shape_on_unknown_intent
    );
    run_test!(
        "error shape on missing auth",
        test_error_shape_on_missing_auth
    );
    run_test!("quote→authorize→buy lifecycle", test_intent_lifecycle);

    println!();
    let (passed, failed, skipped) = print_report(&runner.results);
    let total = runner.results.len();
    println!();
    println!(
        "  {}{}/{} passed{RESET}  {}{} failed{RESET}  {}{} skipped{RESET}",
        if failed == 0 { GREEN } else { RED },
        passed,
        total,
        if failed > 0 { RED } else { DIM },
        failed,
        if skipped > 0 { YELLOW } else { DIM },
        skipped,
    );
    println!();

    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
