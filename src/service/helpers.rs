//! Shared helpers used by every intent family in the service tree.
//!
//! Free functions live at the top of the file; methods on `IcpService`
//! sit in the trailing `impl` block so call sites read identically to
//! how they did before the split.

use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::errors::ApiError;
use crate::events::Event;
use crate::mandate::{self, MandateEvaluation, MandateSpendLimits};
use crate::models::{
    BillingCadence, Buyer, IntentEnvelope, LineItem, Money, PaymentInstrument, PeerQuote,
    RequestItem, Subscription, Totals, Transaction, TransactionState,
};

use super::{IcpService, IntentInput};

pub(super) fn validate_payment_instrument(
    payment: &PaymentInstrument,
    execution_mode: &str,
) -> Result<(), ApiError> {
    match payment {
        PaymentInstrument::Card { token, .. } => {
            require_non_empty(token.as_deref(), "payment.card.token")?;
        }
        PaymentInstrument::DelegatedVault { token, provider } => {
            require_non_empty(Some(token), "payment.delegated_vault.token")?;
            if let Some(provider) = provider.as_deref() {
                require_non_empty(Some(provider), "payment.delegated_vault.provider")?;
            }
        }
        PaymentInstrument::Stablecoin {
            asset, chain, from, ..
        } => {
            require_non_empty(Some(asset), "payment.stablecoin.asset")?;
            require_non_empty(Some(chain), "payment.stablecoin.chain")?;
            require_non_empty(Some(from), "payment.stablecoin.from")?;
        }
        PaymentInstrument::A2A { peer_agent_id, .. } => {
            require_non_empty(Some(peer_agent_id), "payment.a2a.peer_agent_id")?;
        }
        PaymentInstrument::ExternalAuthorization {
            provider,
            authorization_id,
            ..
        } => {
            require_non_empty(Some(provider), "payment.external_authorization.provider")?;
            require_non_empty(
                Some(authorization_id),
                "payment.external_authorization.authorization_id",
            )?;
        }
    }

    if execution_mode == "external_required"
        && !matches!(payment, PaymentInstrument::ExternalAuthorization { .. })
    {
        return Err(ApiError::PreconditionFailed(
            "external payment authorization required by this handler".into(),
        ));
    }

    Ok(())
}

pub(super) fn require_non_empty(value: Option<&str>, field: &str) -> Result<(), ApiError> {
    if value.is_some_and(|v| !v.trim().is_empty()) {
        Ok(())
    } else {
        Err(ApiError::InvalidRequest(format!(
            "{field} must be non-empty"
        )))
    }
}

/// Advance a billing period anchor by one cadence unit.
///
/// `Monthly` uses chrono's `checked_add_months`, which clamps to the
/// last day of the month for inputs near month-end (Jan 31 → Feb 28/29).
/// `Annual` likewise clamps Feb 29 → Feb 28 in non-leap years.
pub(super) fn period_advance(
    start: chrono::DateTime<Utc>,
    cadence: BillingCadence,
) -> chrono::DateTime<Utc> {
    match cadence {
        BillingCadence::Weekly => start + chrono::Duration::days(7),
        BillingCadence::Monthly => start
            .checked_add_months(chrono::Months::new(1))
            .unwrap_or(start + chrono::Duration::days(30)),
        BillingCadence::Annual => start
            .checked_add_months(chrono::Months::new(12))
            .unwrap_or(start + chrono::Duration::days(365)),
    }
}

