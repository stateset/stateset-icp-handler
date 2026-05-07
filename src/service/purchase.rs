//! Purchase intents: `authorize`, `buy` (and the `pay` alias).
//!
//! These are the money-moving intents that operate against an existing
//! quoted transaction. Mandate spend reservation runs here before the
//! engine is asked to persist the order, so a budget rejection never
//! leaves a half-completed order in the engine.

use chrono::Utc;

use crate::errors::ApiError;
use crate::mandate::MandateEvaluation;
use crate::models::{AuthorizeParams, BuyParams, OrderSummary, TransactionState};

use super::helpers::validate_payment_instrument;
use super::{IcpService, IntentInput, Outcome};

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

        // Persist order to the embedded engine when available.
        let order = if let Some(engine) = self.engine.as_ref() {
            match engine.persist_order(&txn.buyer, &txn.currency, &txn.line_items, &txn.totals) {
                Ok(order) => order,
                Err(err) => {
                    // Engine persistence is best-effort in v0.1 — the
                    // transaction still completes and we still sign a
                    // receipt. Surface the failure so operators can see it.
                    tracing::warn!(error = %err, "engine persist_order failed");
                    None
                }
            }
        } else {
            None
        };

        let persisted = self
            .transactions
            .update(&txn_id, |t| {
                t.state = TransactionState::Completed;
                t.order_id = order.as_ref().map(|o| o.id.clone());
                t.updated_at = Utc::now();
            })
            .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {txn_id}")))?;

        let order_summary = order.map(|o| OrderSummary {
            id: o.id,
            order_number: o.order_number,
            status: "created".into(),
            permalink_url: None,
            total: o.total,
        });

        Ok(Outcome {
            transaction: persisted,
            order: order_summary,
            subscription: None,
            peer_quote: None,
        })
    }
}
