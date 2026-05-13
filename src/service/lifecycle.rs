//! Background lifecycle ticks: subscription auto-renewal + expiry sweeps.
//!
//! Both methods are driven by `crate::scheduler` on a tokio interval in
//! production, and called directly with a `now` clock from tests for
//! determinism. They consume the same per-intent helpers as the
//! agent-driven path so a scheduler renewal produces a real
//! `Transaction` and signed receipt indistinguishable from a
//! `intent.renew` invocation.

use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::events::Event;
use crate::intent::Intent;
use crate::models::{
    IntentEnvelope, PaymentInstrument, PeerQuote, PeerQuoteStatus, Subscription,
    SubscriptionStatus, Transaction, TransactionState,
};

use super::helpers::period_advance;
use super::{ExpiryTickReport, IcpService, IntentInput, SchedulerTickReport};

impl IcpService {
    /// Maximum consecutive scheduler-driven failures before a
    /// subscription transitions to `past_due` when no
    /// `subscription_dunning_schedule_hours` is configured. With a
    /// non-empty dunning schedule, the schedule length determines the
    /// max instead (see `dunning_max_attempts`).
    pub const MAX_RENEWAL_FAILURES: u32 = 3;

    /// Effective max-attempts count for a subscription failure cycle.
    /// With a non-empty dunning schedule of length N, the (N+1)th
    /// failure transitions to past_due (each failure consumes one
    /// schedule entry; once exhausted there are no more retries left
    /// to schedule). With an empty schedule, falls back to
    /// `MAX_RENEWAL_FAILURES`.
    fn dunning_max_attempts(&self) -> u32 {
        let schedule = &self.config.subscription_dunning_schedule_hours;
        if schedule.is_empty() {
            Self::MAX_RENEWAL_FAILURES
        } else {
            // schedule.len() entries = schedule.len() retry windows;
            // past_due triggers on the failure that would need the
            // (N+1)th window. So total allowed failures = N+1.
            (schedule.len() as u32).saturating_add(1)
        }
    }

    /// Compute the next-attempt timestamp after the Nth failure
    /// (1-indexed). Returns `None` when the schedule is exhausted
    /// (caller transitions to past_due) or when no schedule is set.
    fn dunning_next_attempt(
        &self,
        failure_count: u32,
        now: chrono::DateTime<Utc>,
    ) -> Option<chrono::DateTime<Utc>> {
        let schedule = &self.config.subscription_dunning_schedule_hours;
        if schedule.is_empty() {
            return None;
        }
        // failure_count is 1-based; map to the 0-based schedule index
        // by subtracting 1. After the schedule is consumed, return
        // None — caller pasts_due.
        let idx = failure_count.checked_sub(1)? as usize;
        let hours = *schedule.get(idx)?;
        Some(now + chrono::Duration::hours(hours as i64))
    }

