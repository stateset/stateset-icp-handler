//! Background delivery worker: drains the outbox, signs, posts, and
//! schedules retries with exponential backoff.

use std::net::IpAddr;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};

use super::outbox::WebhookOutbox;
use super::signing::sign;
use super::subscribers::SubscriberStore;
use super::types::{DeliveryStatus, PruneReport, TickReport, WebhookDelivery};
use super::url::{is_forbidden_ip, validate_destination_url};
use super::DEFAULT_TIMEOUT_SECS;

/// Exponential backoff: `attempts²` seconds, clamped to 1 hour.
/// Attempts is the count *after* the increment, so attempt #1 → 1s
/// (first retry happens 1s after first failure), #2 → 4s, #3 → 9s,
/// #4 → 16s, #5 → 25s.
pub fn backoff_for(attempts: u32) -> Duration {
    let secs = (attempts as i64).saturating_mul(attempts as i64);
    Duration::seconds(secs.min(3600))
}

#[derive(Clone)]
pub struct WebhookWorker {
    outbox: WebhookOutbox,
    global_secret: Option<String>,
    subscribers: Option<SubscriberStore>,
    client: reqwest::Client,
    timeout: StdDuration,
    allow_insecure_urls: bool,
    /// Retain `delivered` rows for this many days. `None` disables
    /// pruning of delivered rows entirely.
    retain_delivered: Option<Duration>,
    /// Retain `dead_lettered` rows for this many days. `None`
    /// disables pruning of dead-lettered rows entirely.
    retain_dead_lettered: Option<Duration>,
}

impl WebhookWorker {
    pub fn new(outbox: WebhookOutbox, secret: String) -> Self {
        Self::new_with_optional_secret(outbox, Some(secret))
    }

