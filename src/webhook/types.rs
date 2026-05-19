//! Shared types: delivery status, errors, payloads, worker reports.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Enqueued, awaiting first attempt.
    Pending,
    /// Worker has picked it up; either succeeds or moves to Failed.
    InFlight,
    /// 2xx response received from the subscriber.
    Delivered,
    /// Last attempt failed; will retry per backoff schedule.
    Failed,
    /// Exhausted `max_attempts`; never retried automatically.
    DeadLettered,
}

impl DeliveryStatus {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InFlight => "in_flight",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::DeadLettered => "dead_lettered",
        }
    }

    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "in_flight" => Self::InFlight,
            "delivered" => Self::Delivered,
            "failed" => Self::Failed,
            "dead_lettered" => Self::DeadLettered,
            _ => Self::Pending,
        }
    }
}

/// Why a manual `reset_for_retry` call was refused. Each variant maps
/// 1:1 to an HTTP status the route handler should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryError {
    /// No row with this id exists → 404.
    NotFound,
    /// Already enqueued — retry would be a noisy no-op → 412.
    AlreadyPending,
    /// Worker has it in flight; retrying now would race the worker → 412.
    InFlight,
    /// Receiver already accepted; nothing to retry → 412.
    AlreadyDelivered,
}

impl RetryError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::NotFound => "webhook delivery not found",
            Self::AlreadyPending => "delivery is already pending; no retry needed",
            Self::InFlight => "delivery is in flight; retry after the current attempt completes",
            Self::AlreadyDelivered => "delivery already succeeded; nothing to retry",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WebhookDelivery {
    pub id: String,
    pub event_id: String,
    pub event_type: String,
    pub url: String,
    /// JSON body that will be POSTed verbatim. Already serialized so
    /// the signed bytes match exactly what's transmitted.
    pub payload_json: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub max_attempts: u32,
    pub next_attempt_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<DateTime<Utc>>,
    /// Originating tenant id. Empty string for pre-multi-tenant rows
    /// and for events the handler enqueues outside any tenant scope
    /// (currently none — every state-changing intent has a bearer key
    /// so a tenant is always present).
    #[serde(default)]
    pub tenant_id: String,
}

/// Result of one [`crate::webhook::WebhookOutbox::prune`] call. The two
/// counts feed the `icp_webhook_outbox_pruned_total{reason}` Prometheus
/// counter so operators can see retention pressure (a sustained nonzero
/// rate is a sign that the outbox would otherwise grow unbounded).
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct PruneReport {
    pub delivered_pruned: usize,
    pub dead_lettered_pruned: usize,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct TickReport {
    pub due: usize,
    pub delivered: usize,
    pub failed: usize,
    pub dead_lettered: usize,
}

/// Snapshot of webhook outbox depth by FSM state. Refreshed once per
/// worker tick and reflected onto the
/// `icp_webhook_outbox_queue_depth{status=...}` gauge — operators
/// alert on `pending > N` (backlog growing) and `dead_lettered > 0`
/// (a destination is broken and ops should manually retry).
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct StatusCounts {
    pub pending: usize,
    pub in_flight: usize,
    pub delivered: usize,
    pub failed: usize,
    pub dead_lettered: usize,
}
