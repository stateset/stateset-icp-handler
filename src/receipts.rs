//! Receipt store.
//!
//! Receipts are persisted so clients can re-fetch them by `jti` and so the
//! handler can answer verification queries without the original caller
//! needing to re-present the compact JWS. Persistence across restart is
//! required: an agent that calls `GET /icp/v1/receipts/:jti` after a pod
//! bounce must still get a 200 with the signed receipt body.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::signing::ReceiptClaims;

#[derive(Debug, Clone)]
pub struct StoredReceipt {
    pub jti: String,
    pub kid: String,
    pub jws: String,
    pub body_digest: String,
    pub claims: ReceiptClaims,
}

#[derive(Clone)]
pub struct ReceiptStore {
    backend: ReceiptBackend,
}

#[derive(Clone)]
enum ReceiptBackend {
    Memory(Arc<RwLock<HashMap<String, StoredReceipt>>>),
    Sqlite(crate::state_db::StatePool),
}

impl Default for ReceiptStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl ReceiptStore {
    pub fn new() -> Self {
        Self::in_memory()
    }

    pub fn in_memory() -> Self {
        Self {
            backend: ReceiptBackend::Memory(Arc::new(RwLock::new(HashMap::new()))),
        }
    }

    pub fn with_pool(pool: crate::state_db::StatePool) -> Self {
        Self {
            backend: ReceiptBackend::Sqlite(pool),
        }
    }

    pub fn insert(&self, receipt: StoredReceipt) {
        match &self.backend {
            ReceiptBackend::Memory(inner) => match inner.write() {
                Ok(mut guard) => {
                    guard.insert(receipt.jti.clone(), receipt);
                }
                Err(err) => {
                    tracing::error!(%err, "receipt store write lock poisoned");
                }
            },
            ReceiptBackend::Sqlite(pool) => {
                let claims_json = match serde_json::to_string(&receipt.claims) {
                    Ok(json) => json,
                    Err(err) => {
                        tracing::error!(jti = %receipt.jti, %err, "serialize receipt claims failed");
                        return;
                    }
                };
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(jti = %receipt.jti, %err, "receipt pool acquire failed");
                        return;
                    }
                };
                if let Err(err) = conn.execute(
                    "INSERT INTO receipts (jti, kid, jws, body_digest, claims_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(jti) DO UPDATE SET \
                         kid = excluded.kid, \
                         jws = excluded.jws, \
                         body_digest = excluded.body_digest, \
                         claims_json = excluded.claims_json",
                    rusqlite::params![
                        receipt.jti,
                        receipt.kid,
                        receipt.jws,
                        receipt.body_digest,
                        claims_json,
                    ],
                ) {
                    tracing::error!(%err, "receipt store write failed");
                }
            }
        }
    }

    pub fn get(&self, jti: &str) -> Option<StoredReceipt> {
        match &self.backend {
            ReceiptBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.get(jti).cloned(),
                Err(err) => {
                    tracing::error!(%err, "receipt store read lock poisoned");
                    None
                }
            },
            ReceiptBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(jti, %err, "receipt pool acquire failed");
                        return None;
                    }
                };
                let row: rusqlite::Result<(String, String, String, String, String)> = conn
                    .query_row(
                    "SELECT jti, kid, jws, body_digest, claims_json FROM receipts WHERE jti = ?1",
                    rusqlite::params![jti],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                );
                match row {
                    Ok((jti, kid, jws, body_digest, claims_json)) => {
                        let claims: ReceiptClaims = match serde_json::from_str(&claims_json) {
                            Ok(claims) => claims,
                            Err(err) => {
                                tracing::error!(jti, %err, "stored receipt claims are invalid");
                                return None;
                            }
                        };
                        Some(StoredReceipt {
                            jti,
                            kid,
                            jws,
                            body_digest,
                            claims,
                        })
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(err) => {
                        tracing::error!(jti, %err, "receipt store read failed");
                        None
                    }
                }
            }
        }
    }

    /// Operator-level cross-tenant scan ordered newest-first by
    /// `claims.iat`. The route handler joins each row to the
    /// underlying transaction and filters out cross-tenant rows
    /// before returning to the caller — receipts don't carry a
    /// `tenant_id` field of their own (the signed claims shape is
    /// wire-stable).
    pub fn list_recent(&self, limit: usize) -> Vec<StoredReceipt> {
        let mut all = match &self.backend {
            ReceiptBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.values().cloned().collect::<Vec<_>>(),
                Err(err) => {
                    tracing::error!(%err, "receipt store read lock poisoned");
                    Vec::new()
                }
            },
            ReceiptBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "receipt pool acquire failed");
                        return Vec::new();
                    }
                };
                let mut stmt = match conn.prepare(
                    "SELECT jti, kid, jws, body_digest, claims_json \
                         FROM receipts ORDER BY created_at DESC LIMIT ?1",
                ) {
                    Ok(stmt) => stmt,
                    Err(err) => {
                        tracing::error!(%err, "prepare receipts list_recent failed");
                        return Vec::new();
                    }
                };
                let rows = match stmt.query_map(rusqlite::params![limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                }) {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::error!(%err, "query receipts list_recent failed");
                        return Vec::new();
                    }
                };
                rows.filter_map(Result::ok)
                    .filter_map(|(jti, kid, jws, body_digest, claims_json)| {
                        serde_json::from_str::<ReceiptClaims>(&claims_json)
                            .ok()
                            .map(|claims| StoredReceipt {
                                jti,
                                kid,
                                jws,
                                body_digest,
                                claims,
                            })
                    })
                    .collect()
            }
        };
        // Sort by signed-at timestamp regardless of backend so the
        // in-memory tests get the same ordering as production.
        all.sort_by(|a, b| b.claims.iat.cmp(&a.claims.iat));
        all.truncate(limit);
        all
    }

    pub fn len(&self) -> usize {
        match &self.backend {
            ReceiptBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.len(),
                Err(err) => {
                    tracing::error!(%err, "receipt store read lock poisoned");
                    0
                }
            },
            ReceiptBackend::Sqlite(pool) => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(%err, "receipt pool acquire failed");
                        return 0;
                    }
                };
                conn.query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get::<_, i64>(0))
                    .map(|n| n as usize)
                    .unwrap_or_else(|err| {
                        tracing::error!(%err, "receipt store count failed");
                        0
                    })
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
