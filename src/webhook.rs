//! Outbound webhook delivery (durable outbox pattern).
//!
//! Subscribers register via the handler's startup config (`ICP_WEBHOOK_URL`
//! + `ICP_WEBHOOK_SECRET`); every state-changing intent enqueues a
//! [`WebhookDelivery`] row that the background worker
//! ([`run_loop`]) drains. Deliveries are HMAC-SHA256 signed in the
//! Stripe convention:
//!
//! ```text
//! ICP-Signature: t=<unix_seconds>,v1=<hex_hmac_sha256>
//! ```
//!
//! Where the HMAC payload is `<t>.<body_json>`. The leading timestamp
//! protects against replay; receivers SHOULD reject signatures whose
//! `t` is more than 5 minutes old.
//!
//! The outbox writes happen *synchronously* inside the intent pipeline
//! so an event is durably enqueued before the response is sent. If the
//! handler crashes between the intent succeeding and the worker
//! delivering, the next process to come up resumes from the same outbox
//! row — events are at-least-once.
//!
//! Retry policy: exponential backoff with `attempts²` seconds between
//! attempts, capped at one hour. After `max_attempts` failures the row
//! transitions to `dead_lettered` and stops being retried; operators
//! can manually re-enqueue via a future admin endpoint.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration as StdDuration;

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::state_db::StatePool;

pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_TICK_SECS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Enqueued, awaiting first attempt.
    Pending,
    /// Worker has picked it up; either succeeds or moves to Failed.
    InFlight,
    /// 2xx response received from the subscriber.
    Delivered,
    /// Last attempt failed; will retry per backoff schedule.
    Failed,
    /// Exhausted `max_attempts`; never retried automatically.
    DeadLettered,
}

impl DeliveryStatus {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InFlight => "in_flight",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::DeadLettered => "dead_lettered",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "in_flight" => Self::InFlight,
            "delivered" => Self::Delivered,
            "failed" => Self::Failed,
            "dead_lettered" => Self::DeadLettered,
            _ => Self::Pending,
        }
    }
}

/// Why a manual `reset_for_retry` call was refused. Each variant maps
/// 1:1 to an HTTP status the route handler should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryError {
    /// No row with this id exists → 404.
    NotFound,
    /// Already enqueued — retry would be a noisy no-op → 412.
    AlreadyPending,
    /// Worker has it in flight; retrying now would race the worker → 412.
    InFlight,
    /// Receiver already accepted; nothing to retry → 412.
    AlreadyDelivered,
}

impl RetryError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::NotFound => "webhook delivery not found",
            Self::AlreadyPending => "delivery is already pending; no retry needed",
            Self::InFlight => "delivery is in flight; retry after the current attempt completes",
            Self::AlreadyDelivered => "delivery already succeeded; nothing to retry",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub id: String,
    pub event_id: String,
    pub event_type: String,
    pub url: String,
    /// JSON body that will be POSTed verbatim. Already serialized so
    /// the signed bytes match exactly what's transmitted.
    pub payload_json: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub max_attempts: u32,
    pub next_attempt_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<DateTime<Utc>>,
    /// Originating tenant id. Empty string for pre-multi-tenant rows
    /// and for events the handler enqueues outside any tenant scope
    /// (currently none — every state-changing intent has a bearer key
    /// so a tenant is always present).
    #[serde(default)]
    pub tenant_id: String,
}

// --------------------------------------------------------------------------
// Signing
// --------------------------------------------------------------------------

type HmacSha256 = Hmac<Sha256>;

/// Compute the value of the `ICP-Signature` header for a delivery.
/// Format mirrors Stripe's: `t=<unix>,v1=<hex>`.
pub fn sign(secret: &str, timestamp_unix: i64, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC-SHA256 key length is unbounded");
    let signing_input = format!("{timestamp_unix}.");
    mac.update(signing_input.as_bytes());
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    format!("t={timestamp_unix},v1={}", hex::encode(tag))
}

