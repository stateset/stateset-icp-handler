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
use rusqlite::OptionalExtension;
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
    /// Tenant that first recorded spend against this mandate. Empty
    /// string for legacy rows written before the field existed (and
    /// before the multi-tenant work in general). The
    /// `GET /icp/v1/mandates/:jti/usage` endpoint refuses cross-tenant
    /// reads by comparing against this — first-spender owns the
    /// readable view. Subsequent spenders still consume the shared
    /// budget (protecting the principal) but cannot read the tally.
    pub tenant_id: String,
}

#[derive(Debug, Clone, Copy)]
pub struct MandateSpendLimits {
    pub budget_minor: i64,
    pub per_transaction: Option<i64>,
    pub window: Duration,
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
        self.try_usage(jti).unwrap_or_else(|err| {
            tracing::error!(jti, %err, "mandate ledger usage failed");
            MandateUsage {
                spent_minor: i64::MAX,
                window_start: Some(Utc::now()),
                tenant_id: String::new(),
            }
        })
    }

    pub fn try_usage(&self, jti: &str) -> Result<MandateUsage, ApiError> {
        match &self.backend {
            MandateBackend::Memory(inner) => {
                let guard = inner.read().map_err(|err| {
                    tracing::error!(jti, %err, "mandate ledger read lock poisoned");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                Ok(guard.get(jti).cloned().unwrap_or_default())
            }
            MandateBackend::Sqlite(pool) => {
                let conn = pool.get().map_err(|err| {
                    tracing::error!(jti, %err, "mandate ledger pool acquire failed");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                // tenant_id is fetched but defaulted on read so this query
                // also works against pre-migration databases (the column
                // wouldn't exist; `unwrap_or_default()` collapses both
                // missing-column and NULL into "").
                let row: rusqlite::Result<(i64, Option<String>, Option<String>)> = conn.query_row(
                    "SELECT spent_minor, window_start, tenant_id FROM mandate_usage WHERE jti = ?1",
                    rusqlite::params![jti],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2).ok())),
                );
                match row {
                    Ok((spent_minor, window_start_str, tenant_id)) => Ok(MandateUsage {
                        spent_minor,
                        window_start: window_start_str
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|dt| dt.with_timezone(&Utc)),
                        tenant_id: tenant_id.unwrap_or_default(),
                    }),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(MandateUsage::default()),
                    Err(err) => {
                        tracing::error!(jti, %err, "mandate ledger read failed");
                        Err(ApiError::EngineUnavailable(
                            "mandate ledger unavailable".into(),
                        ))
                    }
                }
            }
        }
    }

    /// Tenant-scoped read. Returns `None` if no spend has been
    /// recorded **or** if the calling tenant doesn't own the mandate
    /// (i.e. wasn't the first spender). Routes that surface this
    /// should map both cases to a 404 so existence isn't leaked.
    /// Empty `tenant_id` rows are *not* matched by any real tenant —
    /// see schema migration notes for the legacy-row policy.
    pub fn usage_for_tenant(&self, jti: &str, tenant_id: &str) -> Option<MandateUsage> {
        self.try_usage_for_tenant(jti, tenant_id)
            .unwrap_or_else(|err| {
                tracing::error!(jti, tenant_id, %err, "mandate tenant usage failed");
                None
            })
    }

    pub fn try_usage_for_tenant(
        &self,
        jti: &str,
        tenant_id: &str,
    ) -> Result<Option<MandateUsage>, ApiError> {
        let usage = self.try_usage(jti)?;
        // No spend recorded → no row → no tenant; treat as miss so
        // the route doesn't fabricate a "you own this empty bucket"
        // response for a jti the caller has never used.
        if usage.window_start.is_none() && usage.spent_minor == 0 && usage.tenant_id.is_empty() {
            return Ok(None);
        }
        if usage.tenant_id == tenant_id {
            Ok(Some(usage))
        } else {
            Ok(None)
        }
    }

    pub fn record_spend(&self, jti: &str, tenant_id: &str, amount_minor: i64, now: DateTime<Utc>) {
        if let Err(err) = self.try_record_spend(jti, tenant_id, amount_minor, now) {
            tracing::error!(jti, tenant_id, amount_minor, %err, "mandate spend record failed");
        }
    }

    pub fn try_record_spend(
        &self,
        jti: &str,
        tenant_id: &str,
        amount_minor: i64,
        now: DateTime<Utc>,
    ) -> Result<(), ApiError> {
        match &self.backend {
            MandateBackend::Memory(inner) => {
                let mut guard = inner.write().map_err(|err| {
                    tracing::error!(jti, tenant_id, %err, "mandate ledger write lock poisoned");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                let entry = guard.entry(jti.to_string()).or_default();
                if entry.window_start.is_none() {
                    entry.window_start = Some(now);
                }
                if entry.tenant_id.is_empty() {
                    entry.tenant_id = tenant_id.to_string();
                }
                entry.spent_minor = entry.spent_minor.saturating_add(amount_minor);
                Ok(())
            }
            MandateBackend::Sqlite(pool) => {
                let conn = pool.get().map_err(|err| {
                    tracing::error!(jti, tenant_id, %err, "mandate ledger pool acquire failed");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                // UPSERT: preserve the original window_start AND
                // tenant_id (only set on first spend for a given jti),
                // saturating_add on spent_minor. Mirrors the in-memory
                // semantics. The COALESCE(NULLIF(..., ''), ...) guard
                // keeps the existing tenant_id sticky if it's already
                // a non-empty string, while letting a non-empty new
                // tenant_id claim a legacy '' row.
                conn.execute(
                    "INSERT INTO mandate_usage (jti, spent_minor, window_start, tenant_id) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(jti) DO UPDATE SET \
                         spent_minor = MIN(9223372036854775807, mandate_usage.spent_minor + excluded.spent_minor), \
                         tenant_id = COALESCE(NULLIF(mandate_usage.tenant_id, ''), excluded.tenant_id)",
                    rusqlite::params![jti, amount_minor, now.to_rfc3339(), tenant_id],
                )
                .map(|_| ())
                .map_err(|err| {
                    tracing::error!(jti, tenant_id, amount_minor, %err, "mandate ledger write failed");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })
            }
        }
    }

    /// Atomically verify that a new spend fits the mandate budget and
    /// record it. This is the write-side budget gate used by the intent
    /// pipeline, so concurrent workers cannot both observe the same
    /// remaining balance and overspend it.
    pub fn try_record_spend_checked(
        &self,
        jti: &str,
        tenant_id: &str,
        amount_minor: i64,
        now: DateTime<Utc>,
        limits: MandateSpendLimits,
    ) -> Result<(), ApiError> {
        if amount_minor <= 0 {
            return Ok(());
        }
        if limits.budget_minor < 0 {
            return Err(ApiError::MandateBudgetExceeded(format!(
                "mandate budget {} is negative",
                limits.budget_minor
            )));
        }
        if let Some(per_txn) = limits.per_transaction {
            if amount_minor > per_txn {
                return Err(ApiError::MandateBudgetExceeded(format!(
                    "intent amount {amount_minor} exceeds per-transaction cap {per_txn}"
                )));
            }
        }

        match &self.backend {
            MandateBackend::Memory(inner) => {
                let mut guard = inner.write().map_err(|err| {
                    tracing::error!(jti, tenant_id, %err, "mandate ledger write lock poisoned");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                let entry = guard.entry(jti.to_string()).or_default();
                if entry
                    .window_start
                    .is_none_or(|start| now - start > limits.window)
                {
                    entry.spent_minor = 0;
                    entry.window_start = Some(now);
                }
                if entry.tenant_id.is_empty() {
                    entry.tenant_id = tenant_id.to_string();
                }
                let remaining = limits.budget_minor.saturating_sub(entry.spent_minor);
                if amount_minor > remaining {
                    return Err(ApiError::MandateBudgetExceeded(format!(
                        "intent amount {amount_minor} would exceed remaining budget {remaining}"
                    )));
                }
                entry.spent_minor = entry.spent_minor.saturating_add(amount_minor);
                Ok(())
            }
            MandateBackend::Sqlite(pool) => {
                let mut conn = pool.get().map_err(|err| {
                    tracing::error!(jti, tenant_id, %err, "mandate ledger pool acquire failed");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|err| {
                        tracing::error!(jti, tenant_id, %err, "mandate ledger tx begin failed");
                        ApiError::EngineUnavailable("mandate ledger unavailable".into())
                    })?;
                let row: Option<(i64, Option<String>, Option<String>)> = tx
                    .query_row(
                        "SELECT spent_minor, window_start, tenant_id \
                         FROM mandate_usage WHERE jti = ?1",
                        rusqlite::params![jti],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2).ok())),
                    )
                    .optional()
                    .map_err(|err| {
                        tracing::error!(jti, tenant_id, %err, "mandate ledger read-for-update failed");
                        ApiError::EngineUnavailable("mandate ledger unavailable".into())
                    })?;

                let mut usage = row
                    .map(|(spent_minor, window_start_str, owner)| MandateUsage {
                        spent_minor,
                        window_start: window_start_str
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|dt| dt.with_timezone(&Utc)),
                        tenant_id: owner.unwrap_or_default(),
                    })
                    .unwrap_or_default();
                if usage
                    .window_start
                    .is_none_or(|start| now - start > limits.window)
                {
                    usage.spent_minor = 0;
                    usage.window_start = Some(now);
                }
                if usage.tenant_id.is_empty() {
                    usage.tenant_id = tenant_id.to_string();
                }
                let remaining = limits.budget_minor.saturating_sub(usage.spent_minor);
                if amount_minor > remaining {
                    return Err(ApiError::MandateBudgetExceeded(format!(
                        "intent amount {amount_minor} would exceed remaining budget {remaining}"
                    )));
                }
                usage.spent_minor = usage.spent_minor.saturating_add(amount_minor);
                let window_start = usage.window_start.map(|w| w.to_rfc3339());
                tx.execute(
                    "INSERT INTO mandate_usage (jti, spent_minor, window_start, tenant_id) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(jti) DO UPDATE SET \
                         spent_minor = excluded.spent_minor, \
                         window_start = excluded.window_start, \
                         tenant_id = COALESCE(NULLIF(mandate_usage.tenant_id, ''), excluded.tenant_id)",
                    rusqlite::params![jti, usage.spent_minor, window_start, usage.tenant_id],
                )
                .map_err(|err| {
                    tracing::error!(jti, tenant_id, amount_minor, %err, "mandate ledger checked write failed");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                tx.commit().map_err(|err| {
                    tracing::error!(jti, tenant_id, %err, "mandate ledger checked commit failed");
                    ApiError::EngineUnavailable("mandate ledger unavailable".into())
                })?;
                Ok(())
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
#[allow(clippy::too_many_arguments)]
pub async fn evaluate(
    compact_jws: &str,
    intent_scope: &str,
    candidate_amount_minor: i64,
    now: DateTime<Utc>,
    tenant_id: &str,
    jurisdiction: Option<&str>,
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

    // 5. Jurisdiction (spec §6.1 step 6). When the mandate constrains
    // jurisdictions (non-empty and not the `*` wildcard), the intent's
    // fulfillment jurisdiction MUST be one of them. A mandate that bounds
    // jurisdictions but on an intent that declares none cannot be proven
    // in-bounds, so it is refused rather than silently allowed.
    let allows_any_jurisdiction =
        payload.icp.jurisdictions.is_empty() || payload.icp.jurisdictions.iter().any(|j| j == "*");
    if !allows_any_jurisdiction {
        match jurisdiction {
            Some(j)
                if payload
                    .icp
                    .jurisdictions
                    .iter()
                    .any(|allowed| jurisdiction_authorizes(allowed, j)) => {}
            Some(j) => {
                return Err(ApiError::MandateOutOfScope(format!(
                    "mandate does not authorize jurisdiction `{j}`"
                )));
            }
            None => {
                return Err(ApiError::MandateOutOfScope(
                    "mandate constrains jurisdictions but the intent declares none".into(),
                ));
            }
        }
    }

    // 6. Policies (spec §6.1 step 7). Honor the declared mandate policies.
    // `prohibit_subscriptions` blocks any subscription-scoped intent.
    if payload.icp.policies.prohibit_subscriptions && intent_scope == "subscribe" {
        return Err(ApiError::MandateOutOfScope(
            "mandate policy prohibits subscriptions".into(),
        ));
    }

    // 7. Budget (global + per-txn, windowed).
    let budget_minor = payload.icp.budget.amount_minor;
    if let Some(per_txn) = payload.icp.budget.per_transaction {
        if candidate_amount_minor > per_txn {
            return Err(ApiError::MandateBudgetExceeded(format!(
                "intent amount {candidate_amount_minor} exceeds per-transaction cap {per_txn}"
            )));
        }
    }

    let usage = ledger.try_usage(&payload.jti)?;
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

    // 8. Signature verification (optional; gated by caller).
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

/// Whether a mandate's `allowed` jurisdiction code authorizes an intent's
/// `actual` fulfillment jurisdiction.
///
/// Mandates are conventionally scoped by ISO 3166-1 country code (`"US"`),
/// while an intent's `context.jurisdiction` is typically the more specific
/// ISO 3166-2 subdivision (`"US-CA"`). A listed country therefore
/// authorizes any of its subdivisions: `"US"` covers `"US-CA"`. The match
/// is case-insensitive and also accepts an exact subdivision listing
/// (`"US-CA"` authorizes `"US-CA"`). It is deliberately one-directional —
/// a mandate scoped to the narrower `"US-CA"` does NOT authorize a broader
/// `"US"` fulfillment, since that could not be proven in-bounds.
fn jurisdiction_authorizes(allowed: &str, actual: &str) -> bool {
    if allowed.eq_ignore_ascii_case(actual) {
        return true;
    }
    // `allowed` is a country; `actual` is `<country>-<subdivision>`.
    match actual.split_once('-') {
        Some((country, _)) => allowed.eq_ignore_ascii_case(country),
        None => false,
    }
}

pub(crate) fn parse_period(s: Option<&str>) -> Option<Duration> {
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
        let err = evaluate(
            &jws,
            "buy",
            1_000,
            now,
            "merchant_demo",
            None,
            &ledger,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::MandateOutOfScope(_)));
    }

    /// Builds a wide-open `buy` mandate, then lets the caller tighten
    /// jurisdictions/policies for a specific assertion.
    fn buy_payload(
        now: DateTime<Utc>,
        jurisdictions: Vec<String>,
        policies: MandatePolicies,
        scope: Vec<String>,
    ) -> MandatePayload {
        MandatePayload {
            iss: "did:buyer:alice".into(),
            sub: "did:stateset:agent:a".into(),
            iat: now.timestamp() - 60,
            nbf: now.timestamp() - 60,
            exp: now.timestamp() + 3600,
            jti: "m_jur".into(),
            icp: MandateTerms {
                version: "2026-04-21".into(),
                scope,
                budget: MandateBudget {
                    currency: "USD".into(),
                    amount_minor: 10_000,
                    per_transaction: None,
                    period: Some("P1D".into()),
                },
                merchants: vec!["*".into()],
                categories: vec![],
                jurisdictions,
                policies,
                linked_payment_methods: vec![],
            },
        }
    }

    #[tokio::test]
    async fn evaluate_enforces_jurisdiction() {
        let now = Utc::now();
        let ledger = MandateLedger::new();
        let payload = buy_payload(
            now,
            vec!["US".into(), "CA".into()],
            MandatePolicies::default(),
            vec!["buy".into()],
        );
        let jws = make_jws(&payload);

        // In-jurisdiction succeeds — and crucially the intent declares the
        // ISO-3166-2 *subdivision* `US-CA` while the mandate lists the
        // *country* `US`. A listed country must authorize its subdivisions,
        // or every real buy (whose context.jurisdiction is `US-CA`) against
        // a country-scoped mandate would be wrongly rejected.
        assert!(evaluate(
            &jws,
            "buy",
            1_000,
            now,
            "merchant_demo",
            Some("US-CA"),
            &ledger,
            None
        )
        .await
        .is_ok());

        // Out-of-jurisdiction is refused.
        let err = evaluate(
            &jws,
            "buy",
            1_000,
            now,
            "merchant_demo",
            Some("GB"),
            &ledger,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::MandateOutOfScope(_)));

        // A jurisdiction-bounded mandate on an intent that declares none
        // cannot be proven in-bounds and is refused.
        let err = evaluate(
            &jws,
            "buy",
            1_000,
            now,
            "merchant_demo",
            None,
            &ledger,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::MandateOutOfScope(_)));
    }

    #[test]
    fn jurisdiction_authorizes_country_over_subdivision() {
        // Country authorizes its subdivisions, case-insensitively.
        assert!(jurisdiction_authorizes("US", "US-CA"));
        assert!(jurisdiction_authorizes("us", "US-CA"));
        assert!(jurisdiction_authorizes("US", "US"));
        assert!(jurisdiction_authorizes("US-CA", "US-CA"));
        // But NOT the reverse: a subdivision-scoped mandate doesn't
        // authorize the broader country, and unrelated codes don't match.
        assert!(!jurisdiction_authorizes("US-CA", "US"));
        assert!(!jurisdiction_authorizes("US", "GB"));
        assert!(!jurisdiction_authorizes("US", "CA"));
        assert!(!jurisdiction_authorizes("US-CA", "US-NY"));
    }

    #[tokio::test]
    async fn evaluate_wildcard_jurisdiction_allows_any() {
        let now = Utc::now();
        let ledger = MandateLedger::new();
        let payload = buy_payload(
            now,
            vec!["*".into()],
            MandatePolicies::default(),
            vec!["buy".into()],
        );
        let jws = make_jws(&payload);
        assert!(evaluate(
            &jws,
            "buy",
            1_000,
            now,
            "merchant_demo",
            Some("ZZ"),
            &ledger,
            None
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn evaluate_enforces_prohibit_subscriptions_policy() {
        let now = Utc::now();
        let ledger = MandateLedger::new();
        let policies = MandatePolicies {
            prohibit_subscriptions: true,
            ..Default::default()
        };
        let payload = buy_payload(now, vec![], policies, vec!["subscribe".into()]);
        let jws = make_jws(&payload);
        let err = evaluate(
            &jws,
            "subscribe",
            1_000,
            now,
            "merchant_demo",
            None,
            &ledger,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::MandateOutOfScope(_)));
    }
}