pub(super) fn price_request_items(
    items: &[RequestItem],
    currency: &str,
    field_prefix: &str,
) -> Result<(Vec<LineItem>, Totals), ApiError> {
    // Placeholder pricing: optional unit_price_hint or $10.00 per unit,
    // with 8.75% tax. The helper is shared by quote and subscriptions so
    // mandate reservation checks use exactly the same total the charge will
    // record.
    let mut line_items = Vec::with_capacity(items.len());
    let mut subtotal_minor: i64 = 0;
    for (idx, req) in items.iter().enumerate() {
        if req.quantity <= 0 {
            return Err(ApiError::InvalidRequest(format!(
                "{field_prefix}[{idx}].quantity must be positive"
            )));
        }
        let unit_minor = req
            .unit_price_hint
            .as_ref()
            .map(|m| m.amount_minor)
            .unwrap_or(1_000);
        if unit_minor < 0 {
            return Err(ApiError::InvalidRequest(format!(
                "{field_prefix}[{idx}].unit_price_hint.amount_minor must be non-negative"
            )));
        }
        let line_subtotal = unit_minor.saturating_mul(req.quantity);
        subtotal_minor = subtotal_minor.saturating_add(line_subtotal);
        line_items.push(LineItem {
            id: format!("li_{idx:06}"),
            sku: req.sku.clone(),
            name: req.sku.clone(),
            quantity: req.quantity,
            unit_price: Money::new(unit_minor, currency),
            subtotal: Money::new(line_subtotal, currency),
            tax: None,
            total: Money::new(line_subtotal, currency),
        });
    }
    let tax_minor = flat_tax_minor(subtotal_minor);
    let total_minor = subtotal_minor.saturating_add(tax_minor);
    let totals = Totals {
        subtotal: Some(Money::new(subtotal_minor, currency)),
        discount: None,
        shipping: None,
        tax: Some(Money::new(tax_minor, currency)),
        total: Some(Money::new(total_minor, currency)),
    };
    Ok((line_items, totals))
}

/// Flat 8.75% tax in minor units. Widened to i128 for the multiply:
/// `subtotal_minor` can reach i64::MAX (line math saturates), and a plain
/// `i64 * 875` would overflow — panicking under overflow-checks or silently
/// wrapping to a corrupt (even negative) tax that then flows into the
/// charged total and the signed receipt. The result is < subtotal, so the
/// narrowing back to i64 cannot truncate.
pub(super) fn flat_tax_minor(subtotal_minor: i64) -> i64 {
    (i128::from(subtotal_minor) * 875 / 10_000) as i64
}

/// Apply a discount (minor units) to an already-priced `Totals`, recomputing
/// tax on the post-discount subtotal: `total = (subtotal − discount) + tax`.
/// The discount is clamped to the subtotal so the total can't go negative.
pub(super) fn apply_discount_to_totals(totals: &mut Totals, discount_minor: i64, currency: &str) {
    let subtotal = totals
        .subtotal
        .as_ref()
        .map(|m| m.amount_minor)
        .unwrap_or(0);
    let discount = discount_minor.clamp(0, subtotal);
    let discounted = subtotal - discount;
    let tax = flat_tax_minor(discounted);
    totals.discount = Some(Money::new(discount, currency));
    totals.tax = Some(Money::new(tax, currency));
    totals.total = Some(Money::new(discounted.saturating_add(tax), currency));
}

pub(super) fn total_amount_minor(totals: &Totals) -> i64 {
    totals
        .total
        .as_ref()
        .map(|money| money.amount_minor)
        .unwrap_or(0)
}

/// Estimate the amount (in minor units) that an intent would put on the
/// mandate's budget. Used to gate budget checks *before* executing the
/// intent.
pub(super) fn estimate_intent_amount_minor(envelope: &IntentEnvelope) -> i64 {
    // If the caller supplied explicit line items (quote/authorize/buy via
    // inline items), sum them. Otherwise, for buy against an existing txn,
    // fall back to 0 here and re-check after loading the transaction.
    if let Some(items) = envelope.params.get("items").and_then(|v| v.as_array()) {
        let mut total: i64 = 0;
        for item in items {
            let qty = item.get("quantity").and_then(|v| v.as_i64()).unwrap_or(0);
            let unit = item
                .get("unit_price_hint")
                .and_then(|p| p.get("amount_minor"))
                .and_then(|v| v.as_i64())
                .unwrap_or(1_000);
            total = total.saturating_add(qty.saturating_mul(unit));
        }
        return total;
    }
    0
}

impl IcpService {
    pub(super) fn transaction_for_input(
        &self,
        transaction_id: &str,
        input: &IntentInput<'_>,
    ) -> Result<Transaction, ApiError> {
        let txn = self
            .transactions
            .get(transaction_id)
            .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {transaction_id}")))?;
        if txn.tenant_id == input.tenant.tenant_id {
            Ok(txn)
        } else {
            Err(ApiError::ResourceNotFound(format!(
                "transaction {transaction_id}"
            )))
        }
    }

