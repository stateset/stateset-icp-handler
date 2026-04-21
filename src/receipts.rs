//! Receipt store.
//!
//! Receipts are persisted so clients can re-fetch them by `jti` and so the
//! handler can answer verification queries without the original caller
//! needing to re-present the compact JWS.

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

#[derive(Clone, Default)]
pub struct ReceiptStore {
    inner: Arc<RwLock<HashMap<String, StoredReceipt>>>,
}

impl ReceiptStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, receipt: StoredReceipt) {
        self.inner
            .write()
            .expect("receipt store write")
            .insert(receipt.jti.clone(), receipt);
    }

    pub fn get(&self, jti: &str) -> Option<StoredReceipt> {
        self.inner
            .read()
            .expect("receipt store read")
            .get(jti)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("receipt store read").len()
    }
}
