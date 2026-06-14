//! Idempotency cache (ICP spec §13).
//!
//! Stores the JSON body + HTTP status code returned for a given
//! `(tenant_id, idempotency_key)` pair, plus a SHA-256 over the
//! JCS-canonicalized request envelope. On a retry:
//!
//!   * **Hit, body matches** → replay the cached response verbatim. The
//!     handler stamps `Idempotent-Replayed: true` on the way out so
//!     callers can tell. This is the case that prevents network-retry
//!     double-charges.
//!   * **Hit, body differs** → `idempotency_conflict` (HTTP 409). Spec
//!     forbids reusing a key across different request bodies.
//!   * **Miss** → caller proceeds normally; a successful response is
//!     stored before being returned.
//!
//! Backed either by an in-memory `HashMap` (tests, ephemeral demos) or
//! by the shared SQLite state pool. The SQLite backend is the only safe
//! choice in production: a restart between the original request and the
//! retry must NOT lose the cached response, otherwise the retry happily
//! double-charges.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::state_db::StatePool;

/// Maximum age of a cached idempotency entry. Reads older than this
/// are treated as misses; the row is reclaimed by the next sweeper
/// tick (see [`run_sweeper_loop`]).
pub const DEFAULT_TTL_HOURS: i64 = 24;

/// Default cadence for the active sweeper, in seconds. 1 hour is a
/// good balance: shorter cadences burn DB ops for no operator
/// benefit (the lazy TTL at lookup time already prevents wrong
/// replays); longer cadences let the table grow more between sweeps.
pub const DEFAULT_SWEEPER_INTERVAL_SECS: u64 = 3600;

#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    /// JCS-canonicalized JSON body, exactly as the original response
    /// was serialized.
    pub body_json: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum LookupOutcome {
    /// No entry — caller should process the request normally and call
    /// `store` with the result.
    Miss,
    /// Cached response found AND the request body digest matches —
    /// replay the carried response verbatim with `Idempotent-Replayed:
    /// true`. The response is bound to the variant so a `Replay` can
    /// never exist without its body (the invariant is enforced by the
    /// type, not by a runtime `expect` at every call site).
    Replay(CachedResponse),
    /// Cached response found but the request body digest differs from
    /// the original — caller MUST return `idempotency_conflict` (409)
    /// per ICP spec §12 / §13.
    Conflict,
}

/// `response_status` sentinel marking a reservation row that has been
/// claimed for execution but not yet completed. No real HTTP response
/// uses status 0, so it unambiguously means "in progress".
const PENDING_STATUS: u16 = 0;

/// The effect of attempting to atomically *claim* an idempotency key
/// before executing (see [`IdempotencyStore::reserve`]). Unlike
/// [`LookupOutcome`], this distinguishes "nobody is executing, I won the
/// claim" from "another worker is currently executing this key" — the
/// distinction that makes idempotency correct across *multiple* handler
/// processes sharing one database, not just within one process.
#[derive(Debug, Clone)]
pub enum ReserveOutcome {
    /// This caller atomically claimed the key and must now execute the
    /// request, then call [`IdempotencyStore::store`] on success or
    /// [`IdempotencyStore::release`] on failure — passing back the carried
    /// reservation token so a superseded holder can't overwrite the result.
    Won(String),
    /// A completed response already exists with a matching digest —
    /// replay it.
    Replay(CachedResponse),
    /// A row exists with a different request digest — 409 conflict.
    Conflict,
    /// Another worker holds an unexpired reservation for this key and is
    /// executing it right now. The caller should briefly wait for the
    /// result, then replay it (or return a retryable error).
    InProgress,
}

#[derive(Clone)]
pub struct IdempotencyStore {
    backend: Backend,
    ttl: Duration,
}

#[derive(Clone)]
enum Backend {
    Memory(Arc<RwLock<HashMap<(String, String), Entry>>>),
    Sqlite(StatePool),
}