    pub(super) fn ensure_transaction_owner(
        &self,
        transaction_id: &str,
        input: &IntentInput<'_>,
    ) -> Result<(), ApiError> {
        self.transaction_for_input(transaction_id, input)
            .map(|_| ())
    }

    pub(super) fn subscription_for_input(
        &self,
        subscription_id: &str,
        input: &IntentInput<'_>,
    ) -> Result<Subscription, ApiError> {
        let sub = self
            .subscriptions
            .get(subscription_id)
            .ok_or_else(|| ApiError::ResourceNotFound(format!("subscription {subscription_id}")))?;
        if sub.tenant_id == input.tenant.tenant_id {
            Ok(sub)
        } else {
            Err(ApiError::ResourceNotFound(format!(
                "subscription {subscription_id}"
            )))
        }
    }

    pub(super) fn peer_quote_for_input(
        &self,
        peer_quote_id: &str,
        input: &IntentInput<'_>,
    ) -> Result<PeerQuote, ApiError> {
        let quote = self
            .peer_quotes
            .get(peer_quote_id)
            .ok_or_else(|| ApiError::ResourceNotFound(format!("peer_quote {peer_quote_id}")))?;
        if quote.tenant_id == input.tenant.tenant_id {
            Ok(quote)
        } else {
            Err(ApiError::ResourceNotFound(format!(
                "peer_quote {peer_quote_id}"
            )))
        }
    }

    pub(super) fn ensure_quote_open(
        &self,
        txn: &Transaction,
        now: chrono::DateTime<Utc>,
    ) -> Result<(), ApiError> {
        if txn
            .quote_expires_at
            .is_some_and(|expires_at| expires_at <= now)
        {
            if matches!(
                txn.state,
                TransactionState::Draft | TransactionState::Quoted
            ) {
                self.transactions.update(&txn.id, |t| {
                    if matches!(t.state, TransactionState::Draft | TransactionState::Quoted)
                        && t.quote_expires_at
                            .is_some_and(|expires_at| expires_at <= now)
                    {
                        t.state = TransactionState::Expired;
                        t.updated_at = now;
                    }
                });
            }
            return Err(ApiError::PreconditionFailed(format!(
                "transaction {} quote has expired",
                txn.id
            )));
        }
        Ok(())
    }

    pub(super) fn reserve_mandate_spend(
        &self,
        mandate: Option<&MandateEvaluation>,
        tenant_id: &str,
        amount_minor: i64,
        currency: &str,
    ) -> Result<(), ApiError> {
        let Some(ev) = mandate else {
            return Ok(());
        };
        if amount_minor <= 0 {
            return Ok(());
        }
        let budget = &ev.payload.icp.budget;
        if !budget.currency.eq_ignore_ascii_case(currency) {
            return Err(ApiError::MandateOutOfScope(format!(
                "mandate budget currency `{}` does not match charge currency `{currency}`",
                budget.currency
            )));
        }
        let window = mandate::parse_period(budget.period.as_deref()).unwrap_or(Duration::days(1));
        self.mandates.try_record_spend_checked(
            &ev.payload.jti,
            tenant_id,
            amount_minor,
            Utc::now(),
            MandateSpendLimits {
                budget_minor: budget.amount_minor,
                per_transaction: budget.per_transaction,
                window,
            },
        )
    }

