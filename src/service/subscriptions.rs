//! Subscription intents: `subscribe`, `renew`, `pause`, `cancel_subscription`.
//!
//! The shared charge path (`run_subscription_charge`) lives here too —
//! both first-time subscribe and operator/agent-driven renew route
//! through it, and the scheduler tick reuses it via the shared module.

use chrono::Utc;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::mandate::MandateEvaluation;
use crate::models::{
    Buyer, RenewParams, RequestItem, SubscribeParams, Subscription, SubscriptionRefParams,
    SubscriptionStatus, Totals, Transaction, TransactionState,
};

use super::helpers::{
    period_advance, price_request_items, total_amount_minor, validate_payment_instrument,
};
use super::{IcpService, IntentInput, Outcome};

impl IcpService {
    pub(super) async fn do_subscribe(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: SubscribeParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("subscribe.params: {e}")))?;
        if params.items.is_empty() {
            return Err(ApiError::InvalidRequest(
                "subscribe.params.items must be non-empty".into(),
            ));
        }
        validate_payment_instrument(&params.payment, &self.config.payment_execution_mode)?;

        let now = Utc::now();
        let next = period_advance(now, params.cadence);
        let currency = input
            .envelope
            .context
            .currency
            .clone()
            .unwrap_or_else(|| "USD".to_string());
        let (_line_items, estimated_totals) =
            price_request_items(&params.items, &currency, "subscribe.params.items")?;
        let estimated_total = total_amount_minor(&estimated_totals);
        self.reserve_mandate_spend(mandate, &input.tenant.tenant_id, estimated_total, &currency)?;

        // First charge — runs through the same quote+buy pipeline so it
        // produces a real Transaction with totals + receipt.
        let buyer = params.buyer.clone().unwrap_or_default();
        let charge_txn = self
            .run_subscription_charge(
                input,
                &buyer,
                params.ship_to.clone(),
                &params.items,
                &currency,
            )
            .await?;
        let sub = Subscription {
            id: format!("sub_{}", Uuid::new_v4().simple()),
            status: SubscriptionStatus::Active,
            agent_id: input.agent.raw.clone(),
            tenant_id: input.tenant.tenant_id.clone(),
            mandate_jti: mandate.map(|m| m.payload.jti.clone()),
            buyer,
            ship_to: params.ship_to,
            items: params.items,
            currency,
            cadence: params.cadence,
            current_period_start: now,
            current_period_end: next,
            next_charge_at: next,
            charges_completed: 1,
            last_transaction_id: Some(charge_txn.id.clone()),
            payment_instrument: Some(params.payment),
            created_at: now,
            updated_at: now,
            canceled_at: None,
            paused_at: None,
            failed_renewal_attempts: 0,
        };
        self.subscriptions.insert(sub.clone());

        Ok(Outcome {
            transaction: charge_txn,
            order: None,
            subscription: Some(sub),
            peer_quote: None,
        })
    }

    pub(super) async fn do_renew(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: RenewParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("renew.params: {e}")))?;
        validate_payment_instrument(&params.payment, &self.config.payment_execution_mode)?;
        let _sub_guard = self.lock_subscription(&params.subscription_id).await;
        let sub = self.subscription_for_input(&params.subscription_id, input)?;

        match sub.status {
            SubscriptionStatus::Canceled => {
                return Err(ApiError::PreconditionFailed(
                    "subscription is canceled and cannot be renewed".into(),
                ));
            }
            SubscriptionStatus::Paused => {
                return Err(ApiError::PreconditionFailed(
                    "subscription is paused; resume before renewing".into(),
                ));
            }
            _ => {}
        }

        let now = Utc::now();
        let (_line_items, estimated_totals) =
            price_request_items(&sub.items, &sub.currency, "subscription.items")?;
        let estimated_total = total_amount_minor(&estimated_totals);
        self.reserve_mandate_spend(
            mandate,
            &input.tenant.tenant_id,
            estimated_total,
            &sub.currency,
        )?;
        let charge_txn = self
            .run_subscription_charge(
                input,
                &sub.buyer,
                sub.ship_to.clone(),
                &sub.items,
                &sub.currency,
            )
            .await?;
        // Advance the period from the *previous* end so consecutive
        // renewals don't drift forward in time.
        let new_period_start = sub.current_period_end;
        let new_period_end = period_advance(new_period_start, sub.cadence);

        let new_payment = params.payment;
        let updated = self
            .subscriptions
            .update(&params.subscription_id, |s| {
                s.current_period_start = new_period_start;
                s.current_period_end = new_period_end;
                s.next_charge_at = new_period_end;
                s.charges_completed = s.charges_completed.saturating_add(1);
                s.last_transaction_id = Some(charge_txn.id.clone());
                s.status = SubscriptionStatus::Active;
                s.failed_renewal_attempts = 0;
                s.payment_instrument = Some(new_payment);
                s.updated_at = now;
                if mandate.is_some() {
                    s.mandate_jti = mandate.map(|m| m.payload.jti.clone());
                }
            })
            .expect("subscription existed at the start of renew");

        Ok(Outcome {
            transaction: charge_txn,
            order: None,
            subscription: Some(updated),
            peer_quote: None,
        })
    }

    pub(super) async fn do_pause(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let params: SubscriptionRefParams =
            serde_json::from_value(input.envelope.params.clone())
                .map_err(|e| ApiError::InvalidRequest(format!("pause.params: {e}")))?;

        let _sub_guard = self.lock_subscription(&params.subscription_id).await;
        let sub = self.subscription_for_input(&params.subscription_id, input)?;
        if sub.status.is_terminal() {
            return Err(ApiError::PreconditionFailed(format!(
                "cannot pause subscription in state {:?}",
                sub.status
            )));
        }

        let now = Utc::now();
        let updated = self
            .subscriptions
            .update(&params.subscription_id, |s| {
                s.status = SubscriptionStatus::Paused;
                s.paused_at = Some(now);
                s.updated_at = now;
            })
            .expect("subscription existed at the start of pause");

        // Synthesize a no-op transaction so the response shape stays
        // uniform across all intents (the receipt still signs over it).
        let txn = self.subscription_pseudo_txn(input, &updated, TransactionState::Canceled);
        self.transactions.insert(txn.clone());

        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: Some(updated),
            peer_quote: None,
        })
    }

    pub(super) async fn do_cancel_subscription(
        &self,
        input: &IntentInput<'_>,
    ) -> Result<Outcome, ApiError> {
        let params: SubscriptionRefParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| {
            ApiError::InvalidRequest(format!("cancel_subscription.params: {e}"))
        })?;

        let _sub_guard = self.lock_subscription(&params.subscription_id).await;
        let sub = self.subscription_for_input(&params.subscription_id, input)?;
        if sub.status == SubscriptionStatus::Canceled {
            return Err(ApiError::PreconditionFailed(
                "subscription already canceled".into(),
            ));
        }

        let now = Utc::now();
        let updated = self
            .subscriptions
            .update(&params.subscription_id, |s| {
                s.status = SubscriptionStatus::Canceled;
                s.canceled_at = Some(now);
                s.updated_at = now;
            })
            .expect("subscription existed at the start of cancel");

        let txn = self.subscription_pseudo_txn(input, &updated, TransactionState::Canceled);
        self.transactions.insert(txn.clone());

        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: Some(updated),
            peer_quote: None,
        })
    }

    /// Run a subscription charge as if it were a `quote → authorize →
    /// buy` triplet collapsed into one step. Returns the resulting
    /// completed transaction (already inserted into the txn store).
    pub(super) async fn run_subscription_charge(
        &self,
        input: &IntentInput<'_>,
        buyer: &Buyer,
        ship_to: Option<crate::models::Address>,
        items: &[RequestItem],
        currency: &str,
    ) -> Result<Transaction, ApiError> {
        let (line_items, totals) = price_request_items(items, currency, "subscription.items")?;

        let now = Utc::now();
        let txn = Transaction {
            id: format!("txn_{}", Uuid::new_v4().simple()),
            state: TransactionState::Completed,
            agent_id: input.agent.raw.clone(),
            tenant_id: input.tenant.tenant_id.clone(),
            mandate_jti: input.envelope.mandate_jti.clone(),
            currency: currency.to_string(),
            jurisdiction: input.envelope.context.jurisdiction.clone(),
            buyer: buyer.clone(),
            ship_to,
            bill_to: None,
            line_items,
            totals,
            order_id: None,
            quote_expires_at: None,
            created_at: now,
            updated_at: now,
            external_refs: Default::default(),
        };
        self.transactions.insert(txn.clone());
        Ok(txn)
    }

    /// Synthesize a placeholder transaction for `pause` /
    /// `cancel_subscription` — those intents don't produce a real
    /// charge but the receipt still needs *something* to sign over.
    /// The transaction's `external_refs["subscription_id"]` link makes
    /// the relationship explicit.
    pub(super) fn subscription_pseudo_txn(
        &self,
        input: &IntentInput<'_>,
        sub: &Subscription,
        state: TransactionState,
    ) -> Transaction {
        let now = Utc::now();
        let mut external_refs = std::collections::BTreeMap::new();
        external_refs.insert("subscription_id".to_string(), sub.id.clone());
        Transaction {
            id: format!("txn_{}", Uuid::new_v4().simple()),
            state,
            agent_id: input.agent.raw.clone(),
            tenant_id: input.tenant.tenant_id.clone(),
            mandate_jti: sub.mandate_jti.clone(),
            currency: sub.currency.clone(),
            jurisdiction: input.envelope.context.jurisdiction.clone(),
            buyer: sub.buyer.clone(),
            ship_to: sub.ship_to.clone(),
            bill_to: None,
            line_items: Vec::new(),
            totals: Totals::default(),
            order_id: None,
            quote_expires_at: None,
            created_at: now,
            updated_at: now,
            external_refs,
        }
    }
}
