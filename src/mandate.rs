//! Mandate parsing, validation, and budget enforcement.
//!
//! A mandate is a compact JWS whose payload is the JSON document described
//! in ICP §6. This module decodes mandates (without full signature
//! verification — that requires principal-DID resolution, which is deferred
//! to a future release), evaluates scope + budget + validity, and
//! maintains an in-memory usage counter.
//!
//! **Draft-grade note**: signature verification against the principal's
//! advertised keyset is scaffolded as a pluggable trait
//! (`PrincipalResolver`) so a production deployment can swap in a real
//! resolver without touching the evaluation path.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use crate::errors::ApiError;

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
}

#[derive(Debug, Clone, Default)]
pub struct MandateUsage {
    pub spent_minor: i64,
    pub window_start: Option<DateTime<Utc>>,
}

/// In-memory mandate usage store. Not distributed — wire to Redis in
/// production for multi-instance deployments.
#[derive(Clone, Default)]
pub struct MandateLedger {
    inner: Arc<RwLock<HashMap<String, MandateUsage>>>,
}

impl MandateLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn usage(&self, jti: &str) -> MandateUsage {
        self.inner
            .read()
            .expect("mandate ledger read")
            .get(jti)
            .cloned()
            .unwrap_or_default()
    }

    pub fn record_spend(&self, jti: &str, amount_minor: i64, now: DateTime<Utc>) {
        let mut guard = self.inner.write().expect("mandate ledger write");
        let entry = guard.entry(jti.to_string()).or_default();
        if entry.window_start.is_none() {
            entry.window_start = Some(now);
        }
        entry.spent_minor = entry.spent_minor.saturating_add(amount_minor);
    }
}

/// Parse a compact JWS mandate without signature verification.
///
/// Returns the payload and the original compact form. Signature verification
/// is a separate step implemented in `verify_signature`.
pub fn decode_unverified(compact_jws: &str) -> Result<MandatePayload, ApiError> {
    let mut parts = compact_jws.split('.');
    let _header = parts
        .next()
        .ok_or_else(|| ApiError::MandateInvalid("mandate: missing header segment".into()))?;
    let payload_b64 = parts
        .next()
        .ok_or_else(|| ApiError::MandateInvalid("mandate: missing payload segment".into()))?;
    let _signature = parts
        .next()
        .ok_or_else(|| ApiError::MandateInvalid("mandate: missing signature segment".into()))?;
    if parts.next().is_some() {
        return Err(ApiError::MandateInvalid(
            "mandate: unexpected extra segments".into(),
        ));
    }

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| ApiError::MandateInvalid("mandate: payload not base64url".into()))?;
    let payload: MandatePayload = serde_json::from_slice(&payload_bytes)
        .map_err(|e| ApiError::MandateInvalid(format!("mandate: {e}")))?;
    Ok(payload)
}

/// Evaluate a mandate against an intent.
///
/// `intent_scope` is the scope required by the intent (`buy`, `subscribe`,
/// `return`, ...). `candidate_amount_minor` is the maximum amount this
/// intent would add to the spend ledger — for writes that do not spend
/// (e.g. `intent.track`) pass 0.
pub fn evaluate(
    compact_jws: &str,
    intent_scope: &str,
    candidate_amount_minor: i64,
    now: DateTime<Utc>,
    tenant_id: &str,
    ledger: &MandateLedger,
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
        return Err(ApiError::MandateInvalid("mandate: missing icp.version".into()));
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
        && !payload.icp.merchants.iter().any(|m| m == "*" || m == tenant_id)
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
    let effective_spent = if window_has_elapsed { 0 } else { usage.spent_minor };
    let remaining = budget_minor.saturating_sub(effective_spent);
    if candidate_amount_minor > remaining {
        return Err(ApiError::MandateBudgetExceeded(format!(
            "intent amount {candidate_amount_minor} would exceed remaining budget {remaining}"
        )));
    }

    Ok(MandateEvaluation {
        payload,
        compact_jws: compact_jws.to_string(),
        spend_room_minor: remaining,
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
        let body =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).expect("serialize payload"));
        // `alg:none` with empty signature — unverified decode accepts this.
        format!("{header}.{body}.")
    }

    #[test]
    fn evaluate_rejects_out_of_scope() {
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
        let err = evaluate(&jws, "buy", 1_000, now, "merchant_demo", &ledger).unwrap_err();
        assert!(matches!(err, ApiError::MandateOutOfScope(_)));
    }
}
