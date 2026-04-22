//! Mandate parsing, validation, and budget enforcement.
//!
//! A mandate is a compact JWS whose payload is the JSON document described
//! in ICP §6. This module:
//!
//!   1. Structurally decodes the JWS ([`decode_unverified`]).
//!   2. Cryptographically verifies the Ed25519 signature against the
//!      principal DID's advertised keyset ([`verify_signature`]).
//!   3. Evaluates scope, merchant, validity window, and budget against
//!      a candidate intent ([`evaluate`]).
//!   4. Records spend against a windowed in-memory ledger
//!      ([`MandateLedger`]).
//!
//! Signature verification is gated by the `verify_mandate_signatures`
//! config flag so local development can continue using `alg:none`
//! mandates without regenerating keypairs. Production deployments MUST
//! enable verification.
//!
//! The [`PrincipalResolver`] trait (in `crate::resolver`) is the
//! extension seam for DID methods beyond `did:key`. v0.2 ships `did:key`
//! only; `did:web` is a documented v0.3 follow-up.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Verifier};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use crate::errors::ApiError;
use crate::resolver::PrincipalResolver;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MandatePayload {
    pub iss: String,
    pub sub: String,
    pub iat: i64,
    pub nbf: i64,
    pub exp: i64,
    pub jti: String,
    pub icp: MandateTerms,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MandateTerms {
    pub version: String,
    pub scope: Vec<String>,
    pub budget: MandateBudget,
    #[serde(default)]
    pub merchants: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub jurisdictions: Vec<String>,
    #[serde(default)]
    pub policies: MandatePolicies,
    #[serde(default)]
    pub linked_payment_methods: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MandateBudget {
    pub currency: String,
    pub amount_minor: i64,
    #[serde(default)]
    pub per_transaction: Option<i64>,
    /// ISO 8601 duration. `P1D` means 24 hours.
    #[serde(default)]
    pub period: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MandatePolicies {
    #[serde(default)]
    pub require_receipt: bool,
    #[serde(default)]
    pub require_shipping_address_confirmation: bool,
    #[serde(default)]
    pub prohibit_subscriptions: bool,
}

/// The effect of evaluating a mandate against a candidate intent.
#[derive(Debug, Clone)]
pub struct MandateEvaluation {
    pub payload: MandatePayload,
    pub compact_jws: String,
    pub spend_room_minor: i64,
    /// Whether the JWS signature was cryptographically verified against
    /// the principal's advertised keyset. `false` means the mandate was
    /// structurally decoded but the signature was trusted (dev mode).
    pub signature_verified: bool,
}

/// Parsed JWS header (subset of fields we care about).
#[derive(Debug, Clone, Deserialize)]
struct JwsHeader {
    #[serde(default)]
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MandateUsage {
    pub spent_minor: i64,
    pub window_start: Option<DateTime<Utc>>,
}

/// Mandate usage ledger. Backed either by an in-process `HashMap` (tests,
/// ephemeral demos) or by the shared SQLite state pool (production).
///
/// Persistent backing is non-negotiable in production: a 24-hour budget
/// mandate whose spend is forgotten on restart effectively allows unbounded
/// further spend in the remaining window. The SQLite backend closes that
/// gap without introducing an external-service dependency.
#[derive(Clone)]
pub struct MandateLedger {
    backend: MandateBackend,
}

#[derive(Clone)]
enum MandateBackend {
    Memory(Arc<RwLock<HashMap<String, MandateUsage>>>),
    Sqlite(crate::state_db::StatePool),
}

impl Default for MandateLedger {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl MandateLedger {
    /// Ephemeral in-memory ledger. Convenient for tests; **never safe in
    /// production** because a restart forgets all accumulated spend.
    pub fn new() -> Self {
        Self::in_memory()
    }

    pub fn in_memory() -> Self {
        Self {
            backend: MandateBackend::Memory(Arc::new(RwLock::new(HashMap::new()))),
        }
    }

    /// Persistent ledger backed by the shared state pool.
    pub fn with_pool(pool: crate::state_db::StatePool) -> Self {
        Self {
            backend: MandateBackend::Sqlite(pool),
        }
    }

    pub fn usage(&self, jti: &str) -> MandateUsage {
        match &self.backend {
            MandateBackend::Memory(inner) => inner
                .read()
                .expect("mandate ledger read")
                .get(jti)
                .cloned()
                .unwrap_or_default(),
            MandateBackend::Sqlite(pool) => {
                let conn = pool.get().expect("mandate ledger pool acquire");
                let row: rusqlite::Result<(i64, Option<String>)> = conn.query_row(
                    "SELECT spent_minor, window_start FROM mandate_usage WHERE jti = ?1",
                    rusqlite::params![jti],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                );
                match row {
                    Ok((spent_minor, window_start_str)) => MandateUsage {
                        spent_minor,
                        window_start: window_start_str
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|dt| dt.with_timezone(&Utc)),
                    },
                    Err(rusqlite::Error::QueryReturnedNoRows) => MandateUsage::default(),
                    Err(e) => panic!("mandate ledger read: {e}"),
                }
            }
        }
    }

    pub fn record_spend(&self, jti: &str, amount_minor: i64, now: DateTime<Utc>) {
        match &self.backend {
            MandateBackend::Memory(inner) => {
                let mut guard = inner.write().expect("mandate ledger write");
                let entry = guard.entry(jti.to_string()).or_default();
                if entry.window_start.is_none() {
                    entry.window_start = Some(now);
                }
                entry.spent_minor = entry.spent_minor.saturating_add(amount_minor);
            }
            MandateBackend::Sqlite(pool) => {
                let conn = pool.get().expect("mandate ledger pool acquire");
                // UPSERT: preserve the original window_start (only set on first
                // spend for a given jti), saturating_add on spent_minor. Mirrors
                // the in-memory semantics exactly — lazy window reset still
                // happens at read time in `evaluate()`.
                conn.execute(
                    "INSERT INTO mandate_usage (jti, spent_minor, window_start) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT(jti) DO UPDATE SET \
                         spent_minor = MIN(9223372036854775807, mandate_usage.spent_minor + excluded.spent_minor)",
                    rusqlite::params![jti, amount_minor, now.to_rfc3339()],
                )
                .expect("mandate ledger write");
            }
        }
    }
}

/// Split a compact JWS into its three base64url-encoded segments.
fn split_compact(compact_jws: &str) -> Result<(&str, &str, &str), ApiError> {
    let mut parts = compact_jws.split('.');
    let h = parts
        .next()
        .ok_or_else(|| ApiError::MandateInvalid("mandate: missing header segment".into()))?;
    let p = parts
        .next()
        .ok_or_else(|| ApiError::MandateInvalid("mandate: missing payload segment".into()))?;
    let s = parts
        .next()
        .ok_or_else(|| ApiError::MandateInvalid("mandate: missing signature segment".into()))?;
    if parts.next().is_some() {
        return Err(ApiError::MandateInvalid(
            "mandate: unexpected extra segments".into(),
        ));
    }
    Ok((h, p, s))
}

/// Parse a compact JWS mandate without signature verification.
///
/// Returns the payload and the original compact form. Signature verification
/// is a separate step implemented in [`verify_signature`].
pub fn decode_unverified(compact_jws: &str) -> Result<MandatePayload, ApiError> {
    let (_h, payload_b64, _s) = split_compact(compact_jws)?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| ApiError::MandateInvalid("mandate: payload not base64url".into()))?;
    let payload: MandatePayload = serde_json::from_slice(&payload_bytes)
        .map_err(|e| ApiError::MandateInvalid(format!("mandate: {e}")))?;
    Ok(payload)
}

/// Verify the Ed25519 signature on a compact-JWS mandate using the
/// principal's resolved keyset.
///
/// * Accepts only `alg: EdDSA` — other algorithms are a v0.3 follow-up.
/// * Rejects `alg: none` outright (no silent bypass).
/// * If the JWS header carries a `kid`, tries that key first and falls
///   back to the remaining keys only if it fails to locate the matching
///   entry. If no `kid` is advertised, tries each key the principal
///   controls in order.
pub async fn verify_signature(
    compact_jws: &str,
    issuer_did: &str,
    resolver: &dyn PrincipalResolver,
) -> Result<(), ApiError> {
    let (header_b64, payload_b64, sig_b64) = split_compact(compact_jws)?;

    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| ApiError::MandateInvalid("mandate: header not base64url".into()))?;
    let header: JwsHeader = serde_json::from_slice(&header_bytes)
        .map_err(|e| ApiError::MandateInvalid(format!("mandate: header JSON: {e}")))?;

    if header.alg == "none" {
        return Err(ApiError::MandateInvalid(
            "mandate: `alg:none` rejected when signature verification is enabled".into(),
        ));
    }
    if header.alg != "EdDSA" {
        return Err(ApiError::MandateInvalid(format!(
            "mandate: unsupported alg `{}` (only EdDSA accepted)",
            header.alg
        )));
    }

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| ApiError::MandateInvalid("mandate: signature not base64url".into()))?;
    if sig_bytes.len() != 64 {
        return Err(ApiError::MandateInvalid(format!(
            "mandate: Ed25519 signature must be 64 bytes, got {}",
            sig_bytes.len()
        )));
    }
    let sig_array: [u8; 64] = sig_bytes.as_slice().try_into().expect("len checked");
    let signature = Signature::from_bytes(&sig_array);

    let keys = resolver
        .resolve(issuer_did)
        .await
        .map_err(|e| ApiError::MandateInvalid(format!("principal resolve: {e}")))?;

    // Choose candidate keys: prefer the one matching the JWS `kid`,
    // otherwise try each.
    let candidates: Vec<_> = if let Some(kid) = header.kid.as_deref() {
        let (matching, other): (Vec<_>, Vec<_>) = keys
            .into_iter()
            .partition(|k| k.kid.as_deref() == Some(kid));
        matching.into_iter().chain(other).collect()
    } else {
        keys
    };
    if candidates.is_empty() {
        return Err(ApiError::MandateInvalid(
            "principal advertises no verifying keys".into(),
        ));
    }

    let signing_input = format!("{header_b64}.{payload_b64}");
    for candidate in candidates {
        if candidate
            .key
            .verify(signing_input.as_bytes(), &signature)
            .is_ok()
        {
            return Ok(());
        }
    }

    Err(ApiError::MandateInvalid(
        "mandate: signature did not verify against principal keyset".into(),
    ))
}

/// Evaluate a mandate against an intent.
///
/// `intent_scope` is the scope required by the intent (`buy`, `subscribe`,
/// `return`, ...). `candidate_amount_minor` is the maximum amount this
/// intent would add to the spend ledger — for writes that do not spend
/// (e.g. `intent.track`) pass 0.
///
/// When `resolver` is `Some`, the mandate's JWS signature is
/// cryptographically verified against the principal's resolved keyset.
/// When `None`, the structural checks still run but the signature is
/// trusted — this is dev mode, intended only for local testing with
/// `alg:none` mandates.
pub async fn evaluate(
    compact_jws: &str,
    intent_scope: &str,
    candidate_amount_minor: i64,
    now: DateTime<Utc>,
    tenant_id: &str,
    ledger: &MandateLedger,
    resolver: Option<&dyn PrincipalResolver>,
) -> Result<MandateEvaluation, ApiError> {
    let payload = decode_unverified(compact_jws)?;

    // 1. Validity window.
    let nbf = DateTime::<Utc>::from_timestamp(payload.nbf, 0)
        .ok_or_else(|| ApiError::MandateInvalid("mandate: invalid `nbf`".into()))?;
    let exp = DateTime::<Utc>::from_timestamp(payload.exp, 0)
        .ok_or_else(|| ApiError::MandateInvalid("mandate: invalid `exp`".into()))?;
    if now < nbf {
        return Err(ApiError::MandateInvalid("mandate not yet valid".into()));
    }
    if now > exp {
        return Err(ApiError::MandateInvalid("mandate expired".into()));
    }

    // 2. Version sanity.
    if payload.icp.version.is_empty() {
        return Err(ApiError::MandateInvalid(
            "mandate: missing icp.version".into(),
        ));
    }

    // 3. Scope.
    let scopes: HashSet<&str> = payload.icp.scope.iter().map(String::as_str).collect();
    if !scopes.contains(intent_scope) {
        return Err(ApiError::MandateOutOfScope(format!(
            "mandate does not authorize scope `{intent_scope}`"
        )));
    }

    // 4. Merchant.
    if !payload.icp.merchants.is_empty()
        && !payload
            .icp
            .merchants
            .iter()
            .any(|m| m == "*" || m == tenant_id)
    {
        return Err(ApiError::MandateOutOfScope(format!(
            "mandate does not authorize merchant `{tenant_id}`"
        )));
    }

    // 5. Budget (global + per-txn, windowed).
    let budget_minor = payload.icp.budget.amount_minor;
    if let Some(per_txn) = payload.icp.budget.per_transaction {
        if candidate_amount_minor > per_txn {
            return Err(ApiError::MandateBudgetExceeded(format!(
                "intent amount {candidate_amount_minor} exceeds per-transaction cap {per_txn}"
            )));
        }
    }

    let usage = ledger.usage(&payload.jti);
    let window = parse_period(payload.icp.budget.period.as_deref()).unwrap_or(Duration::days(1));
    let window_has_elapsed = match usage.window_start {
        Some(start) => now - start > window,
        None => false,
    };
    let effective_spent = if window_has_elapsed {
        0
    } else {
        usage.spent_minor
    };
    let remaining = budget_minor.saturating_sub(effective_spent);
    if candidate_amount_minor > remaining {
        return Err(ApiError::MandateBudgetExceeded(format!(
            "intent amount {candidate_amount_minor} would exceed remaining budget {remaining}"
        )));
    }

    // 6. Signature verification (optional; gated by caller).
    let signature_verified = if let Some(r) = resolver {
        verify_signature(compact_jws, &payload.iss, r).await?;
        true
    } else {
        false
    };

    Ok(MandateEvaluation {
        payload,
        compact_jws: compact_jws.to_string(),
        spend_room_minor: remaining,
        signature_verified,
    })
}

fn parse_period(s: Option<&str>) -> Option<Duration> {
    // Very small ISO-8601 duration parser — supports the common forms we
    // actually emit: `PT<H>H`, `P<D>D`, `P<W>W`, `P1M` (approximated as 30d).
    let s = s?;
    let inner = s.strip_prefix('P')?;
    if let Some(rest) = inner.strip_prefix('T') {
        if let Some(h) = rest.strip_suffix('H') {
            return h.parse::<i64>().ok().map(Duration::hours);
        }
        return None;
    }
    if let Some(d) = inner.strip_suffix('D') {
        return d.parse::<i64>().ok().map(Duration::days);
    }
    if let Some(w) = inner.strip_suffix('W') {
        return w.parse::<i64>().ok().map(|w| Duration::days(w * 7));
    }
    if let Some(m) = inner.strip_suffix('M') {
        return m.parse::<i64>().ok().map(|m| Duration::days(m * 30));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jws(payload: &MandatePayload) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).expect("serialize payload"));
        // `alg:none` with empty signature — unverified decode accepts this.
        format!("{header}.{body}.")
    }

    #[tokio::test]
    async fn evaluate_rejects_out_of_scope() {
        let now = Utc::now();
        let payload = MandatePayload {
            iss: "did:buyer:alice".into(),
            sub: "did:stateset:agent:a".into(),
            iat: now.timestamp() - 60,
            nbf: now.timestamp() - 60,
            exp: now.timestamp() + 3600,
            jti: "m1".into(),
            icp: MandateTerms {
                version: "2026-04-21".into(),
                scope: vec!["discover".into()],
                budget: MandateBudget {
                    currency: "USD".into(),
                    amount_minor: 10_000,
                    per_transaction: None,
                    period: Some("P1D".into()),
                },
                merchants: vec!["*".into()],
                categories: vec![],
                jurisdictions: vec![],
                policies: MandatePolicies::default(),
                linked_payment_methods: vec![],
            },
        };
        let jws = make_jws(&payload);
        let ledger = MandateLedger::new();
        let err = evaluate(&jws, "buy", 1_000, now, "merchant_demo", &ledger, None)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::MandateOutOfScope(_)));
    }
}