#[derive(Clone)]
struct Entry {
    request_digest: String,
    response: CachedResponse,
    created_at: DateTime<Utc>,
    /// Owner/fencing token, set when the row is claimed as a reservation.
    /// `store`/`release` only act when their token matches, so a
    /// superseded (stale-lease takeover) holder can't clobber the taker's
    /// result. Empty for legacy/completed rows written before fencing.
    reservation_id: String,
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl IdempotencyStore {
    pub fn in_memory() -> Self {
        Self {
            backend: Backend::Memory(Arc::new(RwLock::new(HashMap::new()))),
            ttl: Duration::hours(DEFAULT_TTL_HOURS),
        }
    }

    pub fn with_pool(pool: StatePool) -> Self {
        Self {
            backend: Backend::Sqlite(pool),
            ttl: Duration::hours(DEFAULT_TTL_HOURS),
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Compute the SHA-256 digest of a request envelope (already
    /// JCS-canonicalized to bytes by the caller).
    pub fn digest_request(canonical_body: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(canonical_body);
        hex::encode(h.finalize())
    }

    /// Decide what to do with a request bearing this idempotency key.
    /// Returns `LookupOutcome::Replay(response)` carrying the cached
    /// response so the caller can return it verbatim.
    pub fn lookup(
        &self,
        tenant_id: &str,
        idempotency_key: &str,
        request_digest: &str,
        now: DateTime<Utc>,
    ) -> LookupOutcome {
        let entry = match &self.backend {
            Backend::Memory(inner) => match inner.read() {
                Ok(guard) => guard
                    .get(&(tenant_id.to_string(), idempotency_key.to_string()))
                    .cloned(),
                Err(err) => {
                    tracing::error!(%err, "idempotency read lock poisoned");
                    None
                }
            },
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "idempotency pool acquire failed");
                        return LookupOutcome::Miss;
                    }
                };
                let row: Option<(String, i64, String, String)> = conn
                    .query_row(
                        "SELECT request_digest, response_status, response_body, created_at \
                         FROM idempotency \
                         WHERE tenant_id = ?1 AND idempotency_key = ?2",
                        rusqlite::params![tenant_id, idempotency_key],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                    .unwrap_or_else(|err| {
                        tracing::error!(%err, "idempotency read failed");
                        None
                    });
                match row {
                    Some((digest, status, body, created)) => {
                        let created_at = DateTime::parse_from_rfc3339(&created)
                            .map(|d| d.with_timezone(&Utc))
                            .unwrap_or(now);
                        Some(Entry {
                            request_digest: digest,
                            response: CachedResponse {
                                status: status as u16,
                                body_json: body.into_bytes(),
                            },
                            created_at,
                            // lookup never inspects the token; default empty.
                            reservation_id: String::new(),
                        })
                    }
                    None => None,
                }
            }
        };

        let Some(entry) = entry else {
            return LookupOutcome::Miss;
        };

        // TTL eviction is lazy — expired entries behave as Miss.
        if now - entry.created_at > self.ttl {
            return LookupOutcome::Miss;
        }

        // A reservation placeholder (status 0) is an in-progress claim, not
        // a completed response — it is not replayable.
        if entry.response.status == PENDING_STATUS {
            return LookupOutcome::Miss;
        }

        if entry.request_digest == request_digest {
            LookupOutcome::Replay(entry.response)
        } else {
            LookupOutcome::Conflict
        }
    }

    /// Atomically claim a key before executing, so that concurrent
    /// duplicates — even across separate handler processes sharing one
    /// database — cannot both execute (the double-charge the in-process
    /// lock alone cannot prevent in a multi-replica fleet).
    ///
    /// On the SQLite backend the read-decide-write runs inside an
    /// `IMMEDIATE` transaction, which takes the database write lock, so
    /// exactly one caller across the fleet can claim a given key at a time.
    ///
    /// `lease` bounds crash recovery: a reservation whose holder died is
    /// reclaimable once it is older than the lease. Keep the lease well
    /// above the slowest expected intent latency — too short and a slow
    /// (not dead) holder's key could be re-claimed and executed twice.
    pub fn reserve(
        &self,
        tenant_id: &str,
        idempotency_key: &str,
        request_digest: &str,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> ReserveOutcome {
        // Fresh fencing token, written only if this call claims the key.
        let token = uuid::Uuid::new_v4().simple().to_string();
        let pending_entry = |reservation_id: String| Entry {
            request_digest: request_digest.to_string(),
            response: CachedResponse {
                status: PENDING_STATUS,
                body_json: Vec::new(),
            },
            created_at: now,
            reservation_id,
        };
        match &self.backend {
            Backend::Memory(inner) => {
                let mut guard = inner
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let key = (tenant_id.to_string(), idempotency_key.to_string());
                match guard.get(&key) {
                    // Fresh, completed, matching → replay; mismatching → conflict.
                    Some(e) if now - e.created_at <= self.ttl => {
                        if e.response.status == PENDING_STATUS {
                            if now - e.created_at > lease {
                                guard.insert(key, pending_entry(token.clone())); // stale → take over
                                ReserveOutcome::Won(token)
                            } else {
                                ReserveOutcome::InProgress
                            }
                        } else if e.request_digest == request_digest {
                            ReserveOutcome::Replay(e.response.clone())
                        } else {
                            ReserveOutcome::Conflict
                        }
                    }
                    // Absent or TTL-expired → claim it.
                    _ => {
                        guard.insert(key, pending_entry(token.clone()));
                        ReserveOutcome::Won(token)
                    }
                }
            }
            Backend::Sqlite(pool) => {
                let mut conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "idempotency pool acquire failed");
                        // Fail closed: tell the caller to retry rather than
                        // risk a second execution.
                        return ReserveOutcome::InProgress;
                    }
                };
                let tx = match conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                {
                    Ok(tx) => tx,
                    Err(err) => {
                        tracing::error!(%err, "idempotency reserve begin failed");
                        return ReserveOutcome::InProgress;
                    }
                };
                let existing: Option<(String, i64, String, String)> = tx
                    .query_row(
                        "SELECT request_digest, response_status, response_body, created_at \
                         FROM idempotency WHERE tenant_id = ?1 AND idempotency_key = ?2",
                        rusqlite::params![tenant_id, idempotency_key],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                    .unwrap_or_else(|err| {
                        tracing::error!(%err, "idempotency reserve read failed");
                        None
                    });

                let claim_pending = |tx: &rusqlite::Transaction<'_>| {
                    tx.execute(
                        "INSERT INTO idempotency \
                             (tenant_id, idempotency_key, request_digest, \
                              response_status, response_body, created_at, reservation_id) \
                         VALUES (?1, ?2, ?3, 0, '', ?4, ?5) \
                         ON CONFLICT(tenant_id, idempotency_key) DO UPDATE SET \
                             request_digest = excluded.request_digest, \
                             response_status = 0, response_body = '', \
                             created_at = excluded.created_at, \
                             reservation_id = excluded.reservation_id",
                        rusqlite::params![
                            tenant_id,
                            idempotency_key,
                            request_digest,
                            now.to_rfc3339(),
                            token,
                        ],
                    )
                };
                let won = |r: rusqlite::Result<usize>| match r {
                    Ok(_) => ReserveOutcome::Won(token.clone()),
                    Err(err) => {
                        tracing::error!(%err, "idempotency reserve claim failed");
                        ReserveOutcome::InProgress
                    }
                };

                let outcome = match existing {
                    None => won(claim_pending(&tx)),
                    Some((digest, status, body, created_str)) => {
                        let created = DateTime::parse_from_rfc3339(&created_str)
                            .map(|d| d.with_timezone(&Utc))
                            .unwrap_or(now);
                        let expired = now - created > self.ttl;
                        let status = status as u16;
                        if expired {
                            // Stale completed/pending row → reclaim.
                            won(claim_pending(&tx))
                        } else if status == PENDING_STATUS {
                            if now - created > lease {
                                won(claim_pending(&tx)) // dead holder → take over
                            } else {
                                ReserveOutcome::InProgress
                            }
                        } else if digest == request_digest {
                            ReserveOutcome::Replay(CachedResponse {
                                status,
                                body_json: body.into_bytes(),
                            })
                        } else {
                            ReserveOutcome::Conflict
                        }
                    }
                };

                if let Err(err) = tx.commit() {
                    tracing::error!(%err, "idempotency reserve commit failed");
                    return ReserveOutcome::InProgress;
                }
                outcome
            }
        }
    }

    /// Release a reservation whose execution failed, so the request can be
    /// retried immediately instead of waiting out the lease. Only deletes a
    /// still-pending row that this caller still owns (matching
    /// `reservation_id`) — never a completed response, and never a row a
    /// stale-lease takeover has since re-claimed.
    pub fn release(&self, tenant_id: &str, idempotency_key: &str, reservation_id: &str) {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut guard = inner
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let key = (tenant_id.to_string(), idempotency_key.to_string());
                if guard.get(&key).is_some_and(|e| {
                    e.response.status == PENDING_STATUS && e.reservation_id == reservation_id
                }) {
                    guard.remove(&key);
                }
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "idempotency pool acquire failed");
                        return;
                    }
                };
                if let Err(err) = conn.execute(
                    "DELETE FROM idempotency \
                     WHERE tenant_id = ?1 AND idempotency_key = ?2 \
                       AND response_status = 0 AND reservation_id = ?3",
                    rusqlite::params![tenant_id, idempotency_key, reservation_id],
                ) {
                    tracing::error!(%err, "idempotency release failed");
                }
            }
        }
    }

    /// Record the response produced for a key this caller reserved. The
    /// write only lands if the caller still owns the reservation
    /// (`reservation_id` matches) — if a stale-lease takeover re-claimed
    /// the key, this caller was superseded and the write is dropped
    /// (logged) rather than clobbering the taker's result. Called only on
    /// the reserve-`Won` path; replays don't re-store.
    pub fn store(
        &self,
        tenant_id: &str,
        idempotency_key: &str,
        request_digest: &str,
        response: CachedResponse,
        now: DateTime<Utc>,
        reservation_id: &str,
    ) {
        match &self.backend {
            Backend::Memory(inner) => {
                let mut guard = inner
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let key = (tenant_id.to_string(), idempotency_key.to_string());
                let owns_or_absent = guard
                    .get(&key)
                    .map(|e| e.reservation_id == reservation_id)
                    .unwrap_or(true);
                if owns_or_absent {
                    guard.insert(
                        key,
                        Entry {
                            request_digest: request_digest.to_string(),
                            response,
                            created_at: now,
                            reservation_id: String::new(),
                        },
                    );
                } else {
                    crate::metrics::record_idempotency_takeover();
                    tracing::warn!(
                        "idempotency store superseded by a takeover; response not cached"
                    );
                }
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "idempotency pool acquire failed");
                        return;
                    }
                };
                let body_str = String::from_utf8_lossy(&response.body_json).to_string();
                // Insert when absent; on conflict, complete (and clear the
                // token) ONLY the reservation we still own — the `WHERE` on the
                // upsert's UPDATE makes a superseded holder's write a no-op
                // instead of clobbering the stale-lease taker's response.
                match conn.execute(
                    "INSERT INTO idempotency \
                         (tenant_id, idempotency_key, request_digest, \
                          response_status, response_body, created_at, reservation_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, '') \
                     ON CONFLICT(tenant_id, idempotency_key) DO UPDATE SET \
                         request_digest = excluded.request_digest, \
                         response_status = excluded.response_status, \
                         response_body = excluded.response_body, \
                         created_at = excluded.created_at, \
                         reservation_id = '' \
                     WHERE idempotency.reservation_id = ?7",
                    rusqlite::params![
                        tenant_id,
                        idempotency_key,
                        request_digest,
                        response.status as i64,
                        body_str,
                        now.to_rfc3339(),
                        reservation_id,
                    ],
                ) {
                    Ok(0) => {
                        crate::metrics::record_idempotency_takeover();
                        tracing::warn!(
                            tenant_id,
                            "idempotency store superseded by a stale-lease takeover; response not cached"
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::error!(%err, "idempotency write failed"),
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.backend {
            Backend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.len(),
                Err(err) => {
                    tracing::error!(%err, "idempotency read lock poisoned");
                    0
                }
            },
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "idempotency pool acquire failed");
                        return 0;
                    }
                };
                conn.query_row("SELECT COUNT(*) FROM idempotency", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as usize)
                .unwrap_or_else(|err| {
                    tracing::error!(%err, "idempotency count failed");
                    0
                })
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Active eviction sweep — DELETE entries whose `created_at` is
    /// older than the configured TTL relative to `now`. Without this
    /// the table grows unbounded: lazy TTL eviction at lookup time
    /// stops a stale entry from being replayed but never reclaims
    /// the row. Returns the number of entries removed so the caller
    /// can update the
    /// `icp_idempotency_pruned_total` counter.
    pub fn prune(&self, now: DateTime<Utc>) -> usize {
        let cutoff = now - self.ttl;
        match &self.backend {
            Backend::Memory(inner) => {
                let mut guard = match inner.write() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(%err, "idempotency write lock poisoned");
                        return 0;
                    }
                };
                let before = guard.len();
                guard.retain(|_k, e| e.created_at >= cutoff);
                before - guard.len()
            }
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "idempotency pool acquire failed");
                        return 0;
                    }
                };
                conn.execute(
                    "DELETE FROM idempotency WHERE created_at < ?1",
                    rusqlite::params![cutoff.to_rfc3339()],
                )
                .unwrap_or_else(|err| {
                    tracing::error!(%err, "idempotency prune failed");
                    0
                })
            }
        }
    }
}