/// Receiver-side helper: returns true iff the supplied header value
/// verifies against `secret` for the given body.
pub fn verify(secret: &str, header_value: &str, body: &[u8]) -> bool {
    // Parse `t=<unix>,v1=<hex>`; tolerate extra fields.
    let mut t = None;
    let mut v1 = None;
    for part in header_value.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("t=") {
            t = rest.parse::<i64>().ok();
        } else if let Some(rest) = part.strip_prefix("v1=") {
            v1 = Some(rest.to_string());
        }
    }
    let (Some(ts), Some(supplied)) = (t, v1) else {
        return false;
    };
    let expected = sign(secret, ts, body);
    // Compare the v1 portion only.
    let expected_v1 = expected
        .split(',')
        .find_map(|p| p.strip_prefix("v1=").map(str::to_string))
        .unwrap_or_default();
    constant_time_eq(supplied.as_bytes(), expected_v1.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// --------------------------------------------------------------------------
// Per-tenant subscribers
// --------------------------------------------------------------------------

/// One registered destination for a tenant's webhook events. The
/// outbox enqueues one `WebhookDelivery` per active subscriber whose
/// `tenant_id` matches the originating tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSubscriber {
    pub id: String,
    pub tenant_id: String,
    pub url: String,
    /// Secret used to HMAC-sign deliveries to this subscriber.
    /// Round-trips on the create response so the caller can also
    /// store / display it; subsequent reads (`GET`) redact it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct SubscriberStore {
    backend: SubBackend,
}

#[derive(Clone)]
enum SubBackend {
    Memory(Arc<RwLock<HashMap<String, WebhookSubscriber>>>),
    Sqlite(StatePool),
}

impl Default for SubscriberStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl SubscriberStore {
    pub fn in_memory() -> Self {
        Self {
            backend: SubBackend::Memory(Arc::new(RwLock::new(HashMap::new()))),
        }
    }

    pub fn with_pool(pool: StatePool) -> Self {
        Self {
            backend: SubBackend::Sqlite(pool),
        }
    }