    pub fn new_with_optional_secret(outbox: WebhookOutbox, global_secret: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(DEFAULT_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build webhook reqwest client");
        Self {
            outbox,
            global_secret,
            subscribers: None,
            client,
            timeout: StdDuration::from_secs(DEFAULT_TIMEOUT_SECS),
            allow_insecure_urls: true,
            retain_delivered: None,
            retain_dead_lettered: None,
        }
    }

    pub fn with_subscribers(mut self, subscribers: SubscriberStore) -> Self {
        self.subscribers = Some(subscribers);
        self
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    pub fn with_allow_insecure_urls(mut self, allow_insecure_urls: bool) -> Self {
        self.allow_insecure_urls = allow_insecure_urls;
        self
    }

    /// Configure retention windows. A `0` value for either count
    /// disables pruning of that status — matches `Config::for_test()`
    /// defaults so existing tests don't suddenly start losing rows.
    pub fn with_retention(mut self, delivered_days: u32, dead_lettered_days: u32) -> Self {
        self.retain_delivered = if delivered_days > 0 {
            Some(Duration::days(delivered_days as i64))
        } else {
            None
        };
        self.retain_dead_lettered = if dead_lettered_days > 0 {
            Some(Duration::days(dead_lettered_days as i64))
        } else {
            None
        };
        self
    }

    /// Run the retention sweep with this worker's configured windows
    /// against the current time. Public so `run_loop` and tests can
    /// drive it deterministically.
    pub fn prune_now(&self, now: DateTime<Utc>) -> PruneReport {
        let delivered_cutoff = self.retain_delivered.map(|d| now - d);
        let dead_cutoff = self.retain_dead_lettered.map(|d| now - d);
        if delivered_cutoff.is_none() && dead_cutoff.is_none() {
            return PruneReport::default();
        }
        self.outbox.prune(delivered_cutoff, dead_cutoff)
    }

    /// Run one drain pass — process every due delivery synchronously.
    /// Returns the number of deliveries acted on (Delivered + Failed +
    /// DeadLettered transitions in this pass).
    pub async fn tick(&self, now: DateTime<Utc>) -> TickReport {
        let mut report = TickReport::default();
        let due = self.outbox.list_due(now, 50);
        report.due = due.len();
        for delivery in due {
            self.outbox.mark_in_flight(&delivery.id, now);
            let send_result = self.send_one(&delivery).await;
            let after = Utc::now();
            match send_result {
                Ok(status) if (200..300).contains(&status) => {
                    self.outbox.mark_delivered(&delivery.id, status, after);
                    report.delivered += 1;
                    crate::metrics::record_webhook_delivery("delivered");
                }
                Ok(status) => {
                    self.outbox.bump_failure(
                        &delivery.id,
                        Some(status),
                        Some(format!("HTTP {status}")),
                        after,
                    );
                    if let Some(d) = self.outbox.get(&delivery.id) {
                        if matches!(d.status, DeliveryStatus::DeadLettered) {
                            report.dead_lettered += 1;
                            crate::metrics::record_webhook_delivery("dead_lettered");
                        } else {
                            report.failed += 1;
                            crate::metrics::record_webhook_delivery("failed");
                        }
                    }
                }
                Err(e) => {
                    self.outbox
                        .bump_failure(&delivery.id, None, Some(e.to_string()), after);
                    if let Some(d) = self.outbox.get(&delivery.id) {
                        if matches!(d.status, DeliveryStatus::DeadLettered) {
                            report.dead_lettered += 1;
                            crate::metrics::record_webhook_delivery("dead_lettered");
                        } else {
                            report.failed += 1;
                            crate::metrics::record_webhook_delivery("failed");
                        }
                    }
                }
            }
        }
        report
    }

    async fn send_one(&self, delivery: &WebhookDelivery) -> anyhow::Result<u16> {
        validate_destination_url(&delivery.url, self.allow_insecure_urls)
            .map_err(|e| anyhow::anyhow!(e))?;
        if !self.allow_insecure_urls {
            self.ensure_public_resolution(&delivery.url).await?;
        }
        let now_unix = Utc::now().timestamp();
        let secret = self
            .secret_for_delivery(delivery)
            .ok_or_else(|| anyhow::anyhow!("no webhook signing secret for delivery"))?;
        let signature = sign(&secret, now_unix, delivery.payload_json.as_bytes());
        let resp = self
            .client
            .post(&delivery.url)
            .timeout(self.timeout)
            .header("Content-Type", "application/json")
            .header("ICP-Signature", signature)
            .header("ICP-Event-Type", &delivery.event_type)
            .header("ICP-Event-Id", &delivery.event_id)
            .header("ICP-Delivery-Id", &delivery.id)
            .header("ICP-Delivery-Attempt", (delivery.attempts + 1).to_string())
            .body(delivery.payload_json.clone())
            .send()
            .await?;
        Ok(resp.status().as_u16())
    }

    async fn ensure_public_resolution(&self, url: &str) -> anyhow::Result<()> {
        let parsed = reqwest::Url::parse(url)?;
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("url must include a host"))?;
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_forbidden_ip(ip) {
                anyhow::bail!("url host must not resolve to localhost or a private network");
            }
            return Ok(());
        }
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| anyhow::anyhow!("url must include a resolvable port"))?;
        let mut addrs = tokio::net::lookup_host((host, port)).await?;
        let mut saw_addr = false;
        for addr in addrs.by_ref() {
            saw_addr = true;
            if is_forbidden_ip(addr.ip()) {
                anyhow::bail!("url host must not resolve to localhost or a private network");
            }
        }
        if !saw_addr {
            anyhow::bail!("url host did not resolve to any address");
        }
        Ok(())
    }

    fn secret_for_delivery(&self, delivery: &WebhookDelivery) -> Option<String> {
        if !delivery.tenant_id.is_empty() {
            if let Some(subscribers) = self.subscribers.as_ref() {
                if let Some(secret) = subscribers
                    .list_active_for_tenant(&delivery.tenant_id)
                    .into_iter()
                    .find(|s| s.url == delivery.url)
                    .and_then(|s| s.secret)
                {
                    return Some(secret);
                }
            }
        }
        self.global_secret.clone()
    }
}

/// Background loop that ticks the worker on `period` until aborted.
pub async fn run_loop(worker: WebhookWorker, period: StdDuration) {
    use tokio::time::{interval, MissedTickBehavior};
    tracing::info!(
        interval_secs = period.as_secs_f64(),
        "webhook worker started"
    );
    let mut tick = interval(period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let now = Utc::now();
        let report = worker.tick(now).await;
        // Run the retention sweep on every tick. When no retention
        // is configured (the test default) `prune_now` is a no-op.
        // The cost in production is two indexed DELETEs against
        // `webhook_deliveries.created_at`.
        let prune = worker.prune_now(now);
        crate::metrics::record_webhook_prune(&prune);
        // Always refresh the queue-depth gauge after a tick — even
        // ticks that processed zero rows can change the gauge if
        // intent-side enqueues landed between ticks. The
        // `record_webhook_tick` helper also bumps the worker
        // liveness counter.
        crate::metrics::record_webhook_tick(&worker.outbox.status_counts());
        if report.due > 0 || report.failed > 0 || report.dead_lettered > 0 {
            tracing::debug!(?report, "webhook worker tick");
        }
        if prune.delivered_pruned > 0 || prune.dead_lettered_pruned > 0 {
            tracing::debug!(?prune, "webhook outbox prune");
        }
    }
}
