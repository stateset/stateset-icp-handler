//! Document stores for transactions, subscriptions, and peer quotes.
//!
//! Transactions are the persistent server-side aggregate of a buy flow
//! (draft → quoted → authorized → captured → completed). Subscriptions are
//! the recurring-billing aggregate. Peer quotes hold pending agent-to-agent
//! negotiation offers. All three are keyed by id and carry an opaque JSON
//! payload; SQLite persistence uses `payload_json` so additive model
//! evolution doesn't require table migrations.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, RwLock};

use rusqlite::OptionalExtension;
use serde::{de::DeserializeOwned, Serialize};

use crate::models::{PeerQuote, Subscription, Transaction};
use crate::state_db::StatePool;

// --------------------------------------------------------------------------
// Shared generic store
// --------------------------------------------------------------------------

/// Generic id-keyed store with either in-memory or SQLite backing. Values
/// are serialized as JSON in the SQLite column so additive model changes
/// don't require a table migration.
#[derive(Clone)]
struct JsonStore<T> {
    backend: JsonBackend<T>,
}

#[derive(Clone)]
enum JsonBackend<T> {
    Memory(Arc<RwLock<HashMap<String, T>>>),
    Sqlite {
        pool: StatePool,
        table: &'static str,
        _ty: PhantomData<fn() -> T>,
    },
}

impl<T> JsonStore<T>
where
    T: Clone + Serialize + DeserializeOwned,
{
    fn in_memory() -> Self {
        Self {
            backend: JsonBackend::Memory(Arc::new(RwLock::new(HashMap::new()))),
        }
    }

    fn with_pool(pool: StatePool, table: &'static str) -> Self {
        Self {
            backend: JsonBackend::Sqlite {
                pool,
                table,
                _ty: PhantomData,
            },
        }
    }

    fn insert(&self, id: &str, value: T) {
        match &self.backend {
            JsonBackend::Memory(inner) => {
                inner
                    .write()
                    .expect("store write")
                    .insert(id.to_string(), value);
            }
            JsonBackend::Sqlite { pool, table, .. } => {
                let json = serde_json::to_string(&value).expect("serialize store value");
                let conn = pool.get().expect("store pool acquire");
                let sql = format!(
                    "INSERT INTO {table} (id, payload_json, updated_at) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET \
                         payload_json = excluded.payload_json, \
                         updated_at = excluded.updated_at"
                );
                conn.execute(
                    &sql,
                    rusqlite::params![id, json, chrono::Utc::now().to_rfc3339()],
                )
                .expect("store insert");
            }
        }
    }

    fn get(&self, id: &str) -> Option<T> {
        match &self.backend {
            JsonBackend::Memory(inner) => inner.read().expect("store read").get(id).cloned(),
            JsonBackend::Sqlite { pool, table, .. } => {
                let conn = pool.get().expect("store pool acquire");
                let sql = format!("SELECT payload_json FROM {table} WHERE id = ?1");
                let row: Option<String> = conn
                    .query_row(&sql, rusqlite::params![id], |r| r.get(0))
                    .optional()
                    .expect("store read");
                row.map(|json| serde_json::from_str(&json).expect("deserialize stored value"))
            }
        }
    }

    fn update<F>(&self, id: &str, f: F) -> Option<T>
    where
        F: FnOnce(&mut T),
    {
        match &self.backend {
            JsonBackend::Memory(inner) => {
                let mut guard = inner.write().expect("store write");
                if let Some(val) = guard.get_mut(id) {
                    f(val);
                    return Some(val.clone());
                }
                None
            }
            JsonBackend::Sqlite { pool, table, .. } => {
                let mut conn = pool.get().expect("store pool acquire");
                let tx = conn.transaction().expect("begin store tx");
                let sql = format!("SELECT payload_json FROM {table} WHERE id = ?1");
                let current: Option<String> = tx
                    .query_row(&sql, rusqlite::params![id], |r| r.get(0))
                    .optional()
                    .expect("store read-for-update");
                let json = current?;
                let mut val: T = serde_json::from_str(&json).expect("deserialize stored value");
                f(&mut val);
                let updated = serde_json::to_string(&val).expect("serialize updated value");
                let update_sql =
                    format!("UPDATE {table} SET payload_json = ?1, updated_at = ?2 WHERE id = ?3");
                tx.execute(
                    &update_sql,
                    rusqlite::params![updated, chrono::Utc::now().to_rfc3339(), id],
                )
                .expect("store update-write");
                tx.commit().expect("store tx commit");
                Some(val)
            }
        }
    }

    fn list(&self, limit: usize) -> Vec<T> {
        match &self.backend {
            JsonBackend::Memory(inner) => inner
                .read()
                .expect("store read")
                .values()
                .take(limit)
                .cloned()
                .collect(),
            JsonBackend::Sqlite { pool, table, .. } => {
                let conn = pool.get().expect("store pool acquire");
                let sql = format!("SELECT payload_json FROM {table} LIMIT ?1");
                let mut stmt = conn.prepare(&sql).expect("prepare list");
                let rows = stmt
                    .query_map(rusqlite::params![limit as i64], |r| r.get::<_, String>(0))
                    .expect("query list");
                rows.filter_map(Result::ok)
                    .map(|json| serde_json::from_str(&json).expect("deserialize stored value"))
                    .collect()
            }
        }
    }

    fn len(&self) -> usize {
        match &self.backend {
            JsonBackend::Memory(inner) => inner.read().expect("store read").len(),
            JsonBackend::Sqlite { pool, table, .. } => {
                let conn = pool.get().expect("store pool acquire");
                let sql = format!("SELECT COUNT(*) FROM {table}");
                conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
                    .map(|n| n as usize)
                    .expect("store count")
            }
        }
    }
}

