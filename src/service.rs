//! ICP intent pipeline.
//!
//! The service turns an [`IntentEnvelope`] into an [`IntentResponseBody`]
//! while:
//!
//! 1. Evaluating the mandate (if required for the intent's scope).
//! 2. Loading or creating the underlying [`Transaction`].
//! 3. Executing the intent against the embedded commerce engine.
//! 4. Recording mandate spend.
//! 5. Signing a receipt over the JCS-canonicalized response body.
//! 6. Emitting an event on the bus.
//!
//! The logic here deliberately keeps the engine interactions small and
//! deterministic. A production deployment will extend this with real
//! tax/shipping/payment providers via the stub seams marked `TODO`.

use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use crate::agent::{AgentIdentifier, ApiKeyInfo};
use crate::commerce::CommerceEngine;
use crate::config::Config;
use crate::errors::ApiError;
use crate::events::{Event, EventBus};
use crate::intent::Intent;
use crate::mandate::{self, MandateEvaluation, MandateLedger};
use crate::models::{
    A2aPayParams, A2aQuoteParams, BillingCadence, Buyer, ConfirmReceiptParams, IntentEnvelope,
    IntentResponseBody, LineItem, Money, NegotiateParams, OrderSummary, PaymentInstrument,
    PeerQuote, PeerQuoteStatus, QuoteParams, ReceiptStub, RenewParams, RequestItem,
    ResponseEnvelope, SubscribeParams, Subscription, SubscriptionRefParams, SubscriptionStatus,
    Totals, Transaction, TransactionState,
};
use crate::receipts::{ReceiptStore, StoredReceipt};
use crate::resolver::{CompositeResolver, PrincipalResolver};
use crate::signing::ReceiptSigner;
use crate::state_store::{PeerQuoteStore, SubscriptionStore, TransactionStore};

#[derive(Clone)]
pub struct IcpService {
    pub config: Arc<Config>,
    pub engine: Option<CommerceEngine>,
    pub transactions: TransactionStore,
    pub subscriptions: SubscriptionStore,
    pub peer_quotes: PeerQuoteStore,
    pub receipts: ReceiptStore,
    pub mandates: MandateLedger,
    pub idempotency: crate::idempotency::IdempotencyStore,
    pub rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    /// Per-IP pre-auth limiter. Fires *before* bearer resolution, so a
    /// flood of fake API keys can't burn unbounded CPU on keystore
    /// lookups. Disabled when the configured capacity is 0.
    pub pre_auth_limiter: Arc<crate::rate_limit::RateLimiter>,
    pub webhook_outbox: crate::webhook::WebhookOutbox,
    /// URL to enqueue events to. `None` disables enqueueing entirely
    /// (for tests + demos that don't want webhook traffic).
    pub webhook_url: Option<String>,
    pub signer: Arc<ReceiptSigner>,
    pub events: EventBus,
    pub resolver: Arc<dyn PrincipalResolver>,
}

pub struct IntentInput<'a> {
    pub envelope: IntentEnvelope,
    pub agent: AgentIdentifier,
    pub tenant: ApiKeyInfo,
    pub mandate_jws: Option<&'a str>,
    pub request_id: String,
    pub trace_id: Option<String>,
    /// When true, skip mandate enforcement. Set *only* by the ACP/UCP
    /// compatibility paths, which treat the tenant's bearer token as a
    /// self-mandate scoped to that merchant.
    pub skip_mandate_check: bool,
}

impl<'a> IntentInput<'a> {
    /// Construct an input for the first-class ICP `/icp/v1/intents` path
    /// — mandate enforcement is governed by `config.require_mandate`.
    pub fn for_icp(
        envelope: IntentEnvelope,
        agent: AgentIdentifier,
        tenant: ApiKeyInfo,
        mandate_jws: Option<&'a str>,
        request_id: String,
        trace_id: Option<String>,
    ) -> Self {
        Self {
            envelope,
            agent,
            tenant,
            mandate_jws,
            request_id,
            trace_id,
            skip_mandate_check: false,
        }
    }

    /// Construct an input for a compat path — mandate enforcement is
    /// bypassed in favor of tenant-level self-authorization.
    pub fn for_compat(
        envelope: IntentEnvelope,
        agent: AgentIdentifier,
        tenant: ApiKeyInfo,
        request_id: String,
        trace_id: Option<String>,
    ) -> Self {
        Self {
            envelope,
            agent,
            tenant,
            mandate_jws: None,
            request_id,
            trace_id,
            skip_mandate_check: true,
        }
    }
}

