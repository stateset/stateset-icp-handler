//! Outbound webhook delivery (durable outbox pattern).
//!
//! Subscribers register via the handler's startup config (`ICP_WEBHOOK_URL`
//! + `ICP_WEBHOOK_SECRET`) or per-tenant via the admin endpoints; every
//! state-changing intent enqueues a [`WebhookDelivery`] row that the
//! background worker ([`run_loop`]) drains. Deliveries are HMAC-SHA256
//! signed in the Stripe convention:
//!
//! ```text
//! ICP-Signature: t=<unix_seconds>,v1=<hex_hmac_sha256>
//! ```
//!
//! Where the HMAC payload is `<t>.<body_json>`. The leading timestamp
//! protects against replay; receivers SHOULD reject signatures whose
//! `t` is more than 5 minutes old.
//!
//! The outbox writes happen *synchronously* inside the intent pipeline
//! so an event is durably enqueued before the response is sent. If the
//! handler crashes between the intent succeeding and the worker
//! delivering, the next process to come up resumes from the same outbox
//! row — events are at-least-once.
//!
//! Retry policy: exponential backoff with `attempts²` seconds between
//! attempts, capped at one hour. After `max_attempts` failures the row
//! transitions to `dead_lettered` and stops being retried; operators
//! can manually re-enqueue via `POST /icp/v1/webhook_deliveries/:id/retry`.

pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_TICK_SECS: u64 = 5;

mod outbox;
mod signing;
mod subscribers;
mod types;
mod url;
mod worker;

pub use outbox::WebhookOutbox;
pub use signing::{sign, verify};
pub use subscribers::{SubscriberStore, WebhookSubscriber};
pub use types::{
    DeliveryStatus, PruneReport, RetryError, StatusCounts, TickReport, WebhookDelivery,
};
pub use url::validate_destination_url;
pub use worker::{backoff_for, run_loop, WebhookWorker};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn sign_then_verify_roundtrip() {
        let body = br#"{"hello":"world"}"#;
        let header = sign("supersecret", 1_745_259_600, body);
        assert!(header.starts_with("t=1745259600,v1="));
        assert!(verify("supersecret", &header, body));
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let header = sign("k", 1_000, b"original");
        assert!(!verify("k", &header, b"tampered"));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let header = sign("k1", 1_000, b"x");
        assert!(!verify("k2", &header, b"x"));
    }

    #[test]
    fn destination_validation_blocks_ssrf_targets_when_insecure_urls_are_disabled() {
        assert!(validate_destination_url("https://hooks.example/webhook", false).is_ok());
        assert!(validate_destination_url("http://hooks.example/webhook", false).is_err());
        assert!(validate_destination_url("https://127.0.0.1/webhook", false).is_err());
        assert!(validate_destination_url("https://10.0.0.8/webhook", false).is_err());
        assert!(validate_destination_url("https://localhost/webhook", false).is_err());
        assert!(validate_destination_url("http://localhost/webhook", true).is_ok());
        assert!(validate_destination_url("ftp://hooks.example/webhook", true).is_err());
        assert!(validate_destination_url("https://user:pass@hooks.example/webhook", true).is_err());
    }

    #[test]
    fn backoff_grows_quadratically_then_caps() {
        assert_eq!(backoff_for(1).num_seconds(), 1);
        assert_eq!(backoff_for(2).num_seconds(), 4);
        assert_eq!(backoff_for(3).num_seconds(), 9);
        assert_eq!(backoff_for(60).num_seconds(), 3600);
        assert_eq!(backoff_for(1000).num_seconds(), 3600); // capped
    }

    #[test]
    fn outbox_in_memory_basic_lifecycle() {
        let outbox = WebhookOutbox::in_memory();
        let now = Utc::now();
        let d = WebhookDelivery {
            id: "del_1".into(),
            event_id: "evt_1".into(),
            event_type: "transaction.completed".into(),
            url: "http://localhost:9999/hook".into(),
            payload_json: "{}".into(),
            status: DeliveryStatus::Pending,
            attempts: 0,
            max_attempts: 3,
            next_attempt_at: now,
            last_status_code: None,
            last_error: None,
            created_at: now,
            updated_at: now,
            delivered_at: None,
            tenant_id: String::new(),
        };
        outbox.enqueue(d);
        assert_eq!(outbox.len(), 1);

        let due = outbox.list_due(now, 10);
        assert_eq!(due.len(), 1);

        outbox.mark_in_flight("del_1", now);
        outbox.mark_delivered("del_1", 200, now);
        let stored = outbox.get("del_1").unwrap();
        assert_eq!(stored.status, DeliveryStatus::Delivered);
        assert_eq!(stored.last_status_code, Some(200));
        assert_eq!(outbox.list_due(now, 10).len(), 0, "delivered → not due");
    }

    #[test]
    fn outbox_failure_then_dead_letter() {
        let outbox = WebhookOutbox::in_memory();
        let now = Utc::now();
        outbox.enqueue(WebhookDelivery {
            id: "del_2".into(),
            event_id: "e".into(),
            event_type: "t".into(),
            url: "u".into(),
            payload_json: "{}".into(),
            status: DeliveryStatus::Pending,
            attempts: 0,
            max_attempts: 2,
            next_attempt_at: now,
            last_status_code: None,
            last_error: None,
            created_at: now,
            updated_at: now,
            delivered_at: None,
            tenant_id: String::new(),
        });
        outbox.bump_failure("del_2", Some(500), Some("server".into()), now);
        assert_eq!(outbox.get("del_2").unwrap().status, DeliveryStatus::Failed);
        outbox.bump_failure("del_2", Some(500), Some("server".into()), now);
        assert_eq!(
            outbox.get("del_2").unwrap().status,
            DeliveryStatus::DeadLettered,
            "second failure exhausts max_attempts=2",
        );
    }
}