// --------------------------------------------------------------------------
// TransactionStore
// --------------------------------------------------------------------------

#[derive(Clone)]
pub struct TransactionStore {
    inner: JsonStore<Transaction>,
}

impl Default for TransactionStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl TransactionStore {
    pub fn new() -> Self {
        Self::in_memory()
    }

    pub fn in_memory() -> Self {
        Self {
            inner: JsonStore::in_memory(),
        }
    }

    pub fn with_pool(pool: StatePool) -> Self {
        Self {
            inner: JsonStore::with_pool(pool, "transactions"),
        }
    }

    pub fn insert(&self, txn: Transaction) {
        let id = txn.id.clone();
        self.inner.insert(&id, txn);
    }

    pub fn get(&self, id: &str) -> Option<Transaction> {
        self.inner.get(id)
    }

    pub fn update<F>(&self, id: &str, f: F) -> Option<Transaction>
    where
        F: FnOnce(&mut Transaction),
    {
        self.inner.update(id, f)
    }

    pub fn list(&self, limit: usize) -> Vec<Transaction> {
        self.inner.list(limit)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// --------------------------------------------------------------------------
// SubscriptionStore
// --------------------------------------------------------------------------

#[derive(Clone)]
pub struct SubscriptionStore {
    inner: JsonStore<Subscription>,
}

impl Default for SubscriptionStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl SubscriptionStore {
    pub fn new() -> Self {
        Self::in_memory()
    }

    pub fn in_memory() -> Self {
        Self {
            inner: JsonStore::in_memory(),
        }
    }

    pub fn with_pool(pool: StatePool) -> Self {
        Self {
            inner: JsonStore::with_pool(pool, "subscriptions"),
        }
    }

    pub fn insert(&self, sub: Subscription) {
        let id = sub.id.clone();
        self.inner.insert(&id, sub);
    }

    pub fn get(&self, id: &str) -> Option<Subscription> {
        self.inner.get(id)
    }

    pub fn update<F>(&self, id: &str, f: F) -> Option<Subscription>
    where
        F: FnOnce(&mut Subscription),
    {
        self.inner.update(id, f)
    }

    pub fn list(&self, limit: usize) -> Vec<Subscription> {
        self.inner.list(limit)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// --------------------------------------------------------------------------
// PeerQuoteStore
// --------------------------------------------------------------------------

#[derive(Clone)]
pub struct PeerQuoteStore {
    inner: JsonStore<PeerQuote>,
}

impl Default for PeerQuoteStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl PeerQuoteStore {
    pub fn new() -> Self {
        Self::in_memory()
    }

    pub fn in_memory() -> Self {
        Self {
            inner: JsonStore::in_memory(),
        }
    }

    pub fn with_pool(pool: StatePool) -> Self {
        Self {
            inner: JsonStore::with_pool(pool, "peer_quotes"),
        }
    }

    pub fn insert(&self, quote: PeerQuote) {
        let id = quote.id.clone();
        self.inner.insert(&id, quote);
    }

    pub fn get(&self, id: &str) -> Option<PeerQuote> {
        self.inner.get(id)
    }

    pub fn update<F>(&self, id: &str, f: F) -> Option<PeerQuote>
    where
        F: FnOnce(&mut PeerQuote),
    {
        self.inner.update(id, f)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