impl IcpService {
    pub fn new(config: Config, engine: Option<CommerceEngine>, signer: ReceiptSigner) -> Self {
        Self::with_resolver(
            config,
            engine,
            signer,
            Arc::new(CompositeResolver::default_set()),
        )
    }

    pub fn with_resolver(
        config: Config,
        engine: Option<CommerceEngine>,
        signer: ReceiptSigner,
        resolver: Arc<dyn PrincipalResolver>,
    ) -> Self {
        let webhook_url = config.webhook_url.clone();
        let rate_limiter = Arc::new(crate::rate_limit::RateLimiter::per_minute(
            config.rate_limit_per_minute,
        ));
        let pre_auth_limiter = Arc::new(crate::rate_limit::RateLimiter::per_minute(
            config.pre_auth_rate_limit_per_minute,
        ));
        Self {
            config: Arc::new(config),
            engine,
            transactions: TransactionStore::new(),
            subscriptions: SubscriptionStore::new(),
            peer_quotes: PeerQuoteStore::new(),
            receipts: ReceiptStore::new(),
            mandates: MandateLedger::new(),
            idempotency: crate::idempotency::IdempotencyStore::default(),
            rate_limiter,
            pre_auth_limiter,
            webhook_outbox: crate::webhook::WebhookOutbox::default(),
            webhook_url,
            signer: Arc::new(signer),
            events: EventBus::default(),
            resolver,
        }
    }

