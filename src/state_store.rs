//! In-memory transaction store.
//!
//! Transactions are the persistent server-side aggregate of a buy flow
//! (draft → quoted → authorized → captured → completed). In v0.1 this is
//! pure in-memory; a Redis backend can be dropped in without changing the
//! `TransactionStore` surface.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::models::Transaction;

#[derive(Clone, Default)]
pub struct TransactionStore {
    inner: Arc<RwLock<HashMap<String, Transaction>>>,
}

impl TransactionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, txn: Transaction) {
        self.inner
            .write()
            .expect("txn store write")
            .insert(txn.id.clone(), txn);
    }

    pub fn get(&self, id: &str) -> Option<Transaction> {
        self.inner
            .read()
            .expect("txn store read")
            .get(id)
            .cloned()
    }

    pub fn update<F>(&self, id: &str, f: F) -> Option<Transaction>
    where
        F: FnOnce(&mut Transaction),
    {
        let mut guard = self.inner.write().expect("txn store write");
        if let Some(txn) = guard.get_mut(id) {
            f(txn);
            return Some(txn.clone());
        }
        None
    }

    pub fn list(&self, limit: usize) -> Vec<Transaction> {
        self.inner
            .read()
            .expect("txn store read")
            .values()
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("txn store read").len()
    }
}