    /// Insert a new subscriber. The supplied row must already have a
    /// fresh id and the secret populated.
    pub fn insert(&self, sub: WebhookSubscriber) {
        match &self.backend {
            SubBackend::Memory(inner) => {
                inner
                    .write()
                    .expect("subscribers write")
                    .insert(sub.id.clone(), sub);
            }
            SubBackend::Sqlite(pool) => {
                let conn = pool.get().expect("subscribers pool acquire");
                conn.execute(
                    "INSERT INTO webhook_subscribers \
                         (id, tenant_id, url, secret, active, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        sub.id,
                        sub.tenant_id,
                        sub.url,
                        sub.secret.clone().unwrap_or_default(),
                        i64::from(sub.active),
                        sub.created_at.to_rfc3339(),
                        sub.updated_at.to_rfc3339(),
                    ],
                )
                .expect("subscribers insert");
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<WebhookSubscriber> {
        match &self.backend {
            SubBackend::Memory(inner) => inner.read().expect("subscribers read").get(id).cloned(),
            SubBackend::Sqlite(pool) => {
                let conn = pool.get().expect("subscribers pool acquire");
                conn.query_row(
                    "SELECT id, tenant_id, url, secret, active, created_at, updated_at \
                     FROM webhook_subscribers WHERE id = ?1",
                    rusqlite::params![id],
                    Self::row_to_subscriber,
                )
                .optional()
                .expect("subscribers read")
            }
        }
    }

    /// All subscribers belonging to `tenant_id`, regardless of active
    /// state. Used by the `GET /icp/v1/webhook_subscribers` endpoint.
    pub fn list_for_tenant(&self, tenant_id: &str) -> Vec<WebhookSubscriber> {
        match &self.backend {
            SubBackend::Memory(inner) => inner
                .read()
                .expect("subscribers read")
                .values()
                .filter(|s| s.tenant_id == tenant_id)
                .cloned()
                .collect(),
            SubBackend::Sqlite(pool) => {
                let conn = pool.get().expect("subscribers pool acquire");
                let mut stmt = conn
                    .prepare(
                        "SELECT id, tenant_id, url, secret, active, created_at, updated_at \
                         FROM webhook_subscribers WHERE tenant_id = ?1 \
                         ORDER BY created_at DESC",
                    )
                    .expect("prepare list_for_tenant");
                let rows = stmt
                    .query_map(rusqlite::params![tenant_id], Self::row_to_subscriber)
                    .expect("query list_for_tenant");
                rows.filter_map(Result::ok).collect()
            }
        }
    }

    /// Active subscribers for `tenant_id` — what the fan-out path uses.
    /// Distinct from `list_for_tenant` because the admin endpoint wants
    /// to see disabled rows too.
    pub fn list_active_for_tenant(&self, tenant_id: &str) -> Vec<WebhookSubscriber> {
        self.list_for_tenant(tenant_id)
            .into_iter()
            .filter(|s| s.active)
            .collect()
    }

    pub fn set_active(
        &self,
        id: &str,
        active: bool,
        now: DateTime<Utc>,
    ) -> Option<WebhookSubscriber> {
        match &self.backend {
            SubBackend::Memory(inner) => {
                let mut guard = inner.write().expect("subscribers write");
                let s = guard.get_mut(id)?;
                s.active = active;
                s.updated_at = now;
                Some(s.clone())
            }
            SubBackend::Sqlite(pool) => {
                let updated = {
                    let conn = pool.get().expect("subscribers pool acquire");
                    conn.execute(
                        "UPDATE webhook_subscribers SET active = ?1, updated_at = ?2 \
                         WHERE id = ?3",
                        rusqlite::params![i64::from(active), now.to_rfc3339(), id],
                    )
                    .expect("subscribers set_active")
                };
                if updated == 0 {
                    None
                } else {
                    self.get(id)
                }
            }
        }
    }

    pub fn delete(&self, id: &str) -> bool {
        match &self.backend {
            SubBackend::Memory(inner) => inner
                .write()
                .expect("subscribers write")
                .remove(id)
                .is_some(),
            SubBackend::Sqlite(pool) => {
                let conn = pool.get().expect("subscribers pool acquire");
                conn.execute(
                    "DELETE FROM webhook_subscribers WHERE id = ?1",
                    rusqlite::params![id],
                )
                .expect("subscribers delete")
                    > 0
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.backend {
            SubBackend::Memory(inner) => inner.read().expect("subscribers read").len(),
            SubBackend::Sqlite(pool) => {
                let conn = pool.get().expect("subscribers pool acquire");
                conn.query_row("SELECT COUNT(*) FROM webhook_subscribers", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as usize)
                .expect("subscribers count")
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn row_to_subscriber(row: &rusqlite::Row<'_>) -> rusqlite::Result<WebhookSubscriber> {
        let parse_dt = |s: String| {
            DateTime::parse_from_rfc3339(&s)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now())
        };
        Ok(WebhookSubscriber {
            id: row.get(0)?,
            tenant_id: row.get(1)?,
            url: row.get(2)?,
            secret: {
                let s: String = row.get(3)?;
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            },
            active: row.get::<_, i64>(4)? != 0,
            created_at: parse_dt(row.get(5)?),
            updated_at: parse_dt(row.get(6)?),
        })
    }
}

// --------------------------------------------------------------------------
// Outbox store
// --------------------------------------------------------------------------

#[derive(Clone)]
pub struct WebhookOutbox {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Memory(Arc<RwLock<HashMap<String, WebhookDelivery>>>),
    Sqlite(StatePool),
}

impl Default for WebhookOutbox {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl WebhookOutbox {
    pub fn in_memory() -> Self {
        Self {
            backend: Backend::Memory(Arc::new(RwLock::new(HashMap::new()))),
        }
    }

    pub fn with_pool(pool: StatePool) -> Self {
        Self {
            backend: Backend::Sqlite(pool),
        }
    }

    pub fn enqueue(&self, delivery: WebhookDelivery) {
        match &self.backend {
            Backend::Memory(inner) => {
                inner
                    .write()
                    .expect("outbox write")
                    .insert(delivery.id.clone(), delivery);
            }
            Backend::Sqlite(pool) => {
                let conn = pool.get().expect("outbox pool acquire");
                conn.execute(
                    "INSERT INTO webhook_deliveries \
                         (id, event_id, event_type, url, payload_json, status, \
                          attempts, max_attempts, next_attempt_at, \
                          last_status_code, last_error, \
                          created_at, updated_at, delivered_at, tenant_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
                     ON CONFLICT(id) DO NOTHING",
                    rusqlite::params![
                        delivery.id,
                        delivery.event_id,
                        delivery.event_type,
                        delivery.url,
                        delivery.payload_json,
                        delivery.status.wire_name(),
                        delivery.attempts as i64,
                        delivery.max_attempts as i64,
                        delivery.next_attempt_at.to_rfc3339(),
                        delivery.last_status_code.map(|c| c as i64),
                        delivery.last_error,
                        delivery.created_at.to_rfc3339(),
                        delivery.updated_at.to_rfc3339(),
                        delivery.delivered_at.map(|d| d.to_rfc3339()),
                        delivery.tenant_id,
                    ],
                )
                .expect("outbox enqueue");
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => inner.read().expect("outbox read").get(id).cloned(),
            Backend::Sqlite(pool) => {
                let conn = pool.get().expect("outbox pool acquire");
                let row: Option<DeliveryRow> = conn
                    .query_row(
                        "SELECT id, event_id, event_type, url, payload_json, status, \
                                attempts, max_attempts, next_attempt_at, \
                                last_status_code, last_error, \
                                created_at, updated_at, delivered_at, tenant_id \
                         FROM webhook_deliveries WHERE id = ?1",
                        rusqlite::params![id],
                        DeliveryRow::from_row,
                    )
                    .optional()
                    .expect("outbox read");
                row.map(WebhookDelivery::from_row)
            }
        }
    }

    /// Pending or Failed deliveries with `next_attempt_at <= now`. Limit
    /// caps the per-tick batch.
    pub fn list_due(&self, now: DateTime<Utc>, limit: usize) -> Vec<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut due: Vec<_> = inner
                    .read()
                    .expect("outbox read")
                    .values()
                    .filter(|d| {
                        matches!(d.status, DeliveryStatus::Pending | DeliveryStatus::Failed)
                            && d.next_attempt_at <= now
                    })
                    .cloned()
                    .collect();
                due.sort_by(|a, b| a.next_attempt_at.cmp(&b.next_attempt_at));
                due.truncate(limit);
                due
            }
            Backend::Sqlite(pool) => {
                let conn = pool.get().expect("outbox pool acquire");
                let mut stmt = conn
                    .prepare(
                        "SELECT id, event_id, event_type, url, payload_json, status, \
                                attempts, max_attempts, next_attempt_at, \
                                last_status_code, last_error, \
                                created_at, updated_at, delivered_at, tenant_id \
                         FROM webhook_deliveries \
                         WHERE status IN ('pending','failed') \
                           AND next_attempt_at <= ?1 \
                         ORDER BY next_attempt_at ASC \
                         LIMIT ?2",
                    )
                    .expect("prepare list_due");
                let rows = stmt
                    .query_map(
                        rusqlite::params![now.to_rfc3339(), limit as i64],
                        DeliveryRow::from_row,
                    )
                    .expect("query list_due");
                rows.filter_map(Result::ok)
                    .map(WebhookDelivery::from_row)
                    .collect()
            }
        }
    }

    pub fn list_recent(&self, limit: usize) -> Vec<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut all: Vec<_> = inner
                    .read()
                    .expect("outbox read")
                    .values()
                    .cloned()
                    .collect();
                all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                all.truncate(limit);
                all
            }
            Backend::Sqlite(pool) => {
                let conn = pool.get().expect("outbox pool acquire");
                let mut stmt = conn
                    .prepare(
                        "SELECT id, event_id, event_type, url, payload_json, status, \
                                attempts, max_attempts, next_attempt_at, \
                                last_status_code, last_error, \
                                created_at, updated_at, delivered_at, tenant_id \
                         FROM webhook_deliveries \
                         ORDER BY created_at DESC LIMIT ?1",
                    )
                    .expect("prepare list_recent");
                let rows = stmt
                    .query_map(rusqlite::params![limit as i64], DeliveryRow::from_row)
                    .expect("query list_recent");
                rows.filter_map(Result::ok)
                    .map(WebhookDelivery::from_row)
                    .collect()
            }
        }
    }

    /// Tenant-scoped variant of [`list_recent`]. Returns only rows
    /// whose `tenant_id` matches; optionally narrows by `status`.
    /// `status` is the wire name (`"pending"`, `"in_flight"`,
    /// `"delivered"`, `"failed"`, `"dead_lettered"`); unknown values
    /// match nothing (the route handler validates first).
    pub fn list_recent_for_tenant(
        &self,
        tenant_id: &str,
        limit: usize,
        status: Option<DeliveryStatus>,
    ) -> Vec<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut all: Vec<_> = inner
                    .read()
                    .expect("outbox read")
                    .values()
                    .filter(|d| d.tenant_id == tenant_id)
                    .filter(|d| status.is_none_or(|s| d.status == s))
                    .cloned()
                    .collect();
                all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                all.truncate(limit);
                all
            }
            Backend::Sqlite(pool) => {
                let conn = pool.get().expect("outbox pool acquire");
                let rows: Vec<DeliveryRow> = if let Some(status) = status {
                    let mut stmt = conn
                        .prepare(
                            "SELECT id, event_id, event_type, url, payload_json, status, \
                                    attempts, max_attempts, next_attempt_at, \
                                    last_status_code, last_error, \
                                    created_at, updated_at, delivered_at, tenant_id \
                             FROM webhook_deliveries \
                             WHERE tenant_id = ?1 AND status = ?2 \
                             ORDER BY created_at DESC LIMIT ?3",
                        )
                        .expect("prepare list_recent_for_tenant_filtered");
                    let mapped = stmt
                        .query_map(
                            rusqlite::params![tenant_id, status.wire_name(), limit as i64],
                            DeliveryRow::from_row,
                        )
                        .expect("query list_recent_for_tenant_filtered");
                    mapped.filter_map(Result::ok).collect()
                } else {
                    let mut stmt = conn
                        .prepare(
                            "SELECT id, event_id, event_type, url, payload_json, status, \
                                    attempts, max_attempts, next_attempt_at, \
                                    last_status_code, last_error, \
                                    created_at, updated_at, delivered_at, tenant_id \
                             FROM webhook_deliveries \
                             WHERE tenant_id = ?1 \
                             ORDER BY created_at DESC LIMIT ?2",
                        )
                        .expect("prepare list_recent_for_tenant");
                    let mapped = stmt
                        .query_map(
                            rusqlite::params![tenant_id, limit as i64],
                            DeliveryRow::from_row,
                        )
                        .expect("query list_recent_for_tenant");
                    mapped.filter_map(Result::ok).collect()
                };
                rows.into_iter().map(WebhookDelivery::from_row).collect()
            }
        }
    }

    /// Tenant-scoped get. Returns `None` for rows that don't exist
    /// **or** that belong to a different tenant — callers map both to
    /// 404 so existence isn't leaked across tenant boundaries.
    pub fn get_for_tenant(&self, id: &str, tenant_id: &str) -> Option<WebhookDelivery> {
        let d = self.get(id)?;
        if d.tenant_id == tenant_id {
            Some(d)
        } else {
            None
        }
    }

    /// Tenant-scoped retry. Cross-tenant ids return `NotFound` so the
    /// route handler emits a 404 — same shape a missing row produces.
    pub fn reset_for_retry_for_tenant(
        &self,
        id: &str,
        tenant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<WebhookDelivery, RetryError> {
        match self.get(id) {
            Some(d) if d.tenant_id == tenant_id => self.reset_for_retry(id, now),
            // Cross-tenant or nonexistent — both surface as NotFound so
            // tenant A can't probe for the existence of tenant B's
            // delivery ids.
            _ => Err(RetryError::NotFound),
        }
    }

    pub fn mark_in_flight(&self, id: &str, now: DateTime<Utc>) {
        self.update(id, |d| {
            d.status = DeliveryStatus::InFlight;
            d.updated_at = now;
        });
    }

    pub fn mark_delivered(&self, id: &str, status_code: u16, now: DateTime<Utc>) {
        self.update(id, |d| {
            d.status = DeliveryStatus::Delivered;
            d.last_status_code = Some(status_code);
            d.last_error = None;
            d.delivered_at = Some(now);
            d.updated_at = now;
            d.attempts = d.attempts.saturating_add(1);
        });
    }

    /// Operator-initiated retry: flip a `failed` or `dead_lettered`
    /// row back to `pending` so the worker picks it up on its next
    /// tick. Resets `attempts` (operators are expressing intent that
    /// the prior failures don't count toward exhaustion this cycle),
    /// clears `last_error` / `last_status_code`, and zeros out
    /// `delivered_at` defensively. Returns the post-retry delivery
    /// snapshot, or a typed error explaining why retry was refused.
    ///
    /// Refuses to retry `pending` (already enqueued — would be a
    /// no-op-with-noise), `in_flight` (would race the worker), and
    /// `delivered` (no point — receiver already accepted). Unknown
    /// id returns `NotFound`.
    pub fn reset_for_retry(
        &self,
        id: &str,
        now: DateTime<Utc>,
    ) -> Result<WebhookDelivery, RetryError> {
        let current = self.get(id).ok_or(RetryError::NotFound)?;
        match current.status {
            DeliveryStatus::Pending => return Err(RetryError::AlreadyPending),
            DeliveryStatus::InFlight => return Err(RetryError::InFlight),
            DeliveryStatus::Delivered => return Err(RetryError::AlreadyDelivered),
            DeliveryStatus::Failed | DeliveryStatus::DeadLettered => {}
        }
        self.update(id, |d| {
            d.status = DeliveryStatus::Pending;
            d.attempts = 0;
            d.last_status_code = None;
            d.last_error = None;
            d.delivered_at = None;
            d.next_attempt_at = now;
            d.updated_at = now;
        });
        // Re-read so we hand back the post-update snapshot, not the
        // captured-before-update copy.
        self.get(id).ok_or(RetryError::NotFound)
    }

    /// Bump the failure counter; transition to DeadLettered if we hit
    /// max_attempts, otherwise schedule the next retry.
    pub fn bump_failure(
        &self,
        id: &str,
        status_code: Option<u16>,
        error: Option<String>,
        now: DateTime<Utc>,
    ) {
        self.update(id, |d| {
            d.attempts = d.attempts.saturating_add(1);
            d.last_status_code = status_code;
            d.last_error = error;
            d.updated_at = now;
            if d.attempts >= d.max_attempts {
                d.status = DeliveryStatus::DeadLettered;
            } else {
                d.status = DeliveryStatus::Failed;
                d.next_attempt_at = now + backoff_for(d.attempts);
            }
        });
    }

    fn update<F>(&self, id: &str, f: F)
    where
        F: FnOnce(&mut WebhookDelivery),
    {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut guard = inner.write().expect("outbox write");
                if let Some(d) = guard.get_mut(id) {
                    f(d);
                }
            }
            Backend::Sqlite(pool) => {
                let mut conn = pool.get().expect("outbox pool acquire");
                let tx = conn.transaction().expect("outbox tx");
                let row: Option<DeliveryRow> = tx
                    .query_row(
                        "SELECT id, event_id, event_type, url, payload_json, status, \
                                attempts, max_attempts, next_attempt_at, \
                                last_status_code, last_error, \
                                created_at, updated_at, delivered_at, tenant_id \
                         FROM webhook_deliveries WHERE id = ?1",
                        rusqlite::params![id],
                        DeliveryRow::from_row,
                    )
                    .optional()
                    .expect("outbox read-for-update");
                let Some(row) = row else { return };
                let mut delivery = WebhookDelivery::from_row(row);
                f(&mut delivery);
                tx.execute(
                    "UPDATE webhook_deliveries SET \
                         status = ?1, attempts = ?2, next_attempt_at = ?3, \
                         last_status_code = ?4, last_error = ?5, \
                         updated_at = ?6, delivered_at = ?7 \
                     WHERE id = ?8",
                    rusqlite::params![
                        delivery.status.wire_name(),
                        delivery.attempts as i64,
                        delivery.next_attempt_at.to_rfc3339(),
                        delivery.last_status_code.map(|c| c as i64),
                        delivery.last_error,
                        delivery.updated_at.to_rfc3339(),
                        delivery.delivered_at.map(|d| d.to_rfc3339()),
                        delivery.id,
                    ],
                )
                .expect("outbox update");
                tx.commit().expect("outbox commit");
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.backend {
            Backend::Memory(inner) => inner.read().expect("outbox read").len(),
            Backend::Sqlite(pool) => {
                let conn = pool.get().expect("outbox pool acquire");
                conn.query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as usize)
                .expect("outbox count")
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Exponential backoff: `attempts²` seconds, clamped to 1 hour.
/// Attempts is the count *after* the increment, so attempt #1 → 1s
/// (first retry happens 1s after first failure), #2 → 4s, #3 → 9s,
/// #4 → 16s, #5 → 25s.
pub fn backoff_for(attempts: u32) -> Duration {
    let secs = (attempts as i64).saturating_mul(attempts as i64);
    Duration::seconds(secs.min(3600))
}

// --------------------------------------------------------------------------
// Worker
// --------------------------------------------------------------------------

#[derive(Clone)]
pub struct WebhookWorker {
    outbox: WebhookOutbox,
    secret: String,
    client: reqwest::Client,
    timeout: StdDuration,
}

impl WebhookWorker {
    pub fn new(outbox: WebhookOutbox, secret: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("build webhook reqwest client");
        Self {
            outbox,
            secret,
            client,
            timeout: StdDuration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Run one drain pass — process every due delivery synchronously.
    /// Returns the number of deliveries acted on (Delivered + Failed +
    /// DeadLettered transitions in this pass).
    pub async fn tick(&self, now: DateTime<Utc>) -> TickReport {
        let mut report = TickReport::default();
        let due = self.outbox.list_due(now, 50);
        report.due = due.len();
        for delivery in due {
            self.outbox.mark_in_flight(&delivery.id, now);
            let send_result = self.send_one(&delivery).await;
            let after = Utc::now();
            match send_result {
                Ok(status) if (200..300).contains(&status) => {
                    self.outbox.mark_delivered(&delivery.id, status, after);
                    report.delivered += 1;
                }
                Ok(status) => {
                    self.outbox.bump_failure(
                        &delivery.id,
                        Some(status),
                        Some(format!("HTTP {status}")),
                        after,
                    );
                    if let Some(d) = self.outbox.get(&delivery.id) {
                        if matches!(d.status, DeliveryStatus::DeadLettered) {
                            report.dead_lettered += 1;
                        } else {
                            report.failed += 1;
                        }
                    }
                }
                Err(e) => {
                    self.outbox
                        .bump_failure(&delivery.id, None, Some(e.to_string()), after);
                    if let Some(d) = self.outbox.get(&delivery.id) {
                        if matches!(d.status, DeliveryStatus::DeadLettered) {
                            report.dead_lettered += 1;
                        } else {
                            report.failed += 1;
                        }
                    }
                }
            }
        }
        report
    }

    async fn send_one(&self, delivery: &WebhookDelivery) -> reqwest::Result<u16> {
        let now_unix = Utc::now().timestamp();
        let signature = sign(&self.secret, now_unix, delivery.payload_json.as_bytes());
        let resp = self
            .client
            .post(&delivery.url)
            .timeout(self.timeout)
            .header("Content-Type", "application/json")
            .header("ICP-Signature", signature)
            .header("ICP-Event-Type", &delivery.event_type)
            .header("ICP-Event-Id", &delivery.event_id)
            .header("ICP-Delivery-Id", &delivery.id)
            .header("ICP-Delivery-Attempt", (delivery.attempts + 1).to_string())
            .body(delivery.payload_json.clone())
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct TickReport {
    pub due: usize,
    pub delivered: usize,
    pub failed: usize,
    pub dead_lettered: usize,
}

/// Background loop that ticks the worker on `period` until aborted.
pub async fn run_loop(worker: WebhookWorker, period: StdDuration) {
    use tokio::time::{interval, MissedTickBehavior};
    tracing::info!(
        interval_secs = period.as_secs_f64(),
        "webhook worker started"
    );
    let mut tick = interval(period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let report = worker.tick(Utc::now()).await;
        if report.due > 0 || report.failed > 0 || report.dead_lettered > 0 {
            tracing::debug!(?report, "webhook worker tick");
        }
    }
}

// --------------------------------------------------------------------------
// SQLite row mapping
// --------------------------------------------------------------------------

#[allow(clippy::type_complexity)]
struct DeliveryRow {
    id: String,
    event_id: String,
    event_type: String,
    url: String,
    payload_json: String,
    status: String,
    attempts: i64,
    max_attempts: i64,
    next_attempt_at: String,
    last_status_code: Option<i64>,
    last_error: Option<String>,
    created_at: String,
    updated_at: String,
    delivered_at: Option<String>,
    tenant_id: String,
}

impl DeliveryRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            event_id: row.get(1)?,
            event_type: row.get(2)?,
            url: row.get(3)?,
            payload_json: row.get(4)?,
            status: row.get(5)?,
            attempts: row.get(6)?,
            max_attempts: row.get(7)?,
            next_attempt_at: row.get(8)?,
            last_status_code: row.get(9)?,
            last_error: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
            delivered_at: row.get(13)?,
            tenant_id: row.get(14).unwrap_or_default(),
        })
    }
}

impl WebhookDelivery {
    fn from_row(r: DeliveryRow) -> Self {
        let parse_dt = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now())
        };
        Self {
            id: r.id,
            event_id: r.event_id,
            event_type: r.event_type,
            url: r.url,
            payload_json: r.payload_json,
            status: DeliveryStatus::parse(&r.status),
            attempts: r.attempts.max(0) as u32,
            max_attempts: r.max_attempts.max(0) as u32,
            next_attempt_at: parse_dt(&r.next_attempt_at),
            last_status_code: r.last_status_code.and_then(|c| u16::try_from(c).ok()),
            last_error: r.last_error,
            created_at: parse_dt(&r.created_at),
            updated_at: parse_dt(&r.updated_at),
            delivered_at: r.delivered_at.as_deref().map(parse_dt),
            tenant_id: r.tenant_id,
        }
    }
}

// Suppress the deprecation: base64::Engine import is needed transitively.
#[allow(dead_code)]
fn _ensure_base64_used() {
    let _ = base64::engine::general_purpose::STANDARD.encode([0u8]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_roundtrip() {
        let body = br#"{"hello":"world"}"#;
        let header = sign("supersecret", 1_745_259_600, body);
        assert!(header.starts_with("t=1745259600,v1="));
        assert!(verify("supersecret", &header, body));
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let header = sign("k", 1_000, b"original");
        assert!(!verify("k", &header, b"tampered"));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let header = sign("k1", 1_000, b"x");
        assert!(!verify("k2", &header, b"x"));
    }

    #[test]
    fn backoff_grows_quadratically_then_caps() {
        assert_eq!(backoff_for(1).num_seconds(), 1);
        assert_eq!(backoff_for(2).num_seconds(), 4);
        assert_eq!(backoff_for(3).num_seconds(), 9);
        assert_eq!(backoff_for(60).num_seconds(), 3600);
        assert_eq!(backoff_for(1000).num_seconds(), 3600); // capped
    }

    #[test]
    fn outbox_in_memory_basic_lifecycle() {
        let outbox = WebhookOutbox::in_memory();
        let now = Utc::now();
        let d = WebhookDelivery {
            id: "del_1".into(),
            event_id: "evt_1".into(),
            event_type: "transaction.completed".into(),
            url: "http://localhost:9999/hook".into(),
            payload_json: "{}".into(),
            status: DeliveryStatus::Pending,
            attempts: 0,
            max_attempts: 3,
            next_attempt_at: now,
            last_status_code: None,
            last_error: None,
            created_at: now,
            updated_at: now,
            delivered_at: None,
            tenant_id: String::new(),
        };
        outbox.enqueue(d);
        assert_eq!(outbox.len(), 1);

        let due = outbox.list_due(now, 10);
        assert_eq!(due.len(), 1);

        outbox.mark_in_flight("del_1", now);
        outbox.mark_delivered("del_1", 200, now);
        let stored = outbox.get("del_1").unwrap();
        assert_eq!(stored.status, DeliveryStatus::Delivered);
        assert_eq!(stored.last_status_code, Some(200));
        assert_eq!(outbox.list_due(now, 10).len(), 0, "delivered → not due");
    }

    #[test]
    fn outbox_failure_then_dead_letter() {
        let outbox = WebhookOutbox::in_memory();
        let now = Utc::now();
        outbox.enqueue(WebhookDelivery {
            id: "del_2".into(),
            event_id: "e".into(),
            event_type: "t".into(),
            url: "u".into(),
            payload_json: "{}".into(),
            status: DeliveryStatus::Pending,
            attempts: 0,
            max_attempts: 2,
            next_attempt_at: now,
            last_status_code: None,
            last_error: None,
            created_at: now,
            updated_at: now,
            delivered_at: None,
            tenant_id: String::new(),
        });
        outbox.bump_failure("del_2", Some(500), Some("server".into()), now);
        assert_eq!(outbox.get("del_2").unwrap().status, DeliveryStatus::Failed);
        outbox.bump_failure("del_2", Some(500), Some("server".into()), now);
        assert_eq!(
            outbox.get("del_2").unwrap().status,
            DeliveryStatus::DeadLettered,
            "second failure exhausts max_attempts=2",
        );
    }
}
