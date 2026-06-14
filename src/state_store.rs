//! Document stores for transactions, subscriptions, and peer quotes.
//!
//! Transactions are the persistent server-side aggregate of a buy flow
//! (draft → quoted → authorized → captured → completed). Subscriptions are
//! the recurring-billing aggregate. Peer quotes hold pending agent-to-agent
//! negotiation offers. All three are keyed by id and carry an opaque JSON
//! payload; SQLite persistence uses `payload_json` so additive model
//! evolution doesn't require table migrations.

use std::collections::{HashMap, HashSet};
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

    /// Insert/replace a row. `index` carries the denormalized metadata
    /// columns (tenant_id, state, timestamps, …) the concrete store wants
    /// kept alongside the JSON payload. They are written in the SAME UPSERT
    /// as `payload_json`, so a concurrent reader/sweeper can never observe
    /// a row whose index columns lag its payload — the previous two-step
    /// (separate-connection) write had exactly that gap.
    fn insert(&self, id: &str, value: T, index: &[(&'static str, String)]) {
        match &self.backend {
            JsonBackend::Memory(inner) => match inner.write() {
                Ok(mut guard) => {
                    guard.insert(id.to_string(), value);
                }
                Err(err) => {
                    tracing::error!(id, %err, "in-memory store write lock poisoned");
                }
            },
            JsonBackend::Sqlite { pool, table, .. } => {
                let json = match serde_json::to_string(&value) {
                    Ok(json) => json,
                    Err(err) => {
                        tracing::error!(table, id, %err, "serialize store value failed");
                        return;
                    }
                };
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(table, id, %err, "store pool acquire failed");
                        return;
                    }
                };
                // Base columns are id + payload_json; every index column is
                // appended to the column list, the VALUES placeholders, and
                // the ON CONFLICT SET clause. Column names are compile-time
                // constants from our own code (never user input), so the
                // format! interpolation is injection-safe.
                let mut cols = String::from("id, payload_json");
                let mut placeholders = String::from("?1, ?2");
                let mut set = String::from("payload_json = excluded.payload_json");
                for (i, (col, _)) in index.iter().enumerate() {
                    cols.push_str(", ");
                    cols.push_str(col);
                    placeholders.push_str(&format!(", ?{}", i + 3));
                    set.push_str(&format!(", {col} = excluded.{col}"));
                }
                let sql = format!(
                    "INSERT INTO {table} ({cols}) VALUES ({placeholders}) \
                     ON CONFLICT(id) DO UPDATE SET {set}"
                );
                let mut params: Vec<String> = Vec::with_capacity(2 + index.len());
                params.push(id.to_string());
                params.push(json);
                params.extend(index.iter().map(|(_, v)| v.clone()));
                conn.execute(&sql, rusqlite::params_from_iter(params.iter()))
                    .unwrap_or_else(|err| {
                        tracing::error!(table, id, %err, "store insert failed");
                        0
                    });
            }
        }
    }

    fn get(&self, id: &str) -> Option<T> {
        match &self.backend {
            JsonBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.get(id).cloned(),
                Err(err) => {
                    tracing::error!(id, %err, "in-memory store read lock poisoned");
                    None
                }
            },
            JsonBackend::Sqlite { pool, table, .. } => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(table, id, %err, "store pool acquire failed");
                        return None;
                    }
                };
                let sql = format!("SELECT payload_json FROM {table} WHERE id = ?1");
                let row: Option<String> = conn
                    .query_row(&sql, rusqlite::params![id], |r| r.get(0))
                    .optional()
                    .unwrap_or_else(|err| {
                        tracing::error!(table, id, %err, "store read failed");
                        None
                    });
                row.and_then(|json| deserialize_payload(table, Some(id), json))
            }
        }
    }

    /// Read-modify-write a row. `index_of` derives the denormalized index
    /// columns from the post-mutation value; they are written in the SAME
    /// transaction as `payload_json` so the row's payload and index can
    /// never diverge (the prior follow-up `UPDATE` on a separate connection
    /// could fail or be observed mid-flight, leaving them inconsistent).
    fn update<F, I>(&self, id: &str, f: F, index_of: I) -> Option<T>
    where
        F: FnOnce(&mut T),
        I: FnOnce(&T) -> Vec<(&'static str, String)>,
    {
        match &self.backend {
            JsonBackend::Memory(inner) => {
                let mut guard = match inner.write() {
                    Ok(guard) => guard,
                    Err(err) => {
                        tracing::error!(id, %err, "in-memory store write lock poisoned");
                        return None;
                    }
                };
                if let Some(val) = guard.get_mut(id) {
                    f(val);
                    return Some(val.clone());
                }
                None
            }
            JsonBackend::Sqlite { pool, table, .. } => {
                let mut conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(table, id, %err, "store pool acquire failed");
                        return None;
                    }
                };
                let tx = match conn.transaction() {
                    Ok(tx) => tx,
                    Err(err) => {
                        tracing::error!(table, id, %err, "begin store tx failed");
                        return None;
                    }
                };
                let sql = format!("SELECT payload_json FROM {table} WHERE id = ?1");
                let current: Option<String> = tx
                    .query_row(&sql, rusqlite::params![id], |r| r.get(0))
                    .optional()
                    .unwrap_or_else(|err| {
                        tracing::error!(table, id, %err, "store read-for-update failed");
                        None
                    });
                let json = current?;
                let mut val: T = deserialize_payload(table, Some(id), json)?;
                f(&mut val);
                let updated = match serde_json::to_string(&val) {
                    Ok(updated) => updated,
                    Err(err) => {
                        tracing::error!(table, id, %err, "serialize updated store value failed");
                        return None;
                    }
                };
                let index = index_of(&val);
                // payload_json is ?1; index columns are ?2..; id is last.
                let mut set = String::from("payload_json = ?1");
                for (i, (col, _)) in index.iter().enumerate() {
                    set.push_str(&format!(", {col} = ?{}", i + 2));
                }
                let id_placeholder = index.len() + 2;
                let update_sql = format!("UPDATE {table} SET {set} WHERE id = ?{id_placeholder}");
                let mut params: Vec<String> = Vec::with_capacity(index.len() + 2);
                params.push(updated);
                params.extend(index.iter().map(|(_, v)| v.clone()));
                params.push(id.to_string());
                if let Err(err) = tx.execute(&update_sql, rusqlite::params_from_iter(params.iter()))
                {
                    tracing::error!(table, id, %err, "store update-write failed");
                    return None;
                }
                if let Err(err) = tx.commit() {
                    tracing::error!(table, id, %err, "store tx commit failed");
                    return None;
                }
                Some(val)
            }
        }
    }

    fn list(&self, limit: usize) -> Vec<T> {
        match &self.backend {
            JsonBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.values().take(limit).cloned().collect(),
                Err(err) => {
                    tracing::error!(%err, "in-memory store read lock poisoned");
                    Vec::new()
                }
            },
            JsonBackend::Sqlite { pool, table, .. } => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(table, %err, "store pool acquire failed");
                        return Vec::new();
                    }
                };
                let sql = format!("SELECT payload_json FROM {table} LIMIT ?1");
                let mut stmt = match conn.prepare(&sql) {
                    Ok(stmt) => stmt,
                    Err(err) => {
                        tracing::error!(table, %err, "prepare store list failed");
                        return Vec::new();
                    }
                };
                let rows = match stmt
                    .query_map(rusqlite::params![limit as i64], |r| r.get::<_, String>(0))
                {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::error!(table, %err, "query store list failed");
                        return Vec::new();
                    }
                };
                rows.filter_map(Result::ok)
                    .filter_map(|json| deserialize_payload(table, None, json))
                    .collect()
            }
        }
    }

    fn len(&self) -> usize {
        match &self.backend {
            JsonBackend::Memory(inner) => match inner.read() {
                Ok(guard) => guard.len(),
                Err(err) => {
                    tracing::error!(%err, "in-memory store read lock poisoned");
                    0
                }
            },
            JsonBackend::Sqlite { pool, table, .. } => {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(table, %err, "store pool acquire failed");
                        return 0;
                    }
                };
                let sql = format!("SELECT COUNT(*) FROM {table}");
                conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
                    .map(|n| n as usize)
                    .unwrap_or_else(|err| {
                        tracing::error!(table, %err, "store count failed");
                        0
                    })
            }
        }
    }
}