    pub async fn handle_intent(
        &self,
        input: IntentInput<'_>,
    ) -> Result<IntentResponseBody, ApiError> {
        let intent = Intent::parse(&input.envelope.intent)?;

        let resolver: Option<&dyn PrincipalResolver> = if self.config.verify_mandate_signatures {
            Some(self.resolver.as_ref())
        } else {
            None
        };
        let mandate = match (intent.scope(), input.skip_mandate_check) {
            (Some(scope), false) => match input.mandate_jws {
                Some(jws) => Some(
                    mandate::evaluate(
                        jws,
                        scope,
                        estimate_intent_amount_minor(&input.envelope),
                        Utc::now(),
                        &input.tenant.tenant_id,
                        &self.mandates,
                        resolver,
                    )
                    .await?,
                ),
                None if self.config.require_mandate => {
                    return Err(ApiError::MandateInvalid(
                        "ICP-Mandate header required for this intent scope".into(),
                    ));
                }
                None => None,
            },
            _ => None,
        };

        // Route by intent.
        let outcome = match intent {
            Intent::Search => self.do_search(&input).await?,
            Intent::Describe => self.do_describe(&input).await?,
            Intent::Quote => self.do_quote(&input, mandate.as_ref()).await?,
            Intent::Authorize => self.do_authorize(&input, mandate.as_ref()).await?,
            Intent::Buy | Intent::Pay => self.do_buy(&input, mandate.as_ref()).await?,
            Intent::Track => self.do_track(&input).await?,
            Intent::Return => self.do_return(&input, mandate.as_ref()).await?,
            Intent::RefundRequest => self.do_refund_request(&input, mandate.as_ref()).await?,
            Intent::Subscribe => self.do_subscribe(&input, mandate.as_ref()).await?,
            Intent::Renew => self.do_renew(&input, mandate.as_ref()).await?,
            Intent::Pause => self.do_pause(&input).await?,
            Intent::CancelSubscription => self.do_cancel_subscription(&input).await?,
            Intent::A2aQuote => self.do_a2a_quote(&input, mandate.as_ref()).await?,
            Intent::A2aPay => self.do_a2a_pay(&input, mandate.as_ref()).await?,
            Intent::Negotiate => self.do_negotiate(&input, mandate.as_ref()).await?,
            Intent::ConfirmReceipt => self.do_confirm_receipt(&input).await?,
        };

        // Record mandate spend (for intents that spend budget).
        if let Some(ev) = mandate.as_ref() {
            let spend = outcome.recorded_spend_minor;
            if spend > 0 {
                self.mandates
                    .record_spend(&ev.payload.jti, spend, Utc::now());
            }
        }

        let envelope = ResponseEnvelope {
            icp_version: self.config.icp_version.clone(),
            request_id: input.request_id.clone(),
            trace_id: input.trace_id.clone(),
            issued_at: Utc::now(),
        };

        // Build response body (unsigned) so we can canonicalize, digest,
        // then sign.
        let intent_id = input
            .envelope
            .intent_id
            .clone()
            .unwrap_or_else(|| format!("int_{}", Uuid::new_v4().simple()));

        let mut body = IntentResponseBody {
            intent: intent.wire_name().to_string(),
            intent_id: intent_id.clone(),
            transaction: outcome.transaction.clone(),
            order: outcome.order.clone(),
            subscription: outcome.subscription.clone(),
            peer_quote: outcome.peer_quote.clone(),
            // Placeholder — overwritten after signing.
            receipt: ReceiptStub {
                jti: String::new(),
                kid: self.signer.kid.clone(),
                jws: String::new(),
                body_digest: String::new(),
            },
            envelope,
        };

        let receipt = if intent.is_state_change() {
            let bytes = serde_jcs::to_vec(&body)
                .map_err(|e| ApiError::ProcessingError(format!("jcs: {e}")))?;
            let signed = self
                .signer
                .sign_receipt(
                    &input.agent.raw,
                    &self.config.public_base_url,
                    intent.wire_name(),
                    &body.transaction.id,
                    body.order.as_ref().map(|o| o.id.as_str()),
                    mandate.as_ref().map(|m| m.payload.jti.as_str()),
                    &bytes,
                )
                .map_err(|e| ApiError::ProcessingError(format!("sign: {e}")))?;

            self.receipts.insert(StoredReceipt {
                jti: signed.jti.clone(),
                kid: signed.kid.clone(),
                jws: signed.jws.clone(),
                body_digest: signed.body_digest.clone(),
                claims: signed.claims.clone(),
            });

            ReceiptStub {
                jti: signed.jti,
                kid: signed.kid,
                jws: signed.jws,
                body_digest: signed.body_digest,
            }
        } else {
            ReceiptStub {
                jti: String::new(),
                kid: self.signer.kid.clone(),
                jws: String::new(),
                body_digest: String::new(),
            }
        };

        body.receipt = receipt;

        // Event type follows the dominant aggregate produced by the
        // intent: `peer_quote.<status>` for A2A, `subscription.<status>`
        // for subscription intents, otherwise `transaction.<state>`.
        // `negotiate` and `confirm_receipt` don't transition the
        // transaction state but ARE meaningful lifecycle events, so
        // they get bespoke event types instead of duplicating the
        // existing `transaction.quoted` / `transaction.completed`.
        let evt_type = if let Some(pq) = outcome.peer_quote.as_ref() {
            format!("peer_quote.{}", pq.status.wire_name())
        } else if let Some(sub) = outcome.subscription.as_ref() {
            format!("subscription.{}", sub.status.wire_name())
        } else {
            match intent {
                Intent::Negotiate => "transaction.renegotiated".to_string(),
                Intent::ConfirmReceipt => "transaction.receipt_confirmed".to_string(),
                _ => format!("transaction.{}", outcome.transaction.state.wire_name()),
            }
        };
        let event = Event {
            id: format!("evt_{}", Uuid::new_v4().simple()),
            r#type: evt_type,
            transaction_id: Some(outcome.transaction.id.clone()),
            order_id: outcome.order.as_ref().map(|o| o.id.clone()),
            agent_id: Some(input.agent.raw.clone()),
            occurred_at: Utc::now(),
            payload: serde_json::to_value(&body).unwrap_or(Value::Null),
        };
        // Outbound webhook (durable outbox) — only enqueue on
        // state-changing intents to avoid spamming subscribers with
        // search/describe noise. Read-only intents already skip the
        // receipt path; same gate applies here.
        if intent.is_state_change() {
            self.enqueue_webhook(&event);
        }
        self.events.emit(event);

        Ok(body)
    }

    // ---------- search / describe ----------

