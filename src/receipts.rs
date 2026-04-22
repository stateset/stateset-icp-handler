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
            ReceiptBackend::Memory(inner) => {
                inner
                    .write()
                    .expect("receipt store write")
                    .insert(receipt.jti.clone(), receipt);
            }
            ReceiptBackend::Sqlite(pool) => {
                let claims_json =
                    serde_json::to_string(&receipt.claims).expect("serialize receipt claims");
                let conn = pool.get().expect("receipt pool acquire");
                conn.execute(
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
                )
                .expect("receipt store write");
            }
        }
    }

    pub fn get(&self, jti: &str) -> Option<StoredReceipt> {
        match &self.backend {
            ReceiptBackend::Memory(inner) => {
                inner.read().expect("receipt store read").get(jti).cloned()
            }
            ReceiptBackend::Sqlite(pool) => {
                let conn = pool.get().expect("receipt pool acquire");
                let row: rusqlite::Result<(String, String, String, String, String)> = conn
                    .query_row(
                    "SELECT jti, kid, jws, body_digest, claims_json FROM receipts WHERE jti = ?1",
                    rusqlite::params![jti],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                );
                match row {
                    Ok((jti, kid, jws, body_digest, claims_json)) => {
                        let claims: ReceiptClaims = serde_json::from_str(&claims_json)
                            .expect("deserialize stored receipt claims");
                        Some(StoredReceipt {
                            jti,
                            kid,
                            jws,
                            body_digest,
                            claims,
                        })
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(e) => panic!("receipt store read: {e}"),
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.backend {
            ReceiptBackend::Memory(inner) => inner.read().expect("receipt store read").len(),
            ReceiptBackend::Sqlite(pool) => {
                let conn = pool.get().expect("receipt pool acquire");
                conn.query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get::<_, i64>(0))
                    .map(|n| n as usize)
                    .expect("receipt store count")
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
