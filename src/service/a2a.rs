//! Agent-to-agent commerce intents: `a2a_quote`, `a2a_pay`.
//!
//! Two flow shapes are supported:
//!   - **a2a_quote** opens a `PeerQuote` between two agents — optionally
//!     priced via `price_hint`, otherwise pending until the peer sets it.
//!   - **a2a_pay** charges either an existing `peer_quote_id` (linking
//!     the resulting transaction back to the quote) or a direct
//!     `peer_agent_id` + `amount` pair.

use chrono::Utc;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::mandate::MandateEvaluation;
use crate::models::{
    A2aPayParams, A2aQuoteParams, LineItem, PeerQuote, PeerQuoteStatus, Totals, Transaction,
    TransactionState,
};

use super::{IcpService, IntentInput, Outcome};

impl IcpService {
    pub(super) async fn do_a2a_quote(
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
            Some(p) => {
                if p.amount_minor <= 0 {
                    return Err(ApiError::InvalidRequest(
                        "a2a_quote.price_hint.amount_minor must be positive".into(),
                    ));
                }
                (PeerQuoteStatus::Quoted, Some(p))
            }
            None => (PeerQuoteStatus::Pending, None),
        };

        let quote = PeerQuote {
            id: format!("pq_{}", Uuid::new_v4().simple()),
            status,
            tenant_id: input.tenant.tenant_id.clone(),
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
            tenant_id: input.tenant.tenant_id.clone(),
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
        })
    }

    pub(super) async fn do_a2a_pay(
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
        if self.config.is_production() {
            return Err(ApiError::PreconditionFailed(
                "a2a_pay requires a production settlement adapter before it can complete".into(),
            ));
        }
        let _quote_guard = match params.peer_quote_id.as_deref() {
            Some(quote_id) => Some(self.lock_peer_quote(quote_id).await),
            None => None,
        };

        let now = Utc::now();

        // Two flow shapes: pay-against-quote and direct-pay.
        let (peer_id, money, quote_to_consume) = match params.peer_quote_id.as_deref() {
            Some(quote_id) => {
                let q = self.peer_quote_for_input(quote_id, input)?;
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
                if money.amount_minor <= 0 {
                    return Err(ApiError::PreconditionFailed(
                        "a2a_pay: peer_quote price must be positive".into(),
                    ));
                }
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
                if money.amount_minor <= 0 {
                    return Err(ApiError::InvalidRequest(
                        "a2a_pay.amount.amount_minor must be positive".into(),
                    ));
                }
                if peer_id == input.agent.raw {
                    return Err(ApiError::InvalidRequest(
                        "a2a_pay: cannot pay yourself".into(),
                    ));
                }
                (peer_id, money, None)
            }
        };
        self.reserve_mandate_spend(
            mandate,
            &input.tenant.tenant_id,
            money.amount_minor,
            &money.currency,
        )?;

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
            tenant_id: input.tenant.tenant_id.clone(),
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

        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: updated_quote,
        })
    }
}
