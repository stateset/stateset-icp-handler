//! ICP data model (JSON wire types).
//!
//! The structs in this module are the authoritative wire format — they are
//! what gets canonicalized (JCS) and signed into receipts.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use utoipa::ToSchema;

// --------------------------------------------------------------------------
// Envelope
// --------------------------------------------------------------------------

/// Metadata stamped onto every response (and echoed into receipts).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
pub struct Totals {
    pub subtotal: Option<Money>,
    pub discount: Option<Money>,
    pub shipping: Option<Money>,
    pub tax: Option<Money>,
    pub total: Option<Money>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Transaction {
    pub id: String,
    pub state: TransactionState,
    pub agent_id: String,
    /// Tenant that owns this transaction — derived from the bearer
    /// key at creation time. The `GET /icp/v1/transactions/:id`
    /// endpoint rejects cross-tenant reads (404 to avoid leaking
    /// existence). Defaulted for backwards compat with rows written
    /// before the field existed.
    #[serde(default)]
    pub tenant_id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IntentResponseBody {
    pub intent: String,
    pub intent_id: String,
    pub transaction: Transaction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<OrderSummary>,
    /// Present on `intent.subscribe`, `intent.renew`, `intent.pause`,
    /// and `intent.cancel_subscription`. Absent for non-subscription
    /// intents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<Subscription>,
    /// Present on `intent.a2a_quote`, and on `intent.a2a_pay` when the
    /// payment was made against an existing peer quote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_quote: Option<PeerQuote>,
    pub receipt: ReceiptStub,
    pub envelope: ResponseEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrderSummary {
    pub id: String,
    pub order_number: String,
    pub status: String,
    pub permalink_url: Option<String>,
    pub total: Money,
}

/// Inline stub of the receipt for convenience. The full signed JWS is also
/// returned in the `ICP-Receipt` response header.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReceiptStub {
    pub jti: String,
    pub kid: String,
    pub jws: String,
    pub body_digest: String,
}

// --------------------------------------------------------------------------
// Intent parameter types (one struct per core intent)
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SearchParams {
    pub query: Option<String>,
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DescribeParams {
    pub product_id: Option<String>,
    pub sku: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QuoteParams {
    pub items: Vec<RequestItem>,
    #[serde(default)]
    pub buyer: Option<Buyer>,
    #[serde(default)]
    pub ship_to: Option<Address>,
    #[serde(default)]
    pub discount_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestItem {
    pub sku: String,
    pub quantity: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_price_hint: Option<Money>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuthorizeParams {
    pub transaction_id: String,
    #[serde(default)]
    pub buyer: Option<Buyer>,
    #[serde(default)]
    pub ship_to: Option<Address>,
    #[serde(default)]
    pub bill_to: Option<Address>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BuyParams {
    pub transaction_id: String,
    pub payment: PaymentInstrument,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum PaymentInstrument {
    #[serde(rename = "card")]
    Card {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        last_digits: Option<String>,
        #[serde(default)]
        brand: Option<String>,
    },
    #[serde(rename = "delegated_vault")]
    DelegatedVault {
        /// An ACP / AP2 / merchant delegated-payment vault token.
        token: String,
        #[serde(default)]
        provider: Option<String>,
    },
    #[serde(rename = "stablecoin")]
    Stablecoin {
        asset: String, // USDC, ssUSD
        chain: String, // base, set, solana
        from: String,  // wallet address
        #[serde(default)]
        network_memo: Option<String>,
    },
    /// Peer-agent payment. Spec wire name is `"a2a"` — explicitly
    /// renamed because serde's `snake_case` rule converts `A2A` to
    /// `"a2_a"` on the digit boundary.
    #[serde(rename = "a2a")]
    A2A {
        peer_agent_id: String,
        #[serde(default)]
        memo: Option<String>,
    },
    #[serde(rename = "external_authorization")]
    ExternalAuthorization {
        /// Payment provider or rail that already authorized/captured
        /// the funds outside this handler.
        provider: String,
        /// Provider authorization/capture id. Required in production
        /// `external_required` mode.
        authorization_id: String,
        #[serde(default)]
        instrument_hint: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TrackParams {
    pub transaction_id: Option<String>,
    pub order_id: Option<String>,
}

// --------------------------------------------------------------------------
// Subscriptions
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Paused,
    Canceled,
    PastDue,
}

impl SubscriptionStatus {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Canceled => "canceled",
            Self::PastDue => "past_due",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Canceled)
    }
}

/// Billing frequency for a subscription. Period boundaries are computed
/// from `current_period_start` per the cadence:
///   * `Weekly` → +7 days
///   * `Monthly` → +1 calendar month (clamped to month-end)
///   * `Annual` → +1 year
///
/// Sub-day cadences are intentionally out of scope for v0.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingCadence {
    Weekly,
    Monthly,
    Annual,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Subscription {
    pub id: String,
    pub status: SubscriptionStatus,
    pub agent_id: String,
    /// Tenant that originally created the subscription. Stored on the
    /// row so scheduler-driven events (`subscription.renewed`,
    /// `subscription.past_due`) can fan out to that tenant's
    /// per-tenant webhook subscribers — no IntentInput available at
    /// scheduler time. Defaulted for backwards compat with rows
    /// written before the field existed.
    #[serde(default)]
    pub tenant_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mandate_jti: Option<String>,
    pub buyer: Buyer,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ship_to: Option<Address>,
    /// Recurring basket — the items that get charged each cycle.
    pub items: Vec<RequestItem>,
    pub currency: String,
    pub cadence: BillingCadence,
    pub current_period_start: DateTime<Utc>,
    pub current_period_end: DateTime<Utc>,
    pub next_charge_at: DateTime<Utc>,
    /// Number of successful charges to date (`subscribe` counts as 1).
    pub charges_completed: u32,
    /// Most recent charge transaction id (set by `subscribe` and each
    /// successful `renew`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transaction_id: Option<String>,
    /// Payment instrument used for *scheduler-driven* auto-renewals.
    /// Set on `intent.subscribe` from the caller's payment params; can
    /// be rotated by passing a fresh payment to `intent.renew`.
    /// Skipped from JSON unless present so we never round-trip it as
    /// `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_instrument: Option<PaymentInstrument>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canceled_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<DateTime<Utc>>,
    /// Number of consecutive scheduler-driven charge failures. Reset on
    /// any successful renewal. Used to drive the `past_due` transition.
    #[serde(default)]
    pub failed_renewal_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubscribeParams {
    pub items: Vec<RequestItem>,
    #[serde(default)]
    pub buyer: Option<Buyer>,
    #[serde(default)]
    pub ship_to: Option<Address>,
    pub cadence: BillingCadence,
    pub payment: PaymentInstrument,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RenewParams {
    pub subscription_id: String,
    pub payment: PaymentInstrument,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubscriptionRefParams {
    pub subscription_id: String,
}

// --------------------------------------------------------------------------
// A2A (peer commerce)
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerQuoteStatus {
    /// Quote requested; peer hasn't priced it yet (no `price_hint`).
    Pending,
    /// Quote priced and ready to be paid.
    Quoted,
    /// Buyer paid the quote — `charge_transaction_id` is set.
    Accepted,
    /// `expires_at` passed without acceptance.
    Expired,
    /// Either party rejected the quote.
    Rejected,
}

impl PeerQuoteStatus {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Quoted => "quoted",
            Self::Accepted => "accepted",
            Self::Expired => "expired",
            Self::Rejected => "rejected",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Accepted | Self::Expired | Self::Rejected)
    }
}

/// Coarse classification of the work being quoted between agents. The
/// `params` field carries kind-specific structured data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum A2aServiceKind {
    Compute,
    DataFeed,
    ImageGeneration,
    AdHoc,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct A2aServiceSpec {
    pub kind: A2aServiceKind,
    pub description: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A request → quote → payment record between two agents.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PeerQuote {
    pub id: String,
    pub status: PeerQuoteStatus,
    /// Tenant that owns this peer quote — derived from the
    /// requester's bearer key. `GET /icp/v1/peer_quotes/:id` rejects
    /// cross-tenant reads (404 to avoid leaking existence).
    /// Defaulted for backwards compat with rows written before the
    /// field existed.
    #[serde(default)]
    pub tenant_id: String,
    /// The agent that asked for the quote (who pays on acceptance).
    pub requester_agent_id: String,
    /// The agent that will perform the work (who gets paid).
    pub peer_agent_id: String,
    pub service: A2aServiceSpec,
    /// Set once the peer (or the requester via `price_hint`) prices
    /// the work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Money>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<DateTime<Utc>>,
    /// The charge transaction created when the requester paid this
    /// quote. Set by `intent.a2a_pay` when `peer_quote_id` is supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charge_transaction_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mandate_jti: Option<String>,
    /// Free-form caller-supplied id for cross-system correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct A2aQuoteParams {
    pub peer_agent_id: String,
    pub service: A2aServiceSpec,
    /// Optional price the requester is willing to pay. When present, the
    /// quote ships in `quoted` status and is immediately payable.
    #[serde(default)]
    pub price_hint: Option<Money>,
    /// Quote validity window. Defaults to 300 seconds (5 min) when omitted.
    #[serde(default)]
    pub expires_in_seconds: Option<u64>,
    #[serde(default)]
    pub reference_id: Option<String>,
}

/// Params for `intent.negotiate` — counter-offer the totals on an
/// existing quoted transaction. Either `proposed_total` (whole-basket
/// override) or `discount_pct` (percentage off the quoted total) is
/// required; if both are supplied, `proposed_total` wins.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NegotiateParams {
    pub transaction_id: String,
    #[serde(default)]
    pub proposed_total: Option<Money>,
    /// Percentage discount the buyer is asking for (e.g. `10.0` = 10%).
    /// Bounded `[0.0, 90.0]` — anything else is rejected with
    /// `invalid_request`.
    #[serde(default)]
    pub discount_pct: Option<f64>,
    /// Free-form rationale the buyer wants on the audit trail.
    #[serde(default)]
    pub message: Option<String>,
}

/// Params for `intent.confirm_receipt` — buyer acknowledges physical
/// receipt of goods. In production this is the trigger for escrow
/// release on A2A and stablecoin flows.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ConfirmReceiptParams {
    pub transaction_id: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct A2aPayParams {
    /// Reference an existing peer quote — the quote's price + peer
    /// become the charge details, and the quote is marked accepted.
    #[serde(default)]
    pub peer_quote_id: Option<String>,
    /// Direct-payment shape (used when `peer_quote_id` is not supplied).
    #[serde(default)]
    pub peer_agent_id: Option<String>,
    #[serde(default)]
    pub amount: Option<Money>,
    /// Wallet/account paying — required for both flows.
    pub from: String,
    /// Settlement instrument. Production `external_required` mode
    /// requires `method=external_authorization`, matching `intent.buy`
    /// and subscription charges.
    #[serde(default)]
    pub payment: Option<PaymentInstrument>,
    #[serde(default)]
    pub memo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