    /// Drive automatic billing for every subscription whose
    /// `next_charge_at` has passed at `now`. Returns a summary of the
    /// work performed — useful for tests + observability.
    ///
    /// Skips subscriptions in `paused`, `canceled`, or `past_due` state,
    /// and any without a stored `payment_instrument`.
    ///
    /// Charge failures bump `failed_renewal_attempts`; once that hits
    /// [`Self::MAX_RENEWAL_FAILURES`] the subscription transitions to
    /// `past_due`. A successful renewal resets the counter.
    pub async fn tick_subscriptions(&self, now: chrono::DateTime<Utc>) -> SchedulerTickReport {
        let mut report = SchedulerTickReport::default();
        let due: Vec<Subscription> = self.subscriptions.list_due_for_renewal(now);
        report.scanned = self.subscriptions.len();
        report.due = due.len();

        for sub in due {
            // Synthesize an IntentInput-like context. Auto-renewals run
            // *without* a caller-supplied agent or mandate; the
            // subscription itself is the standing authorization.
            let charge = self.scheduler_charge(&sub).await;

            match charge {
                Ok((txn, order_summary)) => {
                    let new_period_start = sub.current_period_end;
                    let new_period_end = period_advance(new_period_start, sub.cadence);
                    let new_charges_completed = sub.charges_completed.saturating_add(1);
                    let txn_id_for_event = txn.id.clone();
                    let order_id_for_event = order_summary
                        .as_ref()
                        .map(|o| o.id.clone())
                        .or_else(|| txn.order_id.clone());
                    let total_amount_minor = txn.totals.total.as_ref().map(|m| m.amount_minor);
                    let total_currency = txn
                        .totals
                        .total
                        .as_ref()
                        .map(|m| m.currency.clone())
                        .unwrap_or_else(|| txn.currency.clone());
                    self.subscriptions.update(&sub.id, |s| {
                        s.current_period_start = new_period_start;
                        s.current_period_end = new_period_end;
                        s.next_charge_at = new_period_end;
                        s.charges_completed = new_charges_completed;
                        s.last_transaction_id = Some(txn.id.clone());
                        s.failed_renewal_attempts = 0;
                        s.updated_at = now;
                    });
                    let event = Event {
                        id: format!("evt_{}", Uuid::new_v4().simple()),
                        r#type: "subscription.renewed".into(),
                        transaction_id: Some(txn.id),
                        order_id: order_id_for_event,
                        agent_id: Some(sub.agent_id.clone()),
                        occurred_at: now,
                        payload: serde_json::json!({
                            "subscription_id": sub.id,
                            // Pre-existing — kept for back-compat.
                            "automatic": true,
                            // New cycle-context fields. Receipt-mailing
                            // automation needs `transaction_id` to link
                            // to the receipt; cycle accounting needs
                            // `cycle_number` and the period bounds;
                            // customer-facing emails need
                            // `next_charge_at` to set expectations.
                            "transaction_id": txn_id_for_event,
                            "cycle_number": new_charges_completed,
                            "current_period_start": new_period_start,
                            "current_period_end": new_period_end,
                            "next_charge_at": new_period_end,
                            "amount_minor": total_amount_minor,
                            "currency": total_currency,
                        }),
                    };
                    self.enqueue_webhook(&event, Some(&sub.tenant_id));
                    self.events.emit(event);
                    report.renewed += 1;
                    crate::metrics::record_subscription_renewal("renewed");
                }
                Err(err) => {
                    let max_attempts = self.dunning_max_attempts();
                    // Compute the next attempt BEFORE the update closure
                    // borrows — we want it pinned to the per-failure
                    // backoff, not just the next tick.
                    let new_failure_count = sub.failed_renewal_attempts.saturating_add(1);
                    let next_attempt = self.dunning_next_attempt(new_failure_count, now);
                    // Capture the error string before `err` is moved
                    // into the tracing call below so it survives into
                    // the event payload (operators paged by past_due
                    // need to know the underlying failure reason —
                    // `card_declined`, `network_timeout`, etc).
                    let err_text = err.to_string();
                    let updated = self.subscriptions.update(&sub.id, |s| {
                        s.failed_renewal_attempts = new_failure_count;
                        if s.failed_renewal_attempts >= max_attempts {
                            s.status = SubscriptionStatus::PastDue;
                        } else if let Some(nx) = next_attempt {
                            // With a dunning schedule, push the next
                            // attempt forward by the configured backoff
                            // so a transient failure doesn't burn the
                            // remaining budget in successive ticks.
                            s.next_charge_at = nx;
                        }
                        // Without a schedule, leave next_charge_at
                        // alone — the legacy "burn fast" semantics
                        // expected by `repeated_failures_transition_to_past_due`.
                        s.updated_at = now;
                    });
                    if let Some(s) = updated.as_ref() {
                        if matches!(s.status, SubscriptionStatus::PastDue) {
                            let event = Event {
                                id: format!("evt_{}", Uuid::new_v4().simple()),
                                r#type: "subscription.past_due".into(),
                                transaction_id: None,
                                order_id: None,
                                agent_id: Some(sub.agent_id.clone()),
                                occurred_at: now,
                                payload: serde_json::json!({
                                    "subscription_id": sub.id,
                                    // Pre-existing field — kept for
                                    // back-compat with existing
                                    // receivers.
                                    "consecutive_failures": s.failed_renewal_attempts,
                                    // New triage fields. Operators
                                    // paged by past_due switch on
                                    // `next_action` to choose between
                                    // automated retry vs surfacing to
                                    // a human, and surface
                                    // `last_error` to the customer
                                    // for self-service repair (e.g.
                                    // "your card was declined; please
                                    // update payment method").
                                    "attempts_made": s.failed_renewal_attempts,
                                    "last_error": err_text,
                                    "last_attempt_at": now,
                                    "next_action": "manual_renewal_required",
                                }),
                            };
                            self.enqueue_webhook(&event, Some(&sub.tenant_id));
                            self.events.emit(event);
                            report.past_due += 1;
                            crate::metrics::record_subscription_renewal("past_due");
                        }
                    }
                    tracing::warn!(
                        subscription_id = %sub.id,
                        attempts = updated
                            .as_ref()
                            .map(|s| s.failed_renewal_attempts)
                            .unwrap_or(0),
                        error = %err,
                        "scheduler renewal charge failed",
                    );
                    report.failed += 1;
                    crate::metrics::record_subscription_renewal("failed");
                }
            }
        }
        report
    }

