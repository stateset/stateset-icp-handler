//! Per-tenant webhook subscriber registry.
//!
//! Subscribers register one or more destinations per tenant via the
//! admin endpoints. The outbox enqueues one `WebhookDelivery` per
//! active subscriber whose `tenant_id` matches the originating tenant.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::state_db::StatePool;

/// One registered destination for a tenant's webhook events.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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
            SubBackend::Memory(inner) => match inner.write() {
                Ok(mut guard) => {
                    guard.insert(sub.id.clone(), sub);
                }
                Err(err) => {
                    tracing::error!(%err, "webhook subscriber write lock poisoned");
                }
            },
            SubBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(id = %sub.id, %err, "webhook subscriber pool acquire failed");
                        return;
                    }
                };
                if let Err(err) = conn.execute(
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
                ) {
                    tracing::error!(%err, "webhook subscriber insert failed");
                }
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<WebhookSubscriber> {
        match &self.backend {
            SubBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.get(id).cloned(),
                Err(err) => {
                    tracing::error!(%err, "webhook subscriber read lock poisoned");
                    None
                }
            },
            SubBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook subscriber pool acquire failed");
                        return None;
                    }
                };
                conn.query_row(
                    "SELECT id, tenant_id, url, secret, active, created_at, updated_at \
                     FROM webhook_subscribers WHERE id = ?1",
                    rusqlite::params![id],
                    Self::row_to_subscriber,
                )
                .optional()
                .unwrap_or_else(|err| {
                    tracing::error!(id, %err, "webhook subscriber read failed");
                    None
                })
            }
        }
    }

    /// All subscribers belonging to `tenant_id`, regardless of active
    /// state. Used by the `GET /icp/v1/webhook_subscribers` endpoint.
    pub fn list_for_tenant(&self, tenant_id: &str) -> Vec<WebhookSubscriber> {
        match &self.backend {
            SubBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard
                    .values()
                    .filter(|s| s.tenant_id == tenant_id)
                    .cloned()
                    .collect(),
                Err(err) => {
                    tracing::error!(tenant_id, %err, "webhook subscriber read lock poisoned");
                    Vec::new()
                }
            },
            SubBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(tenant_id, %err, "webhook subscriber pool acquire failed");
                        return Vec::new();
                    }
                };
                let mut stmt = match conn.prepare(
                    "SELECT id, tenant_id, url, secret, active, created_at, updated_at \
                         FROM webhook_subscribers WHERE tenant_id = ?1 \
                         ORDER BY created_at DESC",
                ) {
                    Ok(stmt) => stmt,
                    Err(err) => {
                        tracing::error!(tenant_id, %err, "prepare webhook subscriber tenant list failed");
                        return Vec::new();
                    }
                };
                let rows = match stmt
                    .query_map(rusqlite::params![tenant_id], Self::row_to_subscriber)
                {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::error!(tenant_id, %err, "query webhook subscriber tenant list failed");
                        return Vec::new();
                    }
                };
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
                let mut guard = match inner.write() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook subscriber write lock poisoned");
                        return None;
                    }
                };
                let s = guard.get_mut(id)?;
                s.active = active;
                s.updated_at = now;
                Some(s.clone())
            }
            SubBackend::Sqlite(pool) => {
                let updated = {
                    let conn = match pool.get() {
                        Ok(conn) => conn,
                        Err(err) => {
                            tracing::error!(id, %err, "webhook subscriber pool acquire failed");
                            return None;
                        }
                    };
                    match conn.execute(
                        "UPDATE webhook_subscribers SET active = ?1, updated_at = ?2 \
                         WHERE id = ?3",
                        rusqlite::params![i64::from(active), now.to_rfc3339(), id],
                    ) {
                        Ok(updated) => updated,
                        Err(err) => {
                            tracing::error!(id, %err, "webhook subscriber set_active failed");
                            return None;
                        }
                    }
                };
                if updated == 0 {
                    None
                } else {
                    self.get(id)
                }
            }
        }
    }

    /// In-place mutation of a subscriber's `url` and/or `secret`.
    /// `None` for a field leaves it unchanged. Returns the updated
    /// row, or `None` if the id doesn't exist. Used by the
    /// `PATCH /icp/v1/webhook_subscribers/:id` endpoint to support
    /// secret rotation and URL updates without forcing a delete +
    /// recreate (which would rotate the id and orphan the
    /// downstream verifier configuration).
    pub fn patch(
        &self,
        id: &str,
        url: Option<&str>,
        secret: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<WebhookSubscriber> {
        match &self.backend {
            SubBackend::Memory(inner) => {
                let mut guard = match inner.write() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook subscriber write lock poisoned");
                        return None;
                    }
                };
                let s = guard.get_mut(id)?;
                if let Some(u) = url {
                    s.url = u.to_string();
                }
                if let Some(sec) = secret {
                    s.secret = Some(sec.to_string());
                }
                s.updated_at = now;
                Some(s.clone())
            }
            SubBackend::Sqlite(pool) => {
                // Read-modify-write so the per-field "None means
                // leave alone" semantics match in-memory exactly,
                // without writing five SQL UPDATE permutations.
                let mut current = self.get(id)?;
                if let Some(u) = url {
                    current.url = u.to_string();
                }
                if let Some(sec) = secret {
                    current.secret = Some(sec.to_string());
                }
                current.updated_at = now;
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook subscriber pool acquire failed");
                        return None;
                    }
                };
                let n = match conn.execute(
                    "UPDATE webhook_subscribers \
                         SET url = ?1, secret = ?2, updated_at = ?3 \
                         WHERE id = ?4",
                    rusqlite::params![
                        current.url,
                        current.secret.clone().unwrap_or_default(),
                        current.updated_at.to_rfc3339(),
                        id,
                    ],
                ) {
                    Ok(n) => n,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook subscriber patch failed");
                        return None;
                    }
                };
                if n == 0 {
                    None
                } else {
                    Some(current)
                }
            }
        }
    }

    pub fn delete(&self, id: &str) -> bool {
        match &self.backend {
            SubBackend::Memory(inner) => match inner.write() {
                Ok(mut guard) => guard.remove(id).is_some(),
                Err(err) => {
                    tracing::error!(id, %err, "webhook subscriber write lock poisoned");
                    false
                }
            },
            SubBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(id, %err, "webhook subscriber pool acquire failed");
                        return false;
                    }
                };
                conn.execute(
                    "DELETE FROM webhook_subscribers WHERE id = ?1",
                    rusqlite::params![id],
                )
                .unwrap_or_else(|err| {
                    tracing::error!(id, %err, "webhook subscriber delete failed");
                    0
                }) > 0
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.backend {
            SubBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.len(),
                Err(err) => {
                    tracing::error!(%err, "webhook subscriber read lock poisoned");
                    0
                }
            },
            SubBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "webhook subscriber pool acquire failed");
                        return 0;
                    }
                };
                conn.query_row("SELECT COUNT(*) FROM webhook_subscribers", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as usize)
                .unwrap_or_else(|err| {
                    tracing::error!(%err, "webhook subscriber count failed");
                    0
                })
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
