//! Catalog/query intents: `search`, `describe`, `quote`.
//!
//! These are the read-shaped intents (search/describe) plus the pricing
//! intent (quote) that opens a buy flow but does not yet move money.

use chrono::Utc;

use crate::errors::ApiError;
use crate::mandate::MandateEvaluation;
use crate::models::{QuoteParams, TransactionState};

use super::helpers::price_request_items;
use super::{IcpService, IntentInput, Outcome};

impl IcpService {
    pub(super) async fn do_search(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        // v0.1: return an advisory transaction with zero line items. A
        // future release will surface the engine's product search.
        let txn = self.fresh_transaction(input, TransactionState::Draft);
        self.transactions.insert(txn.clone());
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
        })
    }

    pub(super) async fn do_describe(&self, input: &IntentInput<'_>) -> Result<Outcome, ApiError> {
        let txn = self.fresh_transaction(input, TransactionState::Draft);
        self.transactions.insert(txn.clone());
        Ok(Outcome {
            transaction: txn,
            order: None,
            subscription: None,
            peer_quote: None,
        })
    }

    pub(super) async fn do_quote(
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

        let (line_items, mut totals) =
            price_request_items(&params.items, &currency, "quote.params.items")?;

        // Apply discount codes through the engine's promotions engine when
        // the caller supplied any. Additive and safe-by-default: with no
        // codes, no engine, or no matching coupon (the un-seeded default),
        // the discount is 0 and totals are unchanged. The discount is baked
        // into the stored totals, so the later charge (which reads the
        // transaction's total) is consistent with the quote.
        if !params.discount_codes.is_empty() {
            if let Some(engine) = self.engine.as_ref() {
                let discount = engine.compute_discount_minor(
                    &params.items,
                    &currency,
                    params.ship_to.as_ref(),
                    &params.discount_codes,
                );
                if discount > 0 {
                    super::helpers::apply_discount_to_totals(&mut totals, discount, &currency);
                }
            }
        }

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
        })
    }
}