    /// Push an event onto the durable webhook outbox.
    ///
    /// Routing: when `tenant_id` has registered active subscribers,
    /// fan out one delivery per subscriber. Otherwise fall back to
    /// the global `webhook_url` if configured. No-op when there's
    /// neither a tenant subscriber nor a global URL.
    ///
    /// `tenant_id == None` skips per-tenant lookup and falls straight
    /// to the global URL — used by scheduler-driven events that
    /// originate without a tenant context. Subscription-bound events
    /// pass the tenant id stored on the subscription.
    pub(super) fn enqueue_webhook(&self, event: &Event, tenant_id: Option<&str>) {
        let payload_json = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
        let now = Utc::now();

        let mut destinations: Vec<(String, Option<String>)> = Vec::new();
        if let Some(t) = tenant_id {
            for sub in self.webhook_subscribers.list_active_for_tenant(t) {
                destinations.push((sub.url, sub.secret));
            }
        }
        // Global fallback fires only when this tenant has zero
        // subscribers — production deployments use it for ops
        // dashboards observing the whole fleet without re-registering
        // the same URL per tenant.
        if destinations.is_empty() {
            if let (Some(url), Some(secret)) =
                (self.webhook_url.as_deref(), self.webhook_secret.clone())
            {
                destinations.push((url.to_string(), Some(secret)));
            }
        }

        for (url, _secret) in destinations {
            // The per-subscriber `secret` is read at delivery time by
            // the worker — we only need to remember the URL on the row
            // for now. (A future iteration can store the secret
            // alongside the delivery so the worker doesn't have to
            // re-resolve the subscriber.)
            let _ = _secret;
            self.webhook_outbox
                .enqueue(crate::webhook::WebhookDelivery {
                    id: format!("del_{}", Uuid::new_v4().simple()),
                    event_id: event.id.clone(),
                    event_type: event.r#type.clone(),
                    url,
                    payload_json: payload_json.clone(),
                    status: crate::webhook::DeliveryStatus::Pending,
                    attempts: 0,
                    max_attempts: crate::webhook::DEFAULT_MAX_ATTEMPTS,
                    next_attempt_at: now,
                    last_status_code: None,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                    delivered_at: None,
                    tenant_id: tenant_id.unwrap_or("").to_string(),
                });
        }
    }

    pub(super) fn fresh_transaction(
        &self,
        input: &IntentInput<'_>,
        state: TransactionState,
    ) -> Transaction {
        let currency = input
            .envelope
            .context
            .currency
            .clone()
            .unwrap_or_else(|| "USD".to_string());
        let id = input
            .envelope
            .transaction_id
            .clone()
            .unwrap_or_else(|| format!("txn_{}", Uuid::new_v4().simple()));
        Transaction {
            id,
            state,
            agent_id: input.agent.raw.clone(),
            tenant_id: input.tenant.tenant_id.clone(),
            mandate_jti: input.envelope.mandate_jti.clone(),
            currency,
            jurisdiction: input.envelope.context.jurisdiction.clone(),
            buyer: Buyer::default(),
            ship_to: None,
            bill_to: None,
            line_items: Vec::new(),
            totals: Totals::default(),
            order_id: None,
            quote_expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            external_refs: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_discount_recomputes_tax_on_post_discount_subtotal() {
        // 10_000 subtotal, 8.75% tax. Apply a 2_000 discount → tax is on
        // the 8_000 discounted subtotal (700), total = 8_000 + 700 = 8_700.
        let mut totals = Totals {
            subtotal: Some(Money::new(10_000, "USD")),
            discount: None,
            shipping: None,
            tax: Some(Money::new(flat_tax_minor(10_000), "USD")),
            total: Some(Money::new(10_000 + flat_tax_minor(10_000), "USD")),
        };
        apply_discount_to_totals(&mut totals, 2_000, "USD");
        assert_eq!(totals.discount.as_ref().unwrap().amount_minor, 2_000);
        assert_eq!(totals.tax.as_ref().unwrap().amount_minor, 700);
        assert_eq!(totals.total.as_ref().unwrap().amount_minor, 8_700);
        // Subtotal is unchanged — the discount is shown separately.
        assert_eq!(totals.subtotal.as_ref().unwrap().amount_minor, 10_000);
    }

    #[test]
    fn discount_is_clamped_to_subtotal_so_total_never_goes_negative() {
        let mut totals = Totals {
            subtotal: Some(Money::new(5_000, "USD")),
            discount: None,
            shipping: None,
            tax: Some(Money::new(flat_tax_minor(5_000), "USD")),
            total: Some(Money::new(5_000 + flat_tax_minor(5_000), "USD")),
        };
        // A discount larger than the subtotal clamps to the subtotal.
        apply_discount_to_totals(&mut totals, 9_999_999, "USD");
        assert_eq!(totals.discount.as_ref().unwrap().amount_minor, 5_000);
        assert_eq!(totals.total.as_ref().unwrap().amount_minor, 0);
    }
}