/// Background TTL sweeper. Designed to be `tokio::spawn`ed alongside
/// the webhook + scheduler workers. Calls `IdempotencyStore::prune`
/// at the configured cadence and bumps the
/// `icp_idempotency_pruned_total` counter with whatever it removes.
pub async fn run_sweeper_loop(store: IdempotencyStore, period: std::time::Duration) {
    use tokio::time::{interval, MissedTickBehavior};
    tracing::info!(
        interval_secs = period.as_secs_f64(),
        ttl_hours = store.ttl.num_hours(),
        "idempotency sweeper started"
    );
    let mut tick = interval(period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let pruned = store.prune(Utc::now());
        crate::metrics::record_idempotency_sweep(pruned);
        if pruned > 0 {
            tracing::debug!(pruned, "idempotency sweeper tick");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miss_then_store_then_replay() {
        let store = IdempotencyStore::in_memory();
        let now = Utc::now();
        let digest = IdempotencyStore::digest_request(b"hello");

        // First call: miss.
        let outcome = store.lookup("t1", "k1", &digest, now);
        assert!(matches!(outcome, LookupOutcome::Miss));

        // Store the response.
        let resp = CachedResponse {
            status: 200,
            body_json: b"{\"ok\":true}".to_vec(),
        };
        store.store("t1", "k1", &digest, resp.clone(), now, "");

        // Second call with same body: replay carries the cached body.
        let outcome = store.lookup("t1", "k1", &digest, now);
        let LookupOutcome::Replay(body) = outcome else {
            panic!("expected Replay, got {outcome:?}");
        };
        assert_eq!(body.status, 200);
        assert_eq!(body.body_json, b"{\"ok\":true}");
    }

    #[test]
    fn conflict_when_body_changes() {
        let store = IdempotencyStore::in_memory();
        let now = Utc::now();
        let d1 = IdempotencyStore::digest_request(b"first");
        let d2 = IdempotencyStore::digest_request(b"second");
        store.store(
            "t1",
            "same-key",
            &d1,
            CachedResponse {
                status: 200,
                body_json: b"{}".to_vec(),
            },
            now,
            "",
        );
        let outcome = store.lookup("t1", "same-key", &d2, now);
        assert!(matches!(outcome, LookupOutcome::Conflict));
    }

    #[test]
    fn distinct_tenants_do_not_collide() {
        let store = IdempotencyStore::in_memory();
        let now = Utc::now();
        let d = IdempotencyStore::digest_request(b"x");
        store.store(
            "tenant_a",
            "k1",
            &d,
            CachedResponse {
                status: 200,
                body_json: b"a".to_vec(),
            },
            now,
            "",
        );
        let outcome = store.lookup("tenant_b", "k1", &d, now);
        assert!(matches!(outcome, LookupOutcome::Miss));
    }

    #[test]
    fn ttl_eviction_treats_old_entries_as_miss() {
        let store = IdempotencyStore::in_memory().with_ttl(Duration::seconds(1));
        let now = Utc::now();
        let d = IdempotencyStore::digest_request(b"x");
        store.store(
            "t",
            "k",
            &d,
            CachedResponse {
                status: 200,
                body_json: b"x".to_vec(),
            },
            now,
            "",
        );
        // 2 seconds later — past TTL.
        let later = now + Duration::seconds(2);
        let outcome = store.lookup("t", "k", &d, later);
        assert!(matches!(outcome, LookupOutcome::Miss));
    }
}
