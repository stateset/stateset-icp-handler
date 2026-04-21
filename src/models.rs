//! ICP data model (JSON wire types).
//!
//! The structs in this module are the authoritative wire format — they are
//! what gets canonicalized (JCS) and signed into receipts.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

// --------------------------------------------------------------------------
// Envelope
// --------------------------------------------------------------------------

/// Metadata stamped onto every response (and echoed into receipts).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub icp_version: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub issued_at: DateTime<Utc>,
}

// --------------------------------------------------------------------------
// Money
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Money {
    /// Integer amount in the currency's minor unit (cents, satoshis, 6-dec
    /// base for USDC, etc.) per ICP §3.2.
    pub amount_minor: i64,
    /// Human-friendly decimal string (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_display: Option<String>,
    /// ISO 4217 for fiat; extended codes for stablecoin/crypto.
    pub currency: String,
}

impl Money {
    pub fn new(amount_minor: i64, currency: impl Into<String>) -> Self {
        Self {
            amount_minor,
            amount_display: None,
            currency: currency.into(),
        }
    }
}

// --------------------------------------------------------------------------
// Addresses
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Address {
    pub name: Option<String>,
    pub line_one: Option<String>,
    pub line_two: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub phone_number: Option<String>,
    pub email: Option<String>,
}

// --------------------------------------------------------------------------
// Buyer / principal
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Buyer {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone_number: Option<String>,
    /// Optional principal DID, when the buyer has a stable decentralized identity.
    pub principal_did: Option<String>,
}

// --------------------------------------------------------------------------
// Intent envelope (§7.1)
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentEnvelope {
    pub intent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mandate_jti: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub context: IntentContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IntentContext {
    pub locale: Option<String>,
    pub jurisdiction: Option<String>,
    pub currency: Option<String>,
    pub channel: Option<String>,
    pub session_hint: Option<String>,
}

// --------------------------------------------------------------------------
// Transactions
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    Draft,
    Quoted,
    Authorized,
    Captured,
    Fulfilled,
    Completed,
    Reversed,
    Canceled,
    Expired,
}

impl TransactionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Reversed | Self::Canceled | Self::Expired
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineItem {
    pub id: String,
    pub sku: String,
    pub name: String,
    pub quantity: i64,
    pub unit_price: Money,
    pub subtotal: Money,
    pub tax: Option<Money>,
    pub total: Money,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Totals {
    pub subtotal: Option<Money>,
    pub discount: Option<Money>,
    pub shipping: Option<Money>,
    pub tax: Option<Money>,
    pub total: Option<Money>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub state: TransactionState,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mandate_jti: Option<String>,
    pub currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jurisdiction: Option<String>,
    pub buyer: Buyer,
    pub ship_to: Option<Address>,
    pub bill_to: Option<Address>,
    pub line_items: Vec<LineItem>,
    pub totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Cross-reference map to external systems (e.g. ACP session id).
    #[serde(default)]
    pub external_refs: BTreeMap<String, String>,
}

// --------------------------------------------------------------------------
// Responses
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentResponseBody {
    pub intent: String,
    pub intent_id: String,
    pub transaction: Transaction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<OrderSummary>,
    pub receipt: ReceiptStub,
    pub envelope: ResponseEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSummary {
    pub id: String,
    pub order_number: String,
    pub status: String,
    pub permalink_url: Option<String>,
    pub total: Money,
}

/// Inline stub of the receipt for convenience. The full signed JWS is also
/// returned in the `ICP-Receipt` response header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptStub {
    pub jti: String,
    pub kid: String,
    pub jws: String,
    pub body_digest: String,
}

// --------------------------------------------------------------------------
// Intent parameter types (one struct per core intent)
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchParams {
    pub query: Option<String>,
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeParams {
    pub product_id: Option<String>,
    pub sku: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteParams {
    pub items: Vec<RequestItem>,
    #[serde(default)]
    pub buyer: Option<Buyer>,
    #[serde(default)]
    pub ship_to: Option<Address>,
    #[serde(default)]
    pub discount_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestItem {
    pub sku: String,
    pub quantity: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_price_hint: Option<Money>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizeParams {
    pub transaction_id: String,
    #[serde(default)]
    pub buyer: Option<Buyer>,
    #[serde(default)]
    pub ship_to: Option<Address>,
    #[serde(default)]
    pub bill_to: Option<Address>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuyParams {
    pub transaction_id: String,
    pub payment: PaymentInstrument,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum PaymentInstrument {
    Card {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        last_digits: Option<String>,
        #[serde(default)]
        brand: Option<String>,
    },
    DelegatedVault {
        /// An ACP / AP2 / merchant delegated-payment vault token.
        token: String,
        #[serde(default)]
        provider: Option<String>,
    },
    Stablecoin {
        asset: String, // USDC, ssUSD
        chain: String, // base, set, solana
        from: String,  // wallet address
        #[serde(default)]
        network_memo: Option<String>,
    },
    A2A {
        peer_agent_id: String,
        #[serde(default)]
        memo: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackParams {
    pub transaction_id: Option<String>,
    pub order_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReturnParams {
    pub order_id: String,
    pub line_item_ids: Vec<String>,
    pub reason: String,
}

// --------------------------------------------------------------------------
// Pricing helpers
// --------------------------------------------------------------------------

pub fn decimal_to_minor(amount: Decimal, currency: &str) -> i64 {
    let scale = minor_unit_scale(currency);
    (amount * Decimal::from(scale))
        .round()
        .try_into()
        .unwrap_or(0)
}

pub fn minor_unit_scale(currency: &str) -> i64 {
    match currency.to_ascii_uppercase().as_str() {
        "JPY" | "KRW" | "VND" => 1,
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 1_000,
        "BTC" => 100_000_000,
        "USDC" | "USDT" | "DAI" | "SSUSD" => 1_000_000,
        _ => 100,
    }
}
