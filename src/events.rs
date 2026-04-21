//! Event bus.
//!
//! Every state-changing intent emits an `Event`. Consumers subscribe via
//! tokio broadcast channel (used by SSE + gRPC streaming endpoints) and
//! optionally receive HMAC-signed outbound webhooks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub r#type: String,
    pub transaction_id: Option<String>,
    pub order_id: Option<String>,
    pub agent_id: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub payload: serde_json::Value,
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    pub fn emit(&self, event: Event) {
        // best-effort: dropped events are acceptable for an in-process bus
        let _ = self.sender.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}
