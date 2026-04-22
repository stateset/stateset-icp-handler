//! Background subscription billing scheduler.
//!
//! Calls [`IcpService::tick_subscriptions`] on a tokio interval. Every
//! tick scans the subscription store for due renewals and runs
//! charges using each subscription's stored payment instrument.
//!
//! The scheduler is opt-out via `ICP_SUBSCRIPTION_SCHEDULER_ENABLED=false`.
//! Tests typically disable it and call `tick_subscriptions(now)`
//! directly so behavior is deterministic without timing dependencies.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info};

use crate::service::IcpService;

/// Run a forever-loop that ticks the subscription scheduler at
/// `interval`. Designed to be `tokio::spawn`ed.
pub async fn run_loop(service: Arc<IcpService>, period: Duration) {
    info!(
        interval_secs = period.as_secs_f64(),
        "subscription scheduler started"
    );

    // Use `Delay` behavior so a slow tick doesn't burst — a missed tick
    // is just postponed by one period instead of running back-to-back.
    let mut tick = interval(period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        let now = Utc::now();
        let report = service.tick_subscriptions(now).await;
        // Refresh the per-status headcount gauge after every tick so
        // scrapes reflect post-tick state (and pick up
        // `subscribe`/`pause`/`cancel` from the request path between
        // ticks). Also bumps the scheduler liveness counter.
        crate::metrics::record_subscription_scheduler_tick(&service.subscriptions.status_counts());
        if report.due > 0 || report.failed > 0 {
            debug!(?report, "subscription scheduler tick");
        }
    }
}

/// Background expiry sweeper. Calls
/// [`IcpService::tick_expiries`] on a tokio interval — transitions
/// `quote_expires_at`-past transactions and `expires_at`-past peer
/// quotes to their terminal `Expired` state. Without this a stale
/// quote sits in `Quoted` forever and could be authorized at the
/// original price. Disable by setting
/// `ICP_EXPIRY_SWEEPER_INTERVAL_SECONDS=0`.
pub async fn run_expiry_loop(service: Arc<IcpService>, period: Duration) {
    info!(
        interval_secs = period.as_secs_f64(),
        "expiry sweeper started"
    );
    let mut tick = interval(period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        let now = Utc::now();
        let report = service.tick_expiries(now).await;
        crate::metrics::record_expiry_tick(&report);
        if report.transactions_expired > 0 || report.peer_quotes_expired > 0 {
            debug!(?report, "expiry sweeper tick");
        }
    }
}
