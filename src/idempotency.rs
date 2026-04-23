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

#[derive(Debug, Clone, Copy)]
pub enum LookupOutcome {
    /// No entry — caller should process the request normally and call
    /// `store` with the result.
    Miss,
    /// Cached response found AND the request body digest matches —
    /// replay the cached response verbatim with `Idempotent-Replayed:
    /// true`.
    Replay,
    /// Cached response found but the request body digest differs from
    /// the original — caller MUST return `idempotency_conflict` (409)
    /// per ICP spec §12 / §13.
    Conflict,
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
    /// Returns the cached response on `Replay` so the caller can return
    /// it verbatim.
    pub fn lookup(
        &self,
        tenant_id: &str,
        idempotency_key: &str,
        request_digest: &str,
        now: DateTime<Utc>,
    ) -> (LookupOutcome, Option<CachedResponse>) {
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
                        return (LookupOutcome::Miss, None);
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
                        })
                    }
                    None => None,
                }
            }
        };

        let Some(entry) = entry else {
            return (LookupOutcome::Miss, None);
        };

        // TTL eviction is lazy — expired entries behave as Miss.
        if now - entry.created_at > self.ttl {
            return (LookupOutcome::Miss, None);
        }

        if entry.request_digest == request_digest {
            (LookupOutcome::Replay, Some(entry.response))
        } else {
            (LookupOutcome::Conflict, None)
        }
    }

    /// Store the response that was just produced for this key. Called
    /// only on the cache-miss path; replays don't re-store.
    pub fn store(
        &self,
        tenant_id: &str,
        idempotency_key: &str,
        request_digest: &str,
        response: CachedResponse,
        now: DateTime<Utc>,
    ) {
        let entry = Entry {
            request_digest: request_digest.to_string(),
            response: response.clone(),
            created_at: now,
        };
        match &self.backend {
            Backend::Memory(inner) => match inner.write() {
                Ok(mut guard) => {
                    guard.insert((tenant_id.to_string(), idempotency_key.to_string()), entry);
                }
                Err(err) => {
                    tracing::error!(%err, "idempotency write lock poisoned");
                }
            },
            Backend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "idempotency pool acquire failed");
                        return;
                    }
                };
                let body_str = String::from_utf8_lossy(&response.body_json).to_string();
                if let Err(err) = conn.execute(
                    "INSERT INTO idempotency \
                         (tenant_id, idempotency_key, request_digest, \
                          response_status, response_body, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(tenant_id, idempotency_key) DO UPDATE SET \
                         request_digest = excluded.request_digest, \
                         response_status = excluded.response_status, \
                         response_body = excluded.response_body, \
                         created_at = excluded.created_at",
                    rusqlite::params![
                        tenant_id,
                        idempotency_key,
                        request_digest,
                        response.status as i64,
                        body_str,
                        now.to_rfc3339(),
                    ],
                ) {
                    tracing::error!(%err, "idempotency write failed");
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
        let (outcome, body) = store.lookup("t1", "k1", &digest, now);
        assert!(matches!(outcome, LookupOutcome::Miss));
        assert!(body.is_none());

        // Store the response.
        let resp = CachedResponse {
            status: 200,
            body_json: b"{\"ok\":true}".to_vec(),
        };
        store.store("t1", "k1", &digest, resp.clone(), now);

        // Second call with same body: replay.
        let (outcome, body) = store.lookup("t1", "k1", &digest, now);
        assert!(matches!(outcome, LookupOutcome::Replay));
        let body = body.unwrap();
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
        );
        let (outcome, body) = store.lookup("t1", "same-key", &d2, now);
        assert!(matches!(outcome, LookupOutcome::Conflict));
        assert!(body.is_none());
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
        );
        let (outcome, _) = store.lookup("tenant_b", "k1", &d, now);
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
        );
        // 2 seconds later — past TTL.
        let later = now + Duration::seconds(2);
        let (outcome, _) = store.lookup("t", "k", &d, later);
        assert!(matches!(outcome, LookupOutcome::Miss));
    }
}