fn deserialize_payload<T>(table: &str, id_hint: Option<&str>, json: String) -> Option<T>
where
    T: DeserializeOwned,
{
    match serde_json::from_str(&json) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::error!(table, id = id_hint, %err, "stored JSON payload is invalid");
            None
        }
    }
}

fn transaction_state_wire_name(state: crate::models::TransactionState) -> &'static str {
    use crate::models::TransactionState;
    match state {
        TransactionState::Draft => "draft",
        TransactionState::Quoted => "quoted",
        TransactionState::Authorized => "authorized",
        TransactionState::Captured => "captured",
        TransactionState::Fulfilled => "fulfilled",
        TransactionState::Completed => "completed",
        TransactionState::Reversed => "reversed",
        TransactionState::Canceled => "canceled",
        TransactionState::Expired => "expired",
    }
}

fn sqlite_has_legacy_metadata_rows(pool: &StatePool, table: &str) -> bool {
    let Ok(conn) = pool.get() else {
        tracing::error!(
            table,
            "store pool acquire failed while checking legacy metadata rows"
        );
        return false;
    };
    let sql = format!("SELECT 1 FROM {table} WHERE tenant_id = '' LIMIT 1");
    conn.query_row(&sql, [], |_r| Ok(()))
        .optional()
        .map(|row| row.is_some())
        .unwrap_or_else(|err| {
            tracing::error!(table, %err, "legacy metadata row check failed");
            false
        })
}

