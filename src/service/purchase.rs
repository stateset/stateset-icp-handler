//! Purchase intents: `authorize`, `buy` (and the `pay` alias).
//!
//! These are the money-moving intents that operate against an existing
//! quoted transaction. Mandate spend reservation runs here before the
//! engine is asked to persist the order, so a budget rejection never
//! leaves a half-completed order in the engine.

use chrono::Utc;

use crate::errors::ApiError;
use crate::mandate::MandateEvaluation;
use crate::models::{AuthorizeParams, BuyParams, OrderSummary, Transaction, TransactionState};

use super::helpers::validate_payment_instrument;
use super::{IcpService, IntentInput, Outcome};

/// Subscription-lifecycle transactions (trial-enrollment previews,
/// pause/cancel markers) are linked to a subscription via
/// `external_refs["subscription_id"]`. They are NOT directly chargeable —
/// authorizing/buying one would bypass the subscription billing flow (e.g.
/// charge a free trial immediately, double-counting mandate spend). Reject
/// any money-moving intent that targets one.
fn ensure_not_subscription_linked(txn: &Transaction) -> Result<(), ApiError> {
    if txn.external_refs.contains_key("subscription_id") {
        return Err(ApiError::PreconditionFailed(
            "transaction belongs to a subscription and cannot be authorized or bought directly"
                .into(),
        ));
    }
    Ok(())
}

impl IcpService {
    pub(super) async fn do_authorize(
        &self,
        input: &IntentInput<'_>,
        _mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: AuthorizeParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("authorize.params: {e}")))?;
        let txn_id = params.transaction_id.clone();
        let _txn_guard = self.lock_transaction(&txn_id).await;
        let txn = self.transaction_for_input(&txn_id, input)?;
        ensure_not_subscription_linked(&txn)?;
        if txn.state != TransactionState::Quoted {
            return Err(ApiError::PreconditionFailed(format!(
                "transaction in state {:?} cannot be authorized",
                txn.state
            )));
        }
        self.ensure_quote_open(&txn, Utc::now())?;

        let txn = self.transactions.update(&txn_id, |t| {
            if let Some(buyer) = params.buyer {
                t.buyer = buyer;
            }
            if let Some(ship_to) = params.ship_to {
                t.ship_to = Some(ship_to);
            }
            if let Some(bill_to) = params.bill_to {
                t.bill_to = Some(bill_to);
            }
            t.state = TransactionState::Authorized;
            t.updated_at = Utc::now();
        });
        let txn = txn.ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {txn_id}")))?;

        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
        })
    }

    pub(super) async fn do_buy(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: BuyParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("buy.params: {e}")))?;
        validate_payment_instrument(&params.payment, &self.config.payment_execution_mode)?;
        let txn_id = params.transaction_id;
        let _txn_guard = self.lock_transaction(&txn_id).await;

        // Load transaction.
        let txn = self.transaction_for_input(&txn_id, input)?;
        ensure_not_subscription_linked(&txn)?;
        match txn.state {
            TransactionState::Quoted => self.ensure_quote_open(&txn, Utc::now())?,
            TransactionState::Authorized => {}
            other => {
                return Err(ApiError::PreconditionFailed(format!(
                    "transaction in state {other:?} cannot be bought"
                )));
            }
        }
        let spend_minor = txn
            .totals
            .total
            .as_ref()
            .map(|m| m.amount_minor)
            .unwrap_or(0);
        self.reserve_mandate_spend(mandate, &input.tenant.tenant_id, spend_minor, &txn.currency)?;

        let order_summary = self.persist_order_for_transaction(&txn)?;

        let persisted = self
            .transactions
            .update(&txn_id, |t| {
                t.state = TransactionState::Completed;
                t.order_id = order_summary.as_ref().map(|o| o.id.clone());
                t.updated_at = Utc::now();
            })
            .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {txn_id}")))?;

        Ok(Outcome {
            transaction: persisted,
            order: order_summary,
            subscription: None,
            peer_quote: None,
        })
    }

    pub(super) fn persist_order_for_transaction(
        &self,
        txn: &Transaction,
    ) -> Result<Option<OrderSummary>, ApiError> {
        let order = if let Some(engine) = self.engine.as_ref() {
            match engine.persist_order(&txn.buyer, &txn.currency, &txn.line_items, &txn.totals) {
                Ok(Some(order)) => Some(order),
                Ok(None) if self.config.is_production() => {
                    return Err(ApiError::PreconditionFailed(
                        "buyer email is required to persist a production order".into(),
                    ));
                }
                Ok(None) => None,
                Err(err) => {
                    if self.config.is_production() {
                        return Err(err);
                    }
                    // Non-production compatibility/demo mode keeps the
                    // protocol flow usable even when a local engine call
                    // fails. Production returns above.
                    tracing::warn!(error = %err, "engine persist_order failed");
                    None
                }
            }
        } else if self.config.is_production() {
            return Err(ApiError::EngineUnavailable(
                "iCommerce engine unavailable".into(),
            ));
        } else {
            None
        };

        Ok(order.map(|o| OrderSummary {
            id: o.id,
            order_number: o.order_number,
            status: "created".into(),
            permalink_url: None,
            total: o.total,
        }))
    }
}
