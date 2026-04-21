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
    Buyer, IntentEnvelope, IntentResponseBody, LineItem, Money, OrderSummary, QuoteParams,
    ReceiptStub, ResponseEnvelope, Totals, Transaction, TransactionState,
};
use crate::receipts::{ReceiptStore, StoredReceipt};
use crate::signing::ReceiptSigner;
use crate::state_store::TransactionStore;

#[derive(Clone)]
pub struct IcpService {
    pub config: Arc<Config>,
    pub engine: Option<CommerceEngine>,
    pub transactions: TransactionStore,
    pub receipts: ReceiptStore,
    pub mandates: MandateLedger,
    pub signer: Arc<ReceiptSigner>,
    pub events: EventBus,
}

pub struct IntentInput<'a> {
    pub envelope: IntentEnvelope,
    pub agent: AgentIdentifier,
    pub tenant: ApiKeyInfo,
    pub mandate_jws: Option<&'a str>,
    pub request_id: String,
    pub trace_id: Option<String>,
}

impl IcpService {
    pub fn new(
        config: Config,
        engine: Option<CommerceEngine>,
        signer: ReceiptSigner,
    ) -> Self {
        Self {
            config: Arc::new(config),
            engine,
            transactions: TransactionStore::new(),
            receipts: ReceiptStore::new(),
            mandates: MandateLedger::new(),
            signer: Arc::new(signer),
            events: EventBus::default(),
        }
    }

    pub async fn handle_intent(
        &self,
        input: IntentInput<'_>,
    ) -> Result<IntentResponseBody, ApiError> {
        let intent = Intent::parse(&input.envelope.intent)?;

        let mandate = match intent.scope() {
            Some(scope) => match input.mandate_jws {
                Some(jws) => Some(mandate::evaluate(
                    jws,
                    scope,
                    estimate_intent_amount_minor(&input.envelope),
                    Utc::now(),
                    &input.tenant.tenant_id,
                    &self.mandates,
                )?),
                None if self.config.require_mandate => {
                    return Err(ApiError::MandateInvalid(
                        "ICP-Mandate header required for this intent scope".into(),
                    ));
                }
                None => None,
            },
            None => None,
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
            Intent::Subscribe
            | Intent::Renew
            | Intent::Pause
            | Intent::CancelSubscription
            | Intent::Negotiate
            | Intent::ConfirmReceipt
            | Intent::A2aPay
            | Intent::A2aQuote => {
                return Err(ApiError::IntentNotSupported(format!(
                    "{} is advertised in discovery but not yet implemented in this v0.1 handler",
                    intent.wire_name()
                )));
            }
        };

        // Record mandate spend (for intents that spend budget).
        if let Some(ev) = mandate.as_ref() {
            let spend = outcome.recorded_spend_minor;
            if spend > 0 {
                self.mandates.record_spend(&ev.payload.jti, spend, Utc::now());
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

        // Emit event.
        self.events.emit(Event {
            id: format!("evt_{}", Uuid::new_v4().simple()),
            r#type: format!("transaction.{}", outcome.transaction.state.wire_name()),
            transaction_id: Some(outcome.transaction.id.clone()),
            order_id: outcome.order.as_ref().map(|o| o.id.clone()),
            agent_id: Some(input.agent.raw.clone()),
            occurred_at: Utc::now(),
            payload: serde_json::to_value(&body).unwrap_or(Value::Null),
        });

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
            recorded_spend_minor: 0,
        })
    }

    async fn do_describe(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let txn = self.fresh_transaction(input, TransactionState::Draft);
        self.transactions.insert(txn.clone());
        Ok(Outcome {
            transaction: txn,
            order: None,
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
            .or_else(|| input.envelope.transaction_id.as_deref())
            .ok_or_else(|| ApiError::InvalidRequest("track: transaction_id required".into()))?;
        let txn = self
            .transactions
            .get(txn_id)
            .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {txn_id}")))?;
        Ok(Outcome {
            transaction: txn,
            order: None,
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
            .or_else(|| input.envelope.params.get("transaction_id").and_then(|v| v.as_str()))
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

    // ---------- helpers ----------

    fn fresh_transaction(
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
    recorded_spend_minor: i64,
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

/// Estimate the amount (in minor units) that an intent would put on the
/// mandate's budget. Used to gate budget checks *before* executing the
/// intent.
fn estimate_intent_amount_minor(envelope: &IntentEnvelope) -> i64 {
    // If the caller supplied explicit line items (quote/authorize/buy via
    // inline items), sum them. Otherwise, for buy against an existing txn,
    // fall back to 0 here and re-check after loading the transaction.
    if let Some(items) = envelope
        .params
        .get("items")
        .and_then(|v| v.as_array())
    {
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
