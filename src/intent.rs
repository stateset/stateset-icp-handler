//! Intent definitions and scope mapping.
//!
//! Each intent is canonical (exact string), and each maps to a mandate
//! scope (or to the special `open` scope for intents that do not require
//! a mandate — currently just `intent.search`, `intent.describe`).

use serde::{Deserialize, Serialize};

use crate::errors::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Search,
    Describe,
    Quote,
    Negotiate,
    Authorize,
    Buy,
    Pay,
    Subscribe,
    Renew,
    Pause,
    CancelSubscription,
    Track,
    ConfirmReceipt,
    Return,
    RefundRequest,
    A2aPay,
    A2aQuote,
}

impl Intent {
    pub const CORE: &'static [Intent] = &[
        Intent::Search,
        Intent::Describe,
        Intent::Quote,
        Intent::Negotiate,
        Intent::Authorize,
        Intent::Buy,
        Intent::Pay,
        Intent::Subscribe,
        Intent::Renew,
        Intent::Pause,
        Intent::CancelSubscription,
        Intent::Track,
        Intent::ConfirmReceipt,
        Intent::Return,
        Intent::RefundRequest,
        Intent::A2aPay,
        Intent::A2aQuote,
    ];

    /// Parse the canonical wire name (e.g. `intent.buy` → `Intent::Buy`).
    pub fn parse(s: &str) -> Result<Self, ApiError> {
        let trimmed = s.trim();
        let name = trimmed
            .strip_prefix("intent.")
            .unwrap_or(trimmed);
        Ok(match name {
            "search" => Self::Search,
            "describe" => Self::Describe,
            "quote" => Self::Quote,
            "negotiate" => Self::Negotiate,
            "authorize" => Self::Authorize,
            "buy" => Self::Buy,
            "pay" => Self::Pay,
            "subscribe" => Self::Subscribe,
            "renew" => Self::Renew,
            "pause" => Self::Pause,
            "cancel_subscription" => Self::CancelSubscription,
            "track" => Self::Track,
            "confirm_receipt" => Self::ConfirmReceipt,
            "return" => Self::Return,
            "refund_request" => Self::RefundRequest,
            "a2a_pay" => Self::A2aPay,
            "a2a_quote" => Self::A2aQuote,
            other => {
                return Err(ApiError::IntentNotSupported(format!(
                    "unknown intent `intent.{other}`"
                )));
            }
        })
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Search => "intent.search",
            Self::Describe => "intent.describe",
            Self::Quote => "intent.quote",
            Self::Negotiate => "intent.negotiate",
            Self::Authorize => "intent.authorize",
            Self::Buy => "intent.buy",
            Self::Pay => "intent.pay",
            Self::Subscribe => "intent.subscribe",
            Self::Renew => "intent.renew",
            Self::Pause => "intent.pause",
            Self::CancelSubscription => "intent.cancel_subscription",
            Self::Track => "intent.track",
            Self::ConfirmReceipt => "intent.confirm_receipt",
            Self::Return => "intent.return",
            Self::RefundRequest => "intent.refund_request",
            Self::A2aPay => "intent.a2a_pay",
            Self::A2aQuote => "intent.a2a_quote",
        }
    }

    /// The mandate scope that gates this intent, or `None` for read-only
    /// intents that do not require a mandate.
    pub fn scope(self) -> Option<&'static str> {
        Some(match self {
            Self::Search | Self::Describe => return None,
            Self::Quote | Self::Negotiate => "quote",
            Self::Authorize | Self::Buy | Self::Pay => "buy",
            Self::Subscribe | Self::Renew | Self::Pause | Self::CancelSubscription => "subscribe",
            Self::Track | Self::ConfirmReceipt => "fulfill",
            Self::Return | Self::RefundRequest => "return",
            Self::A2aPay | Self::A2aQuote => "pay_peer",
        })
    }

    /// Whether this intent creates or transitions a transaction (and
    /// therefore must be signed into a receipt).
    pub fn is_state_change(self) -> bool {
        !matches!(self, Self::Search | Self::Describe | Self::Track)
    }
}
