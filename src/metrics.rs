//! Prometheus metrics.

use lazy_static::lazy_static;
use prometheus::{
    register_histogram_vec, register_int_counter_vec, HistogramVec, IntCounterVec, TextEncoder,
};

lazy_static! {
    pub static ref HTTP_REQUESTS: IntCounterVec = register_int_counter_vec!(
        "icp_http_requests_total",
        "Total ICP HTTP requests by route and status.",
        &["route", "status"]
    )
    .expect("register icp_http_requests_total");
    pub static ref HTTP_LATENCY: HistogramVec = register_histogram_vec!(
        "icp_http_request_duration_seconds",
        "ICP HTTP request latency in seconds.",
        &["route"]
    )
    .expect("register icp_http_request_duration_seconds");
    pub static ref INTENTS_PROCESSED: IntCounterVec = register_int_counter_vec!(
        "icp_intents_total",
        "Intents processed, partitioned by intent and outcome.",
        &["intent", "outcome"]
    )
    .expect("register icp_intents_total");
}

pub fn encode() -> String {
    let encoder = TextEncoder::new();
    let metrics = prometheus::gather();
    encoder.encode_to_string(&metrics).unwrap_or_default()
}

pub fn record_http(route: &str, status: u16, elapsed_secs: f64) {
    HTTP_REQUESTS
        .with_label_values(&[route, &status.to_string()])
        .inc();
    HTTP_LATENCY.with_label_values(&[route]).observe(elapsed_secs);
}

pub fn record_intent(intent: &str, outcome: &str) {
    INTENTS_PROCESSED
        .with_label_values(&[intent, outcome])
        .inc();
}