    /// Sweep transactions and peer quotes whose expiry deadline has
    /// passed and transition them to their terminal `Expired` state.
    /// Without this, a stale quote stays in `Quoted` forever and could
    /// be authorized at the original price hours/days later — a real
    /// pricing/inventory consistency bug.
    ///
    /// **Transactions**: any txn with `quote_expires_at <= now` AND
    /// state ∈ `{Draft, Quoted}` transitions to `Expired`. Authorized,
    /// captured, fulfilled, and completed transactions are skipped —
    /// the caller has already moved past the quote-expiry window.
    /// Already-terminal transactions are skipped.
    ///
    /// **Peer quotes**: any quote with `expires_at <= now` AND status
    /// ∈ `{Pending, Quoted}` transitions to `Expired`. Accepted,
    /// expired, and rejected quotes are skipped (terminal).
    ///
    /// Each transition emits an `<entity>.expired` event onto the
    /// originating tenant's webhook + SSE channels.
    pub async fn tick_expiries(&self, now: chrono::DateTime<Utc>) -> ExpiryTickReport {
        let mut report = ExpiryTickReport::default();

        // Transactions ----------------------------------------------------
        let due_txns: Vec<Transaction> = self.transactions.list_due_for_expiry(now);
        for txn in due_txns {
            self.transactions.update(&txn.id, |t| {
                t.state = TransactionState::Expired;
                t.updated_at = now;
            });
            let event = Event {
                id: format!("evt_{}", Uuid::new_v4().simple()),
                r#type: "transaction.expired".into(),
                transaction_id: Some(txn.id.clone()),
                order_id: None,
                agent_id: Some(txn.agent_id.clone()),
                occurred_at: now,
                payload: serde_json::json!({
                    "transaction_id": txn.id,
                    "expired_at": now,
                    // The state the txn was in BEFORE the sweep
                    // transitioned it. Operators handle Draft vs
                    // Quoted differently — Quoted means a customer
                    // saw a price they didn't act on (re-quote at
                    // current price); Draft means they never made
                    // it past initial discovery.
                    "previous_state": txn.state.wire_name(),
                    // The deadline that was hit. Often equals
                    // expired_at within the sweep tick, but operator
                    // log searches want the original deadline that
                    // was set at quote time.
                    "quote_expires_at": txn.quote_expires_at,
                    // Customer-facing renewal/notification fields.
                    // Best-effort — buyer fields populate only if the
                    // upstream intent supplied them.
                    "buyer_email": txn.buyer.email,
                    // Quoted amount that the customer saw, so a
                    // re-quote email can include the original price
                    // for context. Best-effort — totals may be unset
                    // on a Draft txn.
                    "amount_minor": txn.totals.total.as_ref().map(|m| m.amount_minor),
                    "currency": txn
                        .totals
                        .total
                        .as_ref()
                        .map(|m| m.currency.clone())
                        .unwrap_or_else(|| txn.currency.clone()),
                    // Stable operator-actionable enum — handlers
                    // switch on this to choose between automated
                    // re-quote vs surfacing to a human.
                    "next_action": "re_quote_required",
                }),
            };
            self.enqueue_webhook(&event, Some(&txn.tenant_id));
            self.events.emit(event);
            report.transactions_expired += 1;
        }

        // Peer quotes — sweep across all tenants. The user-facing
        // list endpoint is tenant-scoped; this background pass is
        // operator-level and needs the unrestricted view.
        let due_quotes: Vec<PeerQuote> = self.peer_quotes.list_due_for_expiry(now);
        for quote in due_quotes {
            self.peer_quotes.update(&quote.id, |q| {
                q.status = PeerQuoteStatus::Expired;
                q.updated_at = now;
            });
            let event = Event {
                id: format!("evt_{}", Uuid::new_v4().simple()),
                r#type: "peer_quote.expired".into(),
                transaction_id: None,
                order_id: None,
                agent_id: Some(quote.requester_agent_id.clone()),
                occurred_at: now,
                payload: serde_json::json!({
                    "peer_quote_id": quote.id,
                    "expired_at": now,
                    // Pre-expiry status: Pending vs Quoted.
                    // Operators handle them differently — Pending
                    // means the peer never priced; Quoted means a
                    // priced offer aged out.
                    "previous_status": quote.status.wire_name(),
                    // Both agent ids so receivers don't have to
                    // re-fetch the quote to know who the parties
                    // were.
                    "requester_agent_id": quote.requester_agent_id,
                    "peer_agent_id": quote.peer_agent_id,
                    // Service spec — what was being requested.
                    // Operators dashboarding A2A activity want this
                    // to segment by service kind.
                    "service_kind": quote.service.kind,
                    // The price the requester saw (if any) so a
                    // re-quote workflow can decide whether to
                    // re-issue at the same target.
                    "price_amount_minor": quote.price.as_ref().map(|p| p.amount_minor),
                    "price_currency": quote.price.as_ref().map(|p| p.currency.clone()),
                    "next_action": "re_quote_required",
                }),
            };
            self.enqueue_webhook(&event, Some(&quote.tenant_id));
            self.events.emit(event);
            report.peer_quotes_expired += 1;
        }

        report
    }

