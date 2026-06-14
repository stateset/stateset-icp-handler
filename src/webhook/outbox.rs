//! Durable webhook outbox.
//!
//! Writes happen synchronously inside the intent pipeline so an event
//! is durably enqueued before the response is sent. If the handler
//! crashes between the intent succeeding and the worker delivering,
//! the next process to come up resumes from the same outbox row —
//! events are at-least-once.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;

use crate::state_db::StatePool;

use super::types::{DeliveryStatus, PruneReport, RetryError, StatusCounts, WebhookDelivery};
use super::worker::backoff_for;

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
            Backend::Memory(inner) => match inner.write() {
                Ok(mut guard) => {
                    guard.insert(delivery.id.clone(), delivery);
                }
                Err(err) => {
                    tracing::error!(%err, "webhook outbox write lock poisoned");
                }
            },
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(id = %delivery.id, %err, "webhook outbox pool acquire failed");
                        return;
                    }
                };
                if let Err(err) = conn.execute(
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
                ) {
                    tracing::error!(%err, "webhook outbox enqueue failed");
                }
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.get(id).cloned(),
                Err(err) => {
                    tracing::error!(id, %err, "webhook outbox read lock poisoned");
                    None
                }
            },
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook outbox pool acquire failed");
                        return None;
                    }
                };
                let row: Option<DeliveryRow> = conn
                    .query_row(
                        DELIVERY_SELECT_BY_ID,
                        rusqlite::params![id],
                        DeliveryRow::from_row,
                    )
                    .optional()
                    .unwrap_or_else(|err| {
                        tracing::error!(id, %err, "webhook outbox read failed");
                        None
                    });
                row.map(WebhookDelivery::from_row)
            }
        }
    }

    /// Pending or Failed deliveries with `next_attempt_at <= now`. Limit
    /// caps the per-tick batch.
    pub fn list_due(&self, now: DateTime<Utc>, limit: usize) -> Vec<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut due: Vec<_> = match inner.read() {
                    Ok(guard) => guard
                        .values()
                        .filter(|d| {
                            matches!(d.status, DeliveryStatus::Pending | DeliveryStatus::Failed)
                                && d.next_attempt_at <= now
                        })
                        .cloned()
                        .collect(),
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox read lock poisoned");
                        Vec::new()
                    }
                };
                due.sort_by(|a, b| a.next_attempt_at.cmp(&b.next_attempt_at));
                due.truncate(limit);
                due
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox pool acquire failed");
                        return Vec::new();
                    }
                };
                let mut stmt = match conn.prepare(
                    "SELECT id, event_id, event_type, url, payload_json, status, \
                                attempts, max_attempts, next_attempt_at, \
                                last_status_code, last_error, \
                                created_at, updated_at, delivered_at, tenant_id \
                         FROM webhook_deliveries \
                         WHERE status IN ('pending','failed') \
                           AND next_attempt_at <= ?1 \
                         ORDER BY next_attempt_at ASC \
                         LIMIT ?2",
                ) {
                    Ok(stmt) => stmt,
                    Err(err) => {
                        tracing::error!(%err, "prepare webhook outbox due list failed");
                        return Vec::new();
                    }
                };
                let rows = match stmt.query_map(
                    rusqlite::params![now.to_rfc3339(), limit as i64],
                    DeliveryRow::from_row,
                ) {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::error!(%err, "query webhook outbox due list failed");
                        return Vec::new();
                    }
                };
                rows.filter_map(Result::ok)
                    .map(WebhookDelivery::from_row)
                    .collect()
            }
        }
    }

    pub fn list_recent(&self, limit: usize) -> Vec<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut all: Vec<_> = match inner.read() {
                    Ok(guard) => guard.values().cloned().collect(),
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox read lock poisoned");
                        Vec::new()
                    }
                };
                all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                all.truncate(limit);
                all
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox pool acquire failed");
                        return Vec::new();
                    }
                };
                let mut stmt = match conn.prepare(
                    "SELECT id, event_id, event_type, url, payload_json, status, \
                                attempts, max_attempts, next_attempt_at, \
                                last_status_code, last_error, \
                                created_at, updated_at, delivered_at, tenant_id \
                         FROM webhook_deliveries \
                         ORDER BY created_at DESC LIMIT ?1",
                ) {
                    Ok(stmt) => stmt,
                    Err(err) => {
                        tracing::error!(%err, "prepare webhook outbox recent list failed");
                        return Vec::new();
                    }
                };
                let rows =
                    match stmt.query_map(rusqlite::params![limit as i64], DeliveryRow::from_row) {
                        Ok(rows) => rows,
                        Err(err) => {
                            tracing::error!(%err, "query webhook outbox recent list failed");
                            return Vec::new();
                        }
                    };
                rows.filter_map(Result::ok)
                    .map(WebhookDelivery::from_row)
                    .collect()
            }
        }
    }

    /// Tenant-scoped variant of [`list_recent`]. Returns only rows
    /// whose `tenant_id` matches; optionally narrows by `status`.
    pub fn list_recent_for_tenant(
        &self,
        tenant_id: &str,
        limit: usize,
        status: Option<DeliveryStatus>,
    ) -> Vec<WebhookDelivery> {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut all: Vec<_> = match inner.read() {
                    Ok(guard) => guard
                        .values()
                        .filter(|d| d.tenant_id == tenant_id)
                        .filter(|d| status.is_none_or(|s| d.status == s))
                        .cloned()
                        .collect(),
                    Err(err) => {
                        tracing::error!(tenant_id, %err, "webhook outbox read lock poisoned");
                        Vec::new()
                    }
                };
                all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                all.truncate(limit);
                all
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(tenant_id, %err, "webhook outbox pool acquire failed");
                        return Vec::new();
                    }
                };
                let rows: Vec<DeliveryRow> = if let Some(status) = status {
                    let mut stmt = match conn.prepare(
                        "SELECT id, event_id, event_type, url, payload_json, status, \
                                    attempts, max_attempts, next_attempt_at, \
                                    last_status_code, last_error, \
                                    created_at, updated_at, delivered_at, tenant_id \
                             FROM webhook_deliveries \
                             WHERE tenant_id = ?1 AND status = ?2 \
                             ORDER BY created_at DESC LIMIT ?3",
                    ) {
                        Ok(stmt) => stmt,
                        Err(err) => {
                            tracing::error!(tenant_id, %err, "prepare webhook outbox tenant filtered list failed");
                            return Vec::new();
                        }
                    };
                    let mapped = match stmt.query_map(
                        rusqlite::params![tenant_id, status.wire_name(), limit as i64],
                        DeliveryRow::from_row,
                    ) {
                        Ok(mapped) => mapped,
                        Err(err) => {
                            tracing::error!(tenant_id, %err, "query webhook outbox tenant filtered list failed");
                            return Vec::new();
                        }
                    };
                    mapped.filter_map(Result::ok).collect()
                } else {
                    let mut stmt = match conn.prepare(
                        "SELECT id, event_id, event_type, url, payload_json, status, \
                                    attempts, max_attempts, next_attempt_at, \
                                    last_status_code, last_error, \
                                    created_at, updated_at, delivered_at, tenant_id \
                             FROM webhook_deliveries \
                             WHERE tenant_id = ?1 \
                             ORDER BY created_at DESC LIMIT ?2",
                    ) {
                        Ok(stmt) => stmt,
                        Err(err) => {
                            tracing::error!(tenant_id, %err, "prepare webhook outbox tenant list failed");
                            return Vec::new();
                        }
                    };
                    let mapped = match stmt.query_map(
                        rusqlite::params![tenant_id, limit as i64],
                        DeliveryRow::from_row,
                    ) {
                        Ok(mapped) => mapped,
                        Err(err) => {
                            tracing::error!(tenant_id, %err, "query webhook outbox tenant list failed");
                            return Vec::new();
                        }
                    };
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

    /// Reclaim deliveries stranded in `in_flight`.
    ///
    /// A row is flipped to `in_flight` immediately before the (slow,
    /// network) delivery POST. If the process dies or is restarted
    /// between that flip and the terminal `mark_delivered` /
    /// `bump_failure`, the row is stuck: `list_due` only selects
    /// `pending`/`failed`, and the operator retry path refuses
    /// `in_flight`. The delivery would then be silently lost — breaking
    /// the at-least-once guarantee at exactly the moment (restart) it
    /// matters most.
    ///
    /// Called once on worker startup. The single worker owns the table,
    /// so any `in_flight` row at startup is necessarily orphaned: reset
    /// it to `pending` and make it immediately due. `attempts` is left
    /// untouched (the interrupted attempt never reached a terminal
    /// transition, so it was never counted). Returns the number of
    /// reclaimed rows.
    pub fn reclaim_in_flight(&self, now: DateTime<Utc>) -> usize {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut guard = match inner.write() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox write lock poisoned");
                        return 0;
                    }
                };
                let mut count = 0;
                for d in guard.values_mut() {
                    if matches!(d.status, DeliveryStatus::InFlight) {
                        d.status = DeliveryStatus::Pending;
                        d.next_attempt_at = now;
                        d.updated_at = now;
                        count += 1;
                    }
                }
                count
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox pool acquire failed");
                        return 0;
                    }
                };
                match conn.execute(
                    "UPDATE webhook_deliveries \
                     SET status = 'pending', next_attempt_at = ?1, updated_at = ?1 \
                     WHERE status = 'in_flight'",
                    rusqlite::params![now.to_rfc3339()],
                ) {
                    Ok(n) => n,
                    Err(err) => {
                        tracing::error!(%err, "webhook in_flight reclaim failed");
                        0
                    }
                }
            }
        }
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
                let mut guard = match inner.write() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook outbox write lock poisoned");
                        return;
                    }
                };
                if let Some(d) = guard.get_mut(id) {
                    f(d);
                }
            }
            Backend::Sqlite(pool) => {
                let mut conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook outbox pool acquire failed");
                        return;
                    }
                };
                let tx = match conn.transaction() {
                    Ok(tx) => tx,
                    Err(err) => {
                        tracing::error!(id, %err, "begin webhook outbox transaction failed");
                        return;
                    }
                };
                let row: Option<DeliveryRow> = tx
                    .query_row(
                        DELIVERY_SELECT_BY_ID,
                        rusqlite::params![id],
                        DeliveryRow::from_row,
                    )
                    .optional()
                    .unwrap_or_else(|err| {
                        tracing::error!(id, %err, "webhook outbox read-for-update failed");
                        None
                    });
                let Some(row) = row else { return };
                let mut delivery = WebhookDelivery::from_row(row);
                f(&mut delivery);
                if let Err(err) = tx.execute(
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
                ) {
                    tracing::error!(id, %err, "webhook outbox update failed");
                    return;
                }
                if let Err(err) = tx.commit() {
                    tracing::error!(id, %err, "webhook outbox commit failed");
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.backend {
            Backend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.len(),
                Err(err) => {
                    tracing::error!(%err, "webhook outbox read lock poisoned");
                    0
                }
            },
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox pool acquire failed");
                        return 0;
                    }
                };
                conn.query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as usize)
                .unwrap_or_else(|err| {
                    tracing::error!(%err, "webhook outbox count failed");
                    0
                })
            }
        }
    }

    /// Snapshot of queue depth by status. Used to update the
    /// Prometheus `icp_webhook_outbox_queue_depth` gauge each worker
    /// tick — operators dashboard the `pending` and `dead_lettered`
    /// series to see backlog and to alert on dead-letter growth.
    pub fn status_counts(&self) -> StatusCounts {
        match &self.backend {
            Backend::Memory(inner) => {
                let guard = match inner.read() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox read lock poisoned");
                        return StatusCounts::default();
                    }
                };
                let mut counts = StatusCounts::default();
                for d in guard.values() {
                    match d.status {
                        DeliveryStatus::Pending => counts.pending += 1,
                        DeliveryStatus::InFlight => counts.in_flight += 1,
                        DeliveryStatus::Delivered => counts.delivered += 1,
                        DeliveryStatus::Failed => counts.failed += 1,
                        DeliveryStatus::DeadLettered => counts.dead_lettered += 1,
                    }
                }
                counts
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox pool acquire failed");
                        return StatusCounts::default();
                    }
                };
                let mut stmt = match conn
                    .prepare("SELECT status, COUNT(*) FROM webhook_deliveries GROUP BY status")
                {
                    Ok(stmt) => stmt,
                    Err(err) => {
                        tracing::error!(%err, "prepare webhook outbox status_counts failed");
                        return StatusCounts::default();
                    }
                };
                let rows = match stmt.query_map([], |row| {
                    let s: String = row.get(0)?;
                    let n: i64 = row.get(1)?;
                    Ok((s, n))
                }) {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::error!(%err, "query webhook outbox status_counts failed");
                        return StatusCounts::default();
                    }
                };
                let mut counts = StatusCounts::default();
                for r in rows.flatten() {
                    let (status, n) = r;
                    let n = n.max(0) as usize;
                    match status.as_str() {
                        "pending" => counts.pending = n,
                        "in_flight" => counts.in_flight = n,
                        "delivered" => counts.delivered = n,
                        "failed" => counts.failed = n,
                        "dead_lettered" => counts.dead_lettered = n,
                        // Unknown status values shouldn't happen — the FSM
                        // only writes the five names above. Any drift is
                        // silently ignored here so a single bad row can't
                        // panic the metrics tick.
                        _ => {}
                    }
                }
                counts
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Prune `delivered` rows older than `delivered_cutoff` and
    /// `dead_lettered` rows older than `dead_lettered_cutoff`.
    /// `pending`, `in_flight`, and `failed` rows are NEVER pruned —
    /// those are still in the FSM and the worker would lose work.
    /// Pass `None` for either cutoff to skip that status. Cutoffs
    /// compare against `created_at` so rows are aged by enqueue time,
    /// not by their last status flip (the operator-meaningful
    /// retention is "how long has this row existed", not "how long
    /// since it last changed state").
    pub fn prune(
        &self,
        delivered_cutoff: Option<DateTime<Utc>>,
        dead_lettered_cutoff: Option<DateTime<Utc>>,
    ) -> PruneReport {
        let mut report = PruneReport::default();
        match &self.backend {
            Backend::Memory(inner) => {
                let mut guard = match inner.write() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox write lock poisoned");
                        return report;
                    }
                };
                guard.retain(|_id, d| {
                    let drop_for_delivered = matches!(d.status, DeliveryStatus::Delivered)
                        && delivered_cutoff.is_some_and(|c| d.created_at < c);
                    let drop_for_dead = matches!(d.status, DeliveryStatus::DeadLettered)
                        && dead_lettered_cutoff.is_some_and(|c| d.created_at < c);
                    if drop_for_delivered {
                        report.delivered_pruned += 1;
                    }
                    if drop_for_dead {
                        report.dead_lettered_pruned += 1;
                    }
                    !(drop_for_delivered || drop_for_dead)
                });
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "webhook outbox pool acquire failed");
                        return report;
                    }
                };
                if let Some(cutoff) = delivered_cutoff {
                    let n = conn
                        .execute(
                            "DELETE FROM webhook_deliveries \
                             WHERE status = 'delivered' AND created_at < ?1",
                            rusqlite::params![cutoff.to_rfc3339()],
                        )
                        .unwrap_or_else(|err| {
                            tracing::error!(%err, "webhook outbox delivered prune failed");
                            0
                        });
                    report.delivered_pruned = n;
                }
                if let Some(cutoff) = dead_lettered_cutoff {
                    let n = conn
                        .execute(
                            "DELETE FROM webhook_deliveries \
                             WHERE status = 'dead_lettered' AND created_at < ?1",
                            rusqlite::params![cutoff.to_rfc3339()],
                        )
                        .unwrap_or_else(|err| {
                            tracing::error!(%err, "webhook outbox dead-letter prune failed");
                            0
                        });
                    report.dead_lettered_pruned = n;
                }
            }
        }
        report
    }
}

const DELIVERY_SELECT_BY_ID: &str = "SELECT id, event_id, event_type, url, payload_json, status, \
                attempts, max_attempts, next_attempt_at, \
                last_status_code, last_error, \
                created_at, updated_at, delivered_at, tenant_id \
         FROM webhook_deliveries WHERE id = ?1";

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