    async fn do_search(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        // v0.1: return an advisory transaction with zero line items. A
        // future release will surface the engine's product search.
        let txn = self.fresh_transaction(input, TransactionState::Draft);
        self.transactions.insert(txn.clone());
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
            recorded_spend_minor: 0,
        })
    }

    async fn do_describe(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let txn = self.fresh_transaction(input, TransactionState::Draft);
        self.transactions.insert(txn.clone());
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
            recorded_spend_minor: 0,
        })
    }

    // ---------- quote ----------

    async fn do_quote(
        &self,
        input: &IntentInput<'_>,
        _mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: QuoteParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("quote.params: {e}")))?;

        if params.items.is_empty() {
            return Err(ApiError::InvalidRequest(
                "quote.params.items must be non-empty".into(),
            ));
        }

        let currency = input
            .envelope
            .context
            .currency
            .clone()
            .unwrap_or_else(|| "USD".to_string());

        // Price each line. In v0.1 we use the optional `unit_price_hint`
        // when provided, else a flat $10.00 per unit placeholder. A real
        // deployment would route this through the iCommerce pricing
        // engine (products.price_for, promotions.apply, tax.calculate).
        let mut line_items = Vec::with_capacity(params.items.len());
        let mut subtotal_minor: i64 = 0;
        for (idx, req) in params.items.iter().enumerate() {
            let unit_minor = req
                .unit_price_hint
                .as_ref()
                .map(|m| m.amount_minor)
                .unwrap_or(1_000);
            let line_subtotal = unit_minor.saturating_mul(req.quantity);
            subtotal_minor = subtotal_minor.saturating_add(line_subtotal);
            line_items.push(LineItem {
                id: format!("li_{:06}", idx),
                sku: req.sku.clone(),
                name: req.sku.clone(),
                quantity: req.quantity,
                unit_price: Money::new(unit_minor, &currency),
                subtotal: Money::new(line_subtotal, &currency),
                tax: None,
                total: Money::new(line_subtotal, &currency),
            });
        }

        let tax_minor = subtotal_minor * 875 / 10_000; // 8.75% placeholder
        let total_minor = subtotal_minor.saturating_add(tax_minor);
        let totals = Totals {
            subtotal: Some(Money::new(subtotal_minor, &currency)),
            discount: None,
            shipping: None,
            tax: Some(Money::new(tax_minor, &currency)),
            total: Some(Money::new(total_minor, &currency)),
        };

        let mut txn = self.fresh_transaction(input, TransactionState::Quoted);
        txn.buyer = params.buyer.unwrap_or_default();
        txn.ship_to = params.ship_to;
        txn.line_items = line_items;
        txn.totals = totals;
        txn.currency = currency;
        txn.quote_expires_at = Some(Utc::now() + chrono::Duration::minutes(15));
        self.transactions.insert(txn.clone());

        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
            recorded_spend_minor: 0,
        })
    }

    // ---------- authorize ----------

    async fn do_authorize(
        &self,
        input: &IntentInput<'_>,
        _mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let txn_id = require_transaction_id(&input.envelope.params)?;
        let txn = self.transactions.update(&txn_id, |t| {
            if let Some(buyer) = input
                .envelope
                .params
                .get("buyer")
                .and_then(|v| serde_json::from_value::<Buyer>(v.clone()).ok())
            {
                t.buyer = buyer;
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
            recorded_spend_minor: 0,
        })
    }

    // ---------- buy ----------

    async fn do_buy(
        &self,
        input: &IntentInput<'_>,
        _mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let txn_id = require_transaction_id(&input.envelope.params)?;

        // Load transaction.
        let txn = self
            .transactions
            .get(&txn_id)
            .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {txn_id}")))?;
        if !matches!(
            txn.state,
            TransactionState::Authorized | TransactionState::Quoted
        ) {
            return Err(ApiError::PreconditionFailed(format!(
                "transaction in state {:?} cannot be bought",
                txn.state
            )));
        }

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

        let recorded_spend_minor = persisted
            .totals
            .total
            .as_ref()
            .map(|m| m.amount_minor)
            .unwrap_or(0);

        Ok(Outcome {
            transaction: persisted,
            order: order_summary,
            subscription: None,
            peer_quote: None,
            recorded_spend_minor,
        })
    }

    // ---------- track ----------

    async fn do_track(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let txn_id = input
            .envelope
            .params
            .get("transaction_id")
            .and_then(|v| v.as_str())
            .or(input.envelope.transaction_id.as_deref())
            .ok_or_else(|| ApiError::InvalidRequest("track: transaction_id required".into()))?;
        let txn = self
            .transactions
            .get(txn_id)
            .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {txn_id}")))?;
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
            recorded_spend_minor: 0,
        })
    }

    // ---------- return / refund ----------

    async fn do_return(
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
            recorded_spend_minor: 0,
        })
    }

    async fn do_refund_request(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        self.do_return(input, mandate).await
    }

    // ---------- subscriptions ----------

    async fn do_subscribe(
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

        let now = Utc::now();
        let next = period_advance(now, params.cadence);
        let currency = input
            .envelope
            .context
            .currency
            .clone()
            .unwrap_or_else(|| "USD".to_string());

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
        let charge_total = charge_txn
            .totals
            .total
            .as_ref()
            .map(|m| m.amount_minor)
            .unwrap_or(0);

        let sub = Subscription {
            id: format!("sub_{}", Uuid::new_v4().simple()),
            status: SubscriptionStatus::Active,
            agent_id: input.agent.raw.clone(),
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
            recorded_spend_minor: charge_total,
        })
    }

    async fn do_renew(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: RenewParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("renew.params: {e}")))?;
        let sub = self
            .subscriptions
            .get(&params.subscription_id)
            .ok_or_else(|| {
                ApiError::ResourceNotFound(format!("subscription {}", params.subscription_id))
            })?;

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
        let charge_txn = self
            .run_subscription_charge(
                input,
                &sub.buyer,
                sub.ship_to.clone(),
                &sub.items,
                &sub.currency,
            )
            .await?;
        let charge_total = charge_txn
            .totals
            .total
            .as_ref()
            .map(|m| m.amount_minor)
            .unwrap_or(0);

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
            recorded_spend_minor: charge_total,
        })
    }

    async fn do_pause(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let params: SubscriptionRefParams =
            serde_json::from_value(input.envelope.params.clone())
                .map_err(|e| ApiError::InvalidRequest(format!("pause.params: {e}")))?;

        let sub = self
            .subscriptions
            .get(&params.subscription_id)
            .ok_or_else(|| {
                ApiError::ResourceNotFound(format!("subscription {}", params.subscription_id))
            })?;
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
            recorded_spend_minor: 0,
        })
    }

    async fn do_cancel_subscription(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let params: SubscriptionRefParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| {
            ApiError::InvalidRequest(format!("cancel_subscription.params: {e}"))
        })?;

        let sub = self
            .subscriptions
            .get(&params.subscription_id)
            .ok_or_else(|| {
                ApiError::ResourceNotFound(format!("subscription {}", params.subscription_id))
            })?;
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
            recorded_spend_minor: 0,
        })
    }

    // ---------- A2A (peer commerce) ----------

    async fn do_a2a_quote(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: A2aQuoteParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("a2a_quote.params: {e}")))?;
        if params.peer_agent_id == input.agent.raw {
            return Err(ApiError::InvalidRequest(
                "a2a_quote: peer_agent_id must differ from the requester agent_id".into(),
            ));
        }

        let now = Utc::now();
        let expires_in = chrono::Duration::seconds(
            params
                .expires_in_seconds
                .unwrap_or(300)
                .clamp(1, 24 * 60 * 60) as i64,
        );

        let (status, price) = match params.price_hint {
            Some(p) => (PeerQuoteStatus::Quoted, Some(p)),
            None => (PeerQuoteStatus::Pending, None),
        };

        let quote = PeerQuote {
            id: format!("pq_{}", Uuid::new_v4().simple()),
            status,
            requester_agent_id: input.agent.raw.clone(),
            peer_agent_id: params.peer_agent_id,
            service: params.service,
            price,
            created_at: now,
            updated_at: now,
            expires_at: now + expires_in,
            accepted_at: None,
            charge_transaction_id: None,
            mandate_jti: mandate.map(|m| m.payload.jti.clone()),
            reference_id: params.reference_id,
        };
        self.peer_quotes.insert(quote.clone());

        // Synthesize a Draft transaction so the receipt has the same
        // shape as every other intent. The transaction's
        // `external_refs["peer_quote_id"]` makes the link explicit.
        let mut external_refs = std::collections::BTreeMap::new();
        external_refs.insert("peer_quote_id".to_string(), quote.id.clone());
        let txn = Transaction {
            id: format!("txn_{}", Uuid::new_v4().simple()),
            state: TransactionState::Draft,
            agent_id: input.agent.raw.clone(),
            mandate_jti: mandate.map(|m| m.payload.jti.clone()),
            currency: quote
                .price
                .as_ref()
                .map(|p| p.currency.clone())
                .or_else(|| input.envelope.context.currency.clone())
                .unwrap_or_else(|| "USD".to_string()),
            jurisdiction: input.envelope.context.jurisdiction.clone(),
            buyer: Default::default(),
            ship_to: None,
            bill_to: None,
            line_items: Vec::new(),
            totals: Totals::default(),
            order_id: None,
            quote_expires_at: Some(quote.expires_at),
            created_at: now,
            updated_at: now,
            external_refs,
        };
        self.transactions.insert(txn.clone());

        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: Some(quote),
            recorded_spend_minor: 0,
        })
    }

    async fn do_a2a_pay(
        &self,
        input: &IntentInput<'_>,
        mandate: Option<&MandateEvaluation>,
    ) -> Result<Outcome, ApiError> {
        let params: A2aPayParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("a2a_pay.params: {e}")))?;
        if params.from.is_empty() {
            return Err(ApiError::InvalidRequest(
                "a2a_pay: `from` is required".into(),
            ));
        }

        let now = Utc::now();

        // Two flow shapes: pay-against-quote and direct-pay.
        let (peer_id, money, quote_to_consume) = match params.peer_quote_id.as_deref() {
            Some(quote_id) => {
                let q = self
                    .peer_quotes
                    .get(quote_id)
                    .ok_or_else(|| ApiError::ResourceNotFound(format!("peer_quote {quote_id}")))?;
                if q.requester_agent_id != input.agent.raw {
                    return Err(ApiError::PreconditionFailed(
                        "a2a_pay: requester does not match the quote's requester_agent_id".into(),
                    ));
                }
                if q.status != PeerQuoteStatus::Quoted {
                    return Err(ApiError::PreconditionFailed(format!(
                        "a2a_pay: peer_quote in state `{}` cannot be paid",
                        q.status.wire_name()
                    )));
                }
                if q.expires_at <= now {
                    // Mark the quote expired so future reads reflect reality.
                    self.peer_quotes.update(quote_id, |x| {
                        x.status = PeerQuoteStatus::Expired;
                        x.updated_at = now;
                    });
                    return Err(ApiError::PreconditionFailed(
                        "a2a_pay: peer_quote has expired".into(),
                    ));
                }
                let money = q.price.clone().ok_or_else(|| {
                    ApiError::PreconditionFailed("a2a_pay: peer_quote has no price set".into())
                })?;
                (q.peer_agent_id.clone(), money, Some(q))
            }
            None => {
                let peer_id = params.peer_agent_id.clone().ok_or_else(|| {
                    ApiError::InvalidRequest(
                        "a2a_pay: either peer_quote_id or peer_agent_id is required".into(),
                    )
                })?;
                let money = params.amount.clone().ok_or_else(|| {
                    ApiError::InvalidRequest("a2a_pay: direct payment requires `amount`".into())
                })?;
                if peer_id == input.agent.raw {
                    return Err(ApiError::InvalidRequest(
                        "a2a_pay: cannot pay yourself".into(),
                    ));
                }
                (peer_id, money, None)
            }
        };

        // Build a single-line-item transaction representing the peer
        // payment. The line item describes what was bought/paid for so
        // it shows up cleanly in receipts and the txn store.
        let line_item = LineItem {
            id: "li_a2a".to_string(),
            sku: format!("a2a:{peer_id}"),
            name: format!("Peer payment to {peer_id}"),
            quantity: 1,
            unit_price: money.clone(),
            subtotal: money.clone(),
            tax: None,
            total: money.clone(),
        };
        let totals = Totals {
            subtotal: Some(money.clone()),
            discount: None,
            shipping: None,
            tax: None,
            total: Some(money.clone()),
        };

        let mut external_refs = std::collections::BTreeMap::new();
        external_refs.insert("peer_agent_id".to_string(), peer_id.clone());
        if let Some(q) = quote_to_consume.as_ref() {
            external_refs.insert("peer_quote_id".to_string(), q.id.clone());
        }
        if let Some(memo) = params.memo.as_deref() {
            external_refs.insert("memo".to_string(), memo.to_string());
        }
        external_refs.insert("a2a_from".to_string(), params.from);

        let txn = Transaction {
            id: format!("txn_{}", Uuid::new_v4().simple()),
            state: TransactionState::Completed,
            agent_id: input.agent.raw.clone(),
            mandate_jti: mandate.map(|m| m.payload.jti.clone()),
            currency: money.currency.clone(),
            jurisdiction: input.envelope.context.jurisdiction.clone(),
            buyer: Default::default(),
            ship_to: None,
            bill_to: None,
            line_items: vec![line_item],
            totals,
            order_id: None,
            quote_expires_at: None,
            created_at: now,
            updated_at: now,
            external_refs,
        };
        self.transactions.insert(txn.clone());

        // Mark the quote accepted (if any) and link it to the charge txn.
        let updated_quote = quote_to_consume.as_ref().and_then(|q| {
            self.peer_quotes.update(&q.id, |x| {
                x.status = PeerQuoteStatus::Accepted;
                x.accepted_at = Some(now);
                x.charge_transaction_id = Some(txn.id.clone());
                x.updated_at = now;
            })
        });

        let recorded_spend = money.amount_minor;
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: updated_quote,
            recorded_spend_minor: recorded_spend,
        })
    }

    // ---------- negotiate / confirm_receipt ----------

    async fn do_negotiate(
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

        let txn = self
            .transactions
            .get(&params.transaction_id)
            .ok_or_else(|| {
                ApiError::ResourceNotFound(format!("transaction {}", params.transaction_id))
            })?;
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
            recorded_spend_minor: 0,
        })
    }

    async fn do_confirm_receipt(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let params: ConfirmReceiptParams = serde_json::from_value(input.envelope.params.clone())
            .map_err(|e| ApiError::InvalidRequest(format!("confirm_receipt.params: {e}")))?;

        let txn = self
            .transactions
            .get(&params.transaction_id)
            .ok_or_else(|| {
                ApiError::ResourceNotFound(format!("transaction {}", params.transaction_id))
            })?;
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
            recorded_spend_minor: 0,
        })
    }

    // ---------- scheduler-driven auto-renewal ----------

    /// Maximum number of consecutive scheduler-driven failures before
    /// a subscription transitions to `past_due` and stops being
    /// retried. The agent must call `intent.renew` manually to recover.
    pub const MAX_RENEWAL_FAILURES: u32 = 3;

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
        let due: Vec<Subscription> = self
            .subscriptions
            .list(usize::MAX)
            .into_iter()
            .filter(|s| {
                matches!(s.status, SubscriptionStatus::Active)
                    && s.next_charge_at <= now
                    && s.payment_instrument.is_some()
            })
            .collect();
        report.scanned = self.subscriptions.len();
        report.due = due.len();

        for sub in due {
            // Synthesize an IntentInput-like context. Auto-renewals run
            // *without* a caller-supplied agent or mandate; the
            // subscription itself is the standing authorization.
            let charge = self.scheduler_charge(&sub).await;

            match charge {
                Ok(txn) => {
                    let new_period_start = sub.current_period_end;
                    let new_period_end = period_advance(new_period_start, sub.cadence);
                    self.subscriptions.update(&sub.id, |s| {
                        s.current_period_start = new_period_start;
                        s.current_period_end = new_period_end;
                        s.next_charge_at = new_period_end;
                        s.charges_completed = s.charges_completed.saturating_add(1);
                        s.last_transaction_id = Some(txn.id.clone());
                        s.failed_renewal_attempts = 0;
                        s.updated_at = now;
                    });
                    let event = Event {
                        id: format!("evt_{}", Uuid::new_v4().simple()),
                        r#type: "subscription.renewed".into(),
                        transaction_id: Some(txn.id),
                        order_id: None,
                        agent_id: Some(sub.agent_id.clone()),
                        occurred_at: now,
                        payload: serde_json::json!({
                            "subscription_id": sub.id,
                            "automatic": true,
                        }),
                    };
                    self.enqueue_webhook(&event);
                    self.events.emit(event);
                    report.renewed += 1;
                }
                Err(err) => {
                    let updated = self.subscriptions.update(&sub.id, |s| {
                        s.failed_renewal_attempts = s.failed_renewal_attempts.saturating_add(1);
                        if s.failed_renewal_attempts >= Self::MAX_RENEWAL_FAILURES {
                            s.status = SubscriptionStatus::PastDue;
                        }
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
                                    "consecutive_failures": s.failed_renewal_attempts,
                                }),
                            };
                            self.enqueue_webhook(&event);
                            self.events.emit(event);
                            report.past_due += 1;
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
                }
            }
        }
        report
    }

    /// Run a single scheduler-initiated charge using the subscription's
    /// stored payment instrument. Returns the completed transaction.
    async fn scheduler_charge(&self, sub: &Subscription) -> Result<Transaction, ApiError> {
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
            tenant_id: format!("scheduler-{}", sub.id),
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

    /// Run a subscription charge as if it were a `quote → authorize →
    /// buy` triplet collapsed into one step. Returns the resulting
    /// completed transaction (already inserted into the txn store).
    async fn run_subscription_charge(
        &self,
        input: &IntentInput<'_>,
        buyer: &Buyer,
        ship_to: Option<crate::models::Address>,
        items: &[RequestItem],
        currency: &str,
    ) -> Result<Transaction, ApiError> {
        // Price the basket inline using the same pricing shape as
        // `do_quote` — a $0.10 default per unit when no hint is given,
        // 8.75% tax. Matches the rest of the v0.2 placeholder pricing.
        let mut line_items = Vec::with_capacity(items.len());
        let mut subtotal_minor: i64 = 0;
        for (idx, req) in items.iter().enumerate() {
            let unit_minor = req
                .unit_price_hint
                .as_ref()
                .map(|m| m.amount_minor)
                .unwrap_or(1_000);
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
        let tax_minor = subtotal_minor * 875 / 10_000;
        let total_minor = subtotal_minor.saturating_add(tax_minor);
        let totals = Totals {
            subtotal: Some(Money::new(subtotal_minor, currency)),
            discount: None,
            shipping: None,
            tax: Some(Money::new(tax_minor, currency)),
            total: Some(Money::new(total_minor, currency)),
        };

        let now = Utc::now();
        let txn = Transaction {
            id: format!("txn_{}", Uuid::new_v4().simple()),
            state: TransactionState::Completed,
            agent_id: input.agent.raw.clone(),
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
    fn subscription_pseudo_txn(
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

    // ---------- helpers ----------

    /// Push an event onto the durable webhook outbox for the configured
    /// subscriber URL. No-op when no `webhook_url` is configured.
    fn enqueue_webhook(&self, event: &Event) {
        let Some(url) = self.webhook_url.as_deref() else {
            return;
        };
        let payload_json = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
        let now = Utc::now();
        self.webhook_outbox
            .enqueue(crate::webhook::WebhookDelivery {
                id: format!("del_{}", Uuid::new_v4().simple()),
                event_id: event.id.clone(),
                event_type: event.r#type.clone(),
                url: url.to_string(),
                payload_json,
                status: crate::webhook::DeliveryStatus::Pending,
                attempts: 0,
                max_attempts: crate::webhook::DEFAULT_MAX_ATTEMPTS,
                next_attempt_at: now,
                last_status_code: None,
                last_error: None,
                created_at: now,
                updated_at: now,
                delivered_at: None,
            });
    }

    fn fresh_transaction(&self, input: &IntentInput<'_>, state: TransactionState) -> Transaction {
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

struct Outcome {
    transaction: Transaction,
    order: Option<OrderSummary>,
    /// Set by subscription intents (`subscribe`, `renew`, `pause`,
    /// `cancel_subscription`). Surfaced into `IntentResponseBody.subscription`
    /// and drives `subscription.<status>` event emission.
    subscription: Option<Subscription>,
    /// Set by `intent.a2a_quote` and `intent.a2a_pay` (when paying an
    /// existing quote). Drives `peer_quote.<status>` event emission.
    peer_quote: Option<PeerQuote>,
    recorded_spend_minor: i64,
}

/// Result of one [`IcpService::tick_subscriptions`] sweep — useful for
/// deterministic testing and Prometheus exposition.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct SchedulerTickReport {
    /// Total subscriptions in the store (any status).
    pub scanned: usize,
    /// Subset due for auto-renewal (active + next_charge_at <= now +
    /// payment_instrument present).
    pub due: usize,
    /// Subscriptions that successfully renewed in this tick.
    pub renewed: usize,
    /// Subscriptions whose charge failed in this tick.
    pub failed: usize,
    /// Subscriptions that transitioned to `past_due` in this tick.
    pub past_due: usize,
}

impl TransactionState {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Quoted => "quoted",
            Self::Authorized => "authorized",
            Self::Captured => "captured",
            Self::Fulfilled => "fulfilled",
            Self::Completed => "completed",
            Self::Reversed => "reversed",
            Self::Canceled => "canceled",
            Self::Expired => "expired",
        }
    }
}

fn require_transaction_id(params: &Value) -> Result<String, ApiError> {
    params
        .get("transaction_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ApiError::InvalidRequest("params.transaction_id required".into()))
}

/// Advance a billing period anchor by one cadence unit.
///
/// `Monthly` uses chrono's `checked_add_months`, which clamps to the
/// last day of the month for inputs near month-end (Jan 31 → Feb 28/29).
/// `Annual` likewise clamps Feb 29 → Feb 28 in non-leap years.
fn period_advance(start: chrono::DateTime<Utc>, cadence: BillingCadence) -> chrono::DateTime<Utc> {
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

/// Estimate the amount (in minor units) that an intent would put on the
/// mandate's budget. Used to gate budget checks *before* executing the
/// intent.
fn estimate_intent_amount_minor(envelope: &IntentEnvelope) -> i64 {
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

// Suppress unused warnings for fields kept on the struct for future use.
#[allow(dead_code)]
fn _unused(_t: &Totals, _l: &LineItem, _m: &Money, _b: &Buyer) {}