fn merge_legacy_transactions(
    indexed: &mut Vec<Transaction>,
    scanned: Vec<Transaction>,
    tenant_id: &str,
    state_filter: Option<crate::models::TransactionState>,
) {
    let mut seen: HashSet<String> = indexed.iter().map(|t| t.id.clone()).collect();
    indexed.extend(
        scanned
            .into_iter()
            .filter(|t| t.tenant_id == tenant_id)
            .filter(|t| state_filter.is_none_or(|s| t.state == s))
            .filter(|t| seen.insert(t.id.clone())),
    );
}

fn merge_legacy_subscriptions(
    indexed: &mut Vec<Subscription>,
    scanned: Vec<Subscription>,
    tenant_id: &str,
    status_filter: Option<crate::models::SubscriptionStatus>,
) {
    let mut seen: HashSet<String> = indexed.iter().map(|s| s.id.clone()).collect();
    indexed.extend(
        scanned
            .into_iter()
            .filter(|s| s.tenant_id == tenant_id)
            .filter(|s| status_filter.is_none_or(|x| s.status == x))
            .filter(|s| seen.insert(s.id.clone())),
    );
}

fn merge_legacy_peer_quotes(
    indexed: &mut Vec<PeerQuote>,
    scanned: Vec<PeerQuote>,
    tenant_id: &str,
    status_filter: Option<crate::models::PeerQuoteStatus>,
) {
    let mut seen: HashSet<String> = indexed.iter().map(|q| q.id.clone()).collect();
    indexed.extend(
        scanned
            .into_iter()
            .filter(|q| q.tenant_id == tenant_id)
            .filter(|q| status_filter.is_none_or(|s| q.status == s))
            .filter(|q| seen.insert(q.id.clone())),
    );
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
        let index = Self::index_columns(&txn);
        self.inner.insert(&id, txn, &index);
    }

    pub fn get(&self, id: &str) -> Option<Transaction> {
        self.inner.get(id)
    }

    pub fn update<F>(&self, id: &str, f: F) -> Option<Transaction>
    where
        F: FnOnce(&mut Transaction),
    {
        self.inner.update(id, f, Self::index_columns)
    }

    /// Denormalized index columns kept alongside the JSON payload (spec
    /// §8 list endpoints + the expiry sweeper read these instead of
    /// scanning every payload). Written atomically with the payload.
    fn index_columns(txn: &Transaction) -> Vec<(&'static str, String)> {
        vec![
            ("updated_at", txn.updated_at.to_rfc3339()),
            ("tenant_id", txn.tenant_id.clone()),
            ("created_at", txn.created_at.to_rfc3339()),
            ("state", transaction_state_wire_name(txn.state).to_string()),
            (
                "quote_expires_at",
                txn.quote_expires_at
                    .map(|expires_at| expires_at.to_rfc3339())
                    .unwrap_or_default(),
            ),
        ]
    }

    pub fn list(&self, limit: usize) -> Vec<Transaction> {
        self.inner.list(limit)
    }

    /// Tenant-scoped list. SQLite uses denormalized metadata columns
    /// (`tenant_id`, `state`, `created_at`) maintained alongside the
    /// JSON payload so list endpoints hit indexes instead of
    /// materializing every row. Legacy rows with blank metadata fall
    /// back to the old JSON scan path only when such rows exist.
    pub fn list_for_tenant(
        &self,
        tenant_id: &str,
        limit: usize,
        state_filter: Option<crate::models::TransactionState>,
    ) -> Vec<Transaction> {
        let mut all = match &self.inner.backend {
            JsonBackend::Memory(_) => self
                .inner
                .list(usize::MAX)
                .into_iter()
                .filter(|t| t.tenant_id == tenant_id)
                .filter(|t| state_filter.is_none_or(|s| t.state == s))
                .collect(),
            JsonBackend::Sqlite { pool, .. } => {
                let mut rows = self.list_for_tenant_indexed(pool, tenant_id, limit, state_filter);
                if rows.len() < limit && sqlite_has_legacy_metadata_rows(pool, "transactions") {
                    merge_legacy_transactions(
                        &mut rows,
                        self.inner.list(usize::MAX),
                        tenant_id,
                        state_filter,
                    );
                }
                rows
            }
        };
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        all.truncate(limit);
        all
    }

    pub fn list_due_for_expiry(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<Transaction> {
        match &self.inner.backend {
            JsonBackend::Memory(_) => self
                .inner
                .list(usize::MAX)
                .into_iter()
                .filter(|t| {
                    matches!(
                        t.state,
                        crate::models::TransactionState::Draft
                            | crate::models::TransactionState::Quoted
                    ) && t.quote_expires_at.is_some_and(|exp| exp <= now)
                })
                .collect(),
            JsonBackend::Sqlite { pool, .. } => {
                if sqlite_has_legacy_metadata_rows(pool, "transactions") {
                    return self
                        .inner
                        .list(usize::MAX)
                        .into_iter()
                        .filter(|t| {
                            matches!(
                                t.state,
                                crate::models::TransactionState::Draft
                                    | crate::models::TransactionState::Quoted
                            ) && t.quote_expires_at.is_some_and(|exp| exp <= now)
                        })
                        .collect();
                }
                self.list_due_for_expiry_indexed(pool, now)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn list_for_tenant_indexed(
        &self,
        pool: &StatePool,
        tenant_id: &str,
        limit: usize,
        state_filter: Option<crate::models::TransactionState>,
    ) -> Vec<Transaction> {
        let Ok(conn) = pool.get() else {
            tracing::error!("transaction tenant list could not acquire pool connection");
            return Vec::new();
        };
        if let Some(state) = state_filter {
            let mut stmt = match conn.prepare(
                "SELECT id, payload_json FROM transactions \
                 WHERE tenant_id = ?1 AND state = ?2 \
                 ORDER BY created_at DESC LIMIT ?3",
            ) {
                Ok(stmt) => stmt,
                Err(err) => {
                    tracing::error!(%err, "prepare indexed transaction tenant list failed");
                    return Vec::new();
                }
            };
            let rows = match stmt.query_map(
                rusqlite::params![tenant_id, transaction_state_wire_name(state), limit as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            ) {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(%err, "query indexed transaction tenant list failed");
                    return Vec::new();
                }
            };
            rows.filter_map(Result::ok)
                .filter_map(|(id, json)| deserialize_payload("transactions", Some(&id), json))
                .collect()
        } else {
            let mut stmt = match conn.prepare(
                "SELECT id, payload_json FROM transactions \
                 WHERE tenant_id = ?1 \
                 ORDER BY created_at DESC LIMIT ?2",
            ) {
                Ok(stmt) => stmt,
                Err(err) => {
                    tracing::error!(%err, "prepare indexed transaction tenant list failed");
                    return Vec::new();
                }
            };
            let rows = match stmt.query_map(rusqlite::params![tenant_id, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }) {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(%err, "query indexed transaction tenant list failed");
                    return Vec::new();
                }
            };
            rows.filter_map(Result::ok)
                .filter_map(|(id, json)| deserialize_payload("transactions", Some(&id), json))
                .collect()
        }
    }

    fn list_due_for_expiry_indexed(
        &self,
        pool: &StatePool,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<Transaction> {
        let Ok(conn) = pool.get() else {
            tracing::error!("transaction expiry list could not acquire pool connection");
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT id, payload_json FROM transactions \
             WHERE state IN ('draft', 'quoted') \
               AND quote_expires_at != '' \
               AND quote_expires_at <= ?1",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::error!(%err, "prepare indexed transaction expiry list failed");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(rusqlite::params![now.to_rfc3339()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(%err, "query indexed transaction expiry list failed");
                return Vec::new();
            }
        };
        rows.filter_map(Result::ok)
            .filter_map(|(id, json)| deserialize_payload("transactions", Some(&id), json))
            .collect()
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
        let index = Self::index_columns(&sub);
        self.inner.insert(&id, sub, &index);
    }

    pub fn get(&self, id: &str) -> Option<Subscription> {
        self.inner.get(id)
    }

    pub fn update<F>(&self, id: &str, f: F) -> Option<Subscription>
    where
        F: FnOnce(&mut Subscription),
    {
        self.inner.update(id, f, Self::index_columns)
    }

    /// Denormalized index columns (tenant_id, status, charge schedule)
    /// read by the subscription list endpoint and the renewal sweeper.
    /// Written atomically with the JSON payload. `payment_present` is a
    /// 0/1 flag; SQLite's INTEGER affinity converts the textual value.
    fn index_columns(sub: &Subscription) -> Vec<(&'static str, String)> {
        vec![
            ("updated_at", sub.updated_at.to_rfc3339()),
            ("tenant_id", sub.tenant_id.clone()),
            ("created_at", sub.created_at.to_rfc3339()),
            ("status", sub.status.wire_name().to_string()),
            ("next_charge_at", sub.next_charge_at.to_rfc3339()),
            (
                "payment_present",
                i64::from(sub.payment_instrument.is_some()).to_string(),
            ),
        ]
    }

    pub fn list(&self, limit: usize) -> Vec<Subscription> {
        self.inner.list(limit)
    }

    /// Tenant-scoped list for `GET /icp/v1/subscriptions`. SQLite
    /// uses indexed metadata columns (`tenant_id`, `status`,
    /// `created_at`) and only falls back to the JSON scan path for
    /// legacy rows written before those columns were maintained.
    pub fn list_for_tenant(
        &self,
        tenant_id: &str,
        limit: usize,
        status_filter: Option<crate::models::SubscriptionStatus>,
    ) -> Vec<Subscription> {
        let mut all = match &self.inner.backend {
            JsonBackend::Memory(_) => self
                .inner
                .list(usize::MAX)
                .into_iter()
                .filter(|s| s.tenant_id == tenant_id)
                .filter(|s| status_filter.is_none_or(|x| s.status == x))
                .collect(),
            JsonBackend::Sqlite { pool, .. } => {
                let mut rows = self.list_for_tenant_indexed(pool, tenant_id, limit, status_filter);
                if rows.len() < limit && sqlite_has_legacy_metadata_rows(pool, "subscriptions") {
                    merge_legacy_subscriptions(
                        &mut rows,
                        self.inner.list(usize::MAX),
                        tenant_id,
                        status_filter,
                    );
                }
                rows
            }
        };
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        all.truncate(limit);
        all
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Count subscriptions by status. SQLite uses the denormalized
    /// `status` column so the scheduler metrics tick does a small
    /// indexed aggregate instead of materializing every subscription.
    /// Legacy blank-metadata rows fall back to the JSON scan path.
    pub fn status_counts(&self) -> SubscriptionStatusCounts {
        if let JsonBackend::Sqlite { pool, .. } = &self.inner.backend {
            if !sqlite_has_legacy_metadata_rows(pool, "subscriptions") {
                return self.status_counts_indexed(pool);
            }
        }
        let mut counts = SubscriptionStatusCounts::default();
        for sub in self.inner.list(usize::MAX) {
            match sub.status {
                crate::models::SubscriptionStatus::Trialing => counts.trialing += 1,
                crate::models::SubscriptionStatus::Active => counts.active += 1,
                crate::models::SubscriptionStatus::Paused => counts.paused += 1,
                crate::models::SubscriptionStatus::Canceled => counts.canceled += 1,
                crate::models::SubscriptionStatus::PastDue => counts.past_due += 1,
            }
        }
        counts
    }

    pub fn list_due_for_renewal(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<Subscription> {
        match &self.inner.backend {
            JsonBackend::Memory(_) => self
                .inner
                .list(usize::MAX)
                .into_iter()
                .filter(|s| {
                    matches!(
                        s.status,
                        crate::models::SubscriptionStatus::Active
                            | crate::models::SubscriptionStatus::Trialing
                    ) && s.next_charge_at <= now
                        && s.payment_instrument.is_some()
                })
                .collect(),
            JsonBackend::Sqlite { pool, .. } => {
                if sqlite_has_legacy_metadata_rows(pool, "subscriptions") {
                    return self
                        .inner
                        .list(usize::MAX)
                        .into_iter()
                        .filter(|s| {
                            matches!(
                                s.status,
                                crate::models::SubscriptionStatus::Active
                                    | crate::models::SubscriptionStatus::Trialing
                            ) && s.next_charge_at <= now
                                && s.payment_instrument.is_some()
                        })
                        .collect();
                }
                self.list_due_for_renewal_indexed(pool, now)
            }
        }
    }

    fn status_counts_indexed(&self, pool: &StatePool) -> SubscriptionStatusCounts {
        let Ok(conn) = pool.get() else {
            tracing::error!("subscription status_counts could not acquire pool connection");
            return SubscriptionStatusCounts::default();
        };
        let mut stmt = match conn.prepare(
            "SELECT status, COUNT(*) FROM subscriptions \
             WHERE status != '' GROUP BY status",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::error!(%err, "prepare subscription status_counts failed");
                return SubscriptionStatusCounts::default();
            }
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(%err, "query subscription status_counts failed");
                return SubscriptionStatusCounts::default();
            }
        };
        let mut counts = SubscriptionStatusCounts::default();
        for row in rows.filter_map(Result::ok) {
            let (status, count) = row;
            let count = count as usize;
            match status.as_str() {
                "trialing" => counts.trialing = count,
                "active" => counts.active = count,
                "paused" => counts.paused = count,
                "canceled" => counts.canceled = count,
                "past_due" => counts.past_due = count,
                other => tracing::warn!(status = other, "unknown subscription status metadata"),
            }
        }
        counts
    }

    fn list_due_for_renewal_indexed(
        &self,
        pool: &StatePool,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<Subscription> {
        let Ok(conn) = pool.get() else {
            tracing::error!("subscription renewal list could not acquire pool connection");
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            // `trialing` is included so a trial's first charge fires at its
            // trial_end (next_charge_at); the scheduler flips it to active.
            "SELECT id, payload_json FROM subscriptions \
             WHERE status IN ('active', 'trialing') \
               AND payment_present = 1 \
               AND next_charge_at != '' \
               AND next_charge_at <= ?1",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::error!(%err, "prepare indexed subscription renewal list failed");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(rusqlite::params![now.to_rfc3339()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(%err, "query indexed subscription renewal list failed");
                return Vec::new();
            }
        };
        rows.filter_map(Result::ok)
            .filter_map(|(id, json)| deserialize_payload("subscriptions", Some(&id), json))
            .collect()
    }

    fn list_for_tenant_indexed(
        &self,
        pool: &StatePool,
        tenant_id: &str,
        limit: usize,
        status_filter: Option<crate::models::SubscriptionStatus>,
    ) -> Vec<Subscription> {
        let Ok(conn) = pool.get() else {
            tracing::error!("subscription tenant list could not acquire pool connection");
            return Vec::new();
        };
        if let Some(status) = status_filter {
            let mut stmt = match conn.prepare(
                "SELECT id, payload_json FROM subscriptions \
                 WHERE tenant_id = ?1 AND status = ?2 \
                 ORDER BY created_at DESC LIMIT ?3",
            ) {
                Ok(stmt) => stmt,
                Err(err) => {
                    tracing::error!(%err, "prepare indexed subscription tenant list failed");
                    return Vec::new();
                }
            };
            let rows = match stmt.query_map(
                rusqlite::params![tenant_id, status.wire_name(), limit as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            ) {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(%err, "query indexed subscription tenant list failed");
                    return Vec::new();
                }
            };
            rows.filter_map(Result::ok)
                .filter_map(|(id, json)| deserialize_payload("subscriptions", Some(&id), json))
                .collect()
        } else {
            let mut stmt = match conn.prepare(
                "SELECT id, payload_json FROM subscriptions \
                 WHERE tenant_id = ?1 \
                 ORDER BY created_at DESC LIMIT ?2",
            ) {
                Ok(stmt) => stmt,
                Err(err) => {
                    tracing::error!(%err, "prepare indexed subscription tenant list failed");
                    return Vec::new();
                }
            };
            let rows = match stmt.query_map(rusqlite::params![tenant_id, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }) {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(%err, "query indexed subscription tenant list failed");
                    return Vec::new();
                }
            };
            rows.filter_map(Result::ok)
                .filter_map(|(id, json)| deserialize_payload("subscriptions", Some(&id), json))
                .collect()
        }
    }
}

/// Snapshot of subscription rows by status. Refreshed once per
/// scheduler tick. Operators dashboard `past_due > 0` to alert on
/// failed dunning, and watch `active` for headcount stability.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct SubscriptionStatusCounts {
    pub trialing: usize,
    pub active: usize,
    pub paused: usize,
    pub canceled: usize,
    pub past_due: usize,
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
        let index = Self::index_columns(&quote);
        self.inner.insert(&id, quote, &index);
    }

    pub fn get(&self, id: &str) -> Option<PeerQuote> {
        self.inner.get(id)
    }

    pub fn update<F>(&self, id: &str, f: F) -> Option<PeerQuote>
    where
        F: FnOnce(&mut PeerQuote),
    {
        self.inner.update(id, f, Self::index_columns)
    }

    /// Denormalized index columns (tenant_id, status, expiry) read by the
    /// peer-quote list endpoint and the expiry sweeper. Written atomically
    /// with the JSON payload.
    fn index_columns(quote: &PeerQuote) -> Vec<(&'static str, String)> {
        vec![
            ("updated_at", quote.updated_at.to_rfc3339()),
            ("tenant_id", quote.tenant_id.clone()),
            ("created_at", quote.created_at.to_rfc3339()),
            ("status", quote.status.wire_name().to_string()),
            ("expires_at", quote.expires_at.to_rfc3339()),
        ]
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cross-tenant scan. Used by the expiry sweeper, which is
    /// operator-level and needs to see every tenant's quotes —
    /// tenant-scoped read paths use `list_for_tenant` instead.
    pub fn list_all(&self, limit: usize) -> Vec<PeerQuote> {
        self.inner.list(limit)
    }

    pub fn list_due_for_expiry(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<PeerQuote> {
        match &self.inner.backend {
            JsonBackend::Memory(_) => self
                .inner
                .list(usize::MAX)
                .into_iter()
                .filter(|q| {
                    matches!(
                        q.status,
                        crate::models::PeerQuoteStatus::Pending
                            | crate::models::PeerQuoteStatus::Quoted
                    ) && q.expires_at <= now
                })
                .collect(),
            JsonBackend::Sqlite { pool, .. } => {
                if sqlite_has_legacy_metadata_rows(pool, "peer_quotes") {
                    return self
                        .inner
                        .list(usize::MAX)
                        .into_iter()
                        .filter(|q| {
                            matches!(
                                q.status,
                                crate::models::PeerQuoteStatus::Pending
                                    | crate::models::PeerQuoteStatus::Quoted
                            ) && q.expires_at <= now
                        })
                        .collect();
                }
                self.list_due_for_expiry_indexed(pool, now)
            }
        }
    }

    /// Tenant-scoped list for `GET /icp/v1/peer_quotes`. SQLite uses
    /// indexed metadata columns (`tenant_id`, `status`, `created_at`)
    /// and only falls back to the JSON scan path for legacy rows
    /// written before those columns were maintained.
    pub fn list_for_tenant(
        &self,
        tenant_id: &str,
        limit: usize,
        status_filter: Option<crate::models::PeerQuoteStatus>,
    ) -> Vec<PeerQuote> {
        let mut all = match &self.inner.backend {
            JsonBackend::Memory(_) => self
                .inner
                .list(usize::MAX)
                .into_iter()
                .filter(|q| q.tenant_id == tenant_id)
                .filter(|q| status_filter.is_none_or(|s| q.status == s))
                .collect(),
            JsonBackend::Sqlite { pool, .. } => {
                let mut rows = self.list_for_tenant_indexed(pool, tenant_id, limit, status_filter);
                if rows.len() < limit && sqlite_has_legacy_metadata_rows(pool, "peer_quotes") {
                    merge_legacy_peer_quotes(
                        &mut rows,
                        self.inner.list(usize::MAX),
                        tenant_id,
                        status_filter,
                    );
                }
                rows
            }
        };
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        all.truncate(limit);
        all
    }

    fn list_due_for_expiry_indexed(
        &self,
        pool: &StatePool,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<PeerQuote> {
        let Ok(conn) = pool.get() else {
            tracing::error!("peer quote expiry list could not acquire pool connection");
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT id, payload_json FROM peer_quotes \
             WHERE status IN ('pending', 'quoted') \
               AND expires_at != '' \
               AND expires_at <= ?1",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                tracing::error!(%err, "prepare indexed peer quote expiry list failed");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(rusqlite::params![now.to_rfc3339()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(%err, "query indexed peer quote expiry list failed");
                return Vec::new();
            }
        };
        rows.filter_map(Result::ok)
            .filter_map(|(id, json)| deserialize_payload("peer_quotes", Some(&id), json))
            .collect()
    }

    fn list_for_tenant_indexed(
        &self,
        pool: &StatePool,
        tenant_id: &str,
        limit: usize,
        status_filter: Option<crate::models::PeerQuoteStatus>,
    ) -> Vec<PeerQuote> {
        let Ok(conn) = pool.get() else {
            tracing::error!("peer quote tenant list could not acquire pool connection");
            return Vec::new();
        };
        if let Some(status) = status_filter {
            let mut stmt = match conn.prepare(
                "SELECT id, payload_json FROM peer_quotes \
                 WHERE tenant_id = ?1 AND status = ?2 \
                 ORDER BY created_at DESC LIMIT ?3",
            ) {
                Ok(stmt) => stmt,
                Err(err) => {
                    tracing::error!(%err, "prepare indexed peer quote tenant list failed");
                    return Vec::new();
                }
            };
            let rows = match stmt.query_map(
                rusqlite::params![tenant_id, status.wire_name(), limit as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            ) {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(%err, "query indexed peer quote tenant list failed");
                    return Vec::new();
                }
            };
            rows.filter_map(Result::ok)
                .filter_map(|(id, json)| deserialize_payload("peer_quotes", Some(&id), json))
                .collect()
        } else {
            let mut stmt = match conn.prepare(
                "SELECT id, payload_json FROM peer_quotes \
                 WHERE tenant_id = ?1 \
                 ORDER BY created_at DESC LIMIT ?2",
            ) {
                Ok(stmt) => stmt,
                Err(err) => {
                    tracing::error!(%err, "prepare indexed peer quote tenant list failed");
                    return Vec::new();
                }
            };
            let rows = match stmt.query_map(rusqlite::params![tenant_id, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }) {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(%err, "query indexed peer quote tenant list failed");
                    return Vec::new();
                }
            };
            rows.filter_map(Result::ok)
                .filter_map(|(id, json)| deserialize_payload("peer_quotes", Some(&id), json))
                .collect()
        }
    }
}