    /// Run a single scheduler-initiated charge using the subscription's
    /// stored payment instrument. Returns the completed transaction and
    /// any order persisted for it.
    async fn scheduler_charge(
        &self,
        sub: &Subscription,
    ) -> Result<(Transaction, Option<crate::models::OrderSummary>), ApiError> {
        // Synthesize an IntentInput just to reuse the existing
        // line-item pricing path.
        let envelope = IntentEnvelope {
            intent: Intent::Renew.wire_name().to_string(),
            intent_id: None,
            transaction_id: None,
            agent_id: sub.agent_id.clone(),
            mandate_jti: sub.mandate_jti.clone(),
            params: Value::Null,
            context: Default::default(),
        };
        let agent = crate::agent::AgentIdentifier::parse(&sub.agent_id);
        let tenant = crate::agent::ApiKeyInfo {
            key: String::new(),
            tenant_id: sub.tenant_id.clone(),
            name: "subscription-scheduler".into(),
            rate_limit_per_minute: None,
            allowed_agents: None,
            expires_at: None,
        };
        let input = IntentInput::for_compat(
            envelope,
            agent,
            tenant,
            format!("req_{}", Uuid::new_v4().simple()),
            None,
        );
        // Validate the saved payment instrument is still something we
        // can charge against. Card / delegated_vault / stablecoin are
        // all acceptable; A2A is rejected because peer commerce
        // requires interactive consent.
        if let Some(PaymentInstrument::A2A { .. }) = sub.payment_instrument {
            return Err(ApiError::PreconditionFailed(
                "A2A payment instruments cannot be auto-renewed".into(),
            ));
        }

        self.run_subscription_charge(
            &input,
            &sub.buyer,
            sub.ship_to.clone(),
            &sub.items,
            &sub.currency,
        )
        .await
    }
}
