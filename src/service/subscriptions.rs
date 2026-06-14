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
    Buyer, OrderSummary, RenewParams, RequestItem, SubscribeParams, Subscription,
    SubscriptionRefParams, SubscriptionStatus, Totals, Transaction, TransactionState,
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
        let currency = input
            .envelope
            .context
            .currency
            .clone()
            .unwrap_or_else(|| "USD".to_string());
        let (preview_line_items, estimated_totals) =
            price_request_items(&params.items, &currency, "subscribe.params.items")?;
        let estimated_total = total_amount_minor(&estimated_totals);
        // Reserve the first charge's amount up front for both paths — the
        // mandate authorizes this subscription's first charge whether it
        // bills now or at trial end.
        self.reserve_mandate_spend(mandate, &input.tenant.tenant_id, estimated_total, &currency)?;
        let buyer = params.buyer.clone().unwrap_or_default();
        let id = format!("sub_{}", Uuid::new_v4().simple());

        // Free trial: enroll without charging. The first charge is deferred
        // to `trial_end`; the scheduler bills it then (the renewal query
        // includes `trialing`) and flips the status to `active`.
        if let Some(trial_days) = params.trial_days.filter(|d| *d > 0) {
            let trial_end = now + chrono::Duration::days(i64::from(trial_days));
            let sub = Subscription {
                id,
                status: SubscriptionStatus::Trialing,
                agent_id: input.agent.raw.clone(),
                tenant_id: input.tenant.tenant_id.clone(),
                mandate_jti: mandate.map(|m| m.payload.jti.clone()),
                buyer,
                ship_to: params.ship_to,
                items: params.items,
                currency,
                cadence: params.cadence,
                current_period_start: now,
                current_period_end: trial_end,
                next_charge_at: trial_end,
                charges_completed: 0,
                last_transaction_id: None,
                payment_instrument: Some(params.payment),
                created_at: now,
                updated_at: now,
                canceled_at: None,
                paused_at: None,
                failed_renewal_attempts: 0,
                trial_end: Some(trial_end),
            };
            self.subscriptions.insert(sub.clone());

            // No money moves yet — return a pseudo-transaction (Authorized)
            // that carries the priced future basket so the agent sees what
            // will be charged at trial end.
            let mut txn = self.subscription_pseudo_txn(input, &sub, TransactionState::Authorized);
            txn.line_items = preview_line_items;
            txn.totals = estimated_totals;
            self.transactions.insert(txn.clone());

            return Ok(Outcome {
                transaction: txn,
                order: None,
                subscription: Some(sub),
                peer_quote: None,
            });
        }

        let next = period_advance(now, params.cadence);
        // First charge — priced with the shared helper and persisted to
        // the engine in production before a completed transaction is stored.
        let (charge_txn, order_summary) = self
            .run_subscription_charge(
                input,
                &buyer,
                params.ship_to.clone(),
                &params.items,
                &currency,
            )
            .await?;
        let sub = Subscription {
            id,
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
            trial_end: None,
        };
        self.subscriptions.insert(sub.clone());

        Ok(Outcome {
            transaction: charge_txn,
            order: order_summary,
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
        let (charge_txn, order_summary) = self
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
                s.trial_end = None;
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
            order: order_summary,
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

    /// Run a subscription charge as a collapsed renewal payment. In
    /// production this fails closed unless the commerce engine persists an
    /// order; non-production keeps the demo-compatible synthetic charge.
    pub(super) async fn run_subscription_charge(
        &self,
        input: &IntentInput<'_>,
        buyer: &Buyer,
        ship_to: Option<crate::models::Address>,
        items: &[RequestItem],
        currency: &str,
    ) -> Result<(Transaction, Option<OrderSummary>), ApiError> {
        let (line_items, totals) = price_request_items(items, currency, "subscription.items")?;

        let now = Utc::now();
        let mut txn = Transaction {
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
        let order_summary = self.persist_order_for_transaction(&txn)?;
        txn.order_id = order_summary.as_ref().map(|o| o.id.clone());
        self.transactions.insert(txn.clone());
        Ok((txn, order_summary))
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
