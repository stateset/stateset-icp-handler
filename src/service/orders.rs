//! Post-fulfillment order intents: `track`, `return`, `refund_request`.
//!
//! `refund_request` is currently a thin alias over `return`; engine-side
//! returns wiring will diverge them in a follow-up.

use chrono::Utc;

use crate::errors::ApiError;
use crate::mandate::MandateEvaluation;
use crate::models::TransactionState;

use super::{IcpService, IntentInput, Outcome};

impl IcpService {
    pub(super) async fn do_track(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let txn_id = input
            .envelope
            .params
            .get("transaction_id")
            .and_then(|v| v.as_str())
            .or(input.envelope.transaction_id.as_deref())
            .ok_or_else(|| ApiError::InvalidRequest("track: transaction_id required".into()))?;
        let txn = self.transaction_for_input(txn_id, input)?;
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
        })
    }

    pub(super) async fn do_return(
        &self,
        input: &IntentInput<'_>,
        _mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        // In v0.1 we only mark the transaction reversed. The engine's
        // returns API will be wired in follow-ups.
        let txn_id = input
            .envelope
            .transaction_id
            .as_deref()
            .or_else(|| {
                input
                    .envelope
                    .params
                    .get("transaction_id")
                    .and_then(|v| v.as_str())
            })
            .ok_or_else(|| ApiError::InvalidRequest("return: transaction_id required".into()))?;
        let _txn_guard = self.lock_transaction(txn_id).await;
        self.ensure_transaction_owner(txn_id, input)?;
        let txn = self
            .transactions
            .update(txn_id, |t| {
                t.state = TransactionState::Reversed;
                t.updated_at = Utc::now();
            })
            .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {txn_id}")))?;
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
        })
    }

    pub(super) async fn do_refund_request(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        self.do_return(input, mandate).await
    }
}
