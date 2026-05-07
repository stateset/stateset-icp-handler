//! Mid-flow transaction intents: `negotiate`, `confirm_receipt`.
//!
//! Neither moves money — both are audit-trail intents that stamp
//! external_refs onto an existing transaction. They produce bespoke
//! event types (`transaction.renegotiated`, `transaction.receipt_confirmed`)
//! rather than the generic `transaction.<state>` form.

use chrono::Utc;

use crate::errors::ApiError;
use crate::mandate::MandateEvaluation;
use crate::models::{ConfirmReceiptParams, Money, NegotiateParams, TransactionState};

use super::{IcpService, IntentInput, Outcome};

impl IcpService {
    pub(super) async fn do_negotiate(
        &self,
        input: &IntentInput<'_>,
        _mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: NegotiateParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("negotiate.params: {e}")))?;

        if params.proposed_total.is_none() && params.discount_pct.is_none() {
            return Err(ApiError::InvalidRequest(
                "negotiate: either `proposed_total` or `discount_pct` is required".into(),
            ));
        }
        if let Some(pct) = params.discount_pct {
            if !(0.0..=90.0).contains(&pct) {
                return Err(ApiError::InvalidRequest(format!(
                    "negotiate.discount_pct must be in [0.0, 90.0], got {pct}"
                )));
            }
        }

        let _txn_guard = self.lock_transaction(&params.transaction_id).await;
        let txn = self.transaction_for_input(&params.transaction_id, input)?;
        if !matches!(txn.state, TransactionState::Quoted) {
            return Err(ApiError::PreconditionFailed(format!(
                "negotiate: transaction in state {:?} cannot be re-negotiated; only `quoted` is open to counter-offers",
                txn.state
            )));
        }

        // Compute the new total. `proposed_total` wins; otherwise apply
        // the percentage discount to the existing total.
        let original_total_minor = txn
            .totals
            .total
            .as_ref()
            .map(|m| m.amount_minor)
            .unwrap_or(0);
        let new_total_minor = if let Some(pt) = params.proposed_total.as_ref() {
            if pt.amount_minor < 0 {
                return Err(ApiError::InvalidRequest(
                    "negotiate.proposed_total.amount_minor must be non-negative".into(),
                ));
            }
            pt.amount_minor
        } else {
            let pct = params.discount_pct.unwrap_or(0.0);
            // Round half-up via i64 arithmetic — avoids float drift on
            // currency values.
            let scaled = (original_total_minor as i128) * ((10_000.0 - pct * 100.0) as i128);
            (scaled / 10_000) as i64
        };

        let currency = txn.currency.clone();
        let now = Utc::now();
        let updated = self
            .transactions
            .update(&params.transaction_id, |t| {
                let prior = t
                    .totals
                    .total
                    .clone()
                    .unwrap_or_else(|| Money::new(original_total_minor, &currency));
                t.totals.total = Some(Money::new(new_total_minor, &currency));
                // Audit: stamp every offer onto external_refs so the
                // history is reconstructible from the transaction alone.
                let history_key = format!(
                    "negotiation_{:03}",
                    t.external_refs
                        .keys()
                        .filter(|k| k.starts_with("negotiation_"))
                        .count()
                );
                let entry = serde_json::json!({
                    "from_minor": prior.amount_minor,
                    "to_minor": new_total_minor,
                    "currency": currency,
                    "discount_pct": params.discount_pct,
                    "message": params.message,
                    "at": now.to_rfc3339(),
                    "agent_id": input.agent.raw,
                });
                t.external_refs.insert(history_key, entry.to_string());
                t.updated_at = now;
            })
            .ok_or_else(|| {
                ApiError::ResourceNotFound(format!("transaction {}", params.transaction_id))
            })?;

        Ok(Outcome {
            transaction: updated,
            order: None,
            subscription: None,
            peer_quote: None,
        })
    }

    pub(super) async fn do_confirm_receipt(
        &self,
        input: &IntentInput<'_>,
    ) -> Result<Outcome, ApiError> {
        let params: ConfirmReceiptParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("confirm_receipt.params: {e}")))?;

        let _txn_guard = self.lock_transaction(&params.transaction_id).await;
        let txn = self.transaction_for_input(&params.transaction_id, input)?;
        // Only post-payment states can be receipt-confirmed.
        if !matches!(
            txn.state,
            TransactionState::Captured | TransactionState::Fulfilled | TransactionState::Completed
        ) {
            return Err(ApiError::PreconditionFailed(format!(
                "confirm_receipt: transaction in state {:?} cannot be confirmed; payment must complete first",
                txn.state
            )));
        }

        if txn.external_refs.contains_key("receipt_confirmed_at") {
            return Err(ApiError::PreconditionFailed(
                "confirm_receipt: this transaction's receipt has already been confirmed".into(),
            ));
        }

        let now = Utc::now();
        let updated = self
            .transactions
            .update(&params.transaction_id, |t| {
                t.external_refs
                    .insert("receipt_confirmed_at".to_string(), now.to_rfc3339());
                t.external_refs
                    .insert("receipt_confirmed_by".to_string(), input.agent.raw.clone());
                if let Some(note) = params.note.as_deref() {
                    t.external_refs
                        .insert("receipt_note".to_string(), note.to_string());
                }
                t.updated_at = now;
            })
            .ok_or_else(|| {
                ApiError::ResourceNotFound(format!("transaction {}", params.transaction_id))
            })?;

        Ok(Outcome {
            transaction: updated,
            order: None,
            subscription: None,
            peer_quote: None,
        })
    }
}
