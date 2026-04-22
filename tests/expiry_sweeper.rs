//! Quote-expiry sweeper.
//!
//! `transaction.quote_expires_at` and `peer_quote.expires_at` are
//! stored at quote creation but nothing previously enforced them — a
//! stale quote could be authorized at the original price hours/days
//! later. The sweeper transitions both kinds to the terminal
//! `Expired` state once their deadline passes, emits the matching
//! `<entity>.expired` event, and bumps the
//! `icp_expiries_total{kind}` counter.
//!
//! Asserts:
//!   * Transactions with `quote_expires_at <= now` AND state ∈
//!     {Draft, Quoted} transition to Expired. Authorized / Captured /
//!     Completed / already-Expired transactions are NEVER touched —
//!     the caller has moved past the quote-validity window.
//!   * Peer quotes with `expires_at <= now` AND status ∈ {Pending,
//!     Quoted} transition to Expired. Accepted / Rejected / already-
//!     Expired quotes are skipped.
//!   * The sweep emits `transaction.expired` / `peer_quote.expired`
//!     events to the originating tenant's webhook outbox.
//!   * Cross-tenant: tenant A's expiring quotes don't leak into
//!     tenant B's outbox.
//!   * The metrics path is wired (record_expiry_tick bumps the
//!     liveness counter and per-kind series).

use chrono::{Duration, Utc};
use serde_json::Value;
use stateset_icp_handler::{
    agent::ApiKeyInfo,
    build_app_state, build_router,
    config::Config,
    metrics::{EXPIRIES, EXPIRY_SWEEPER_TICKS},
    models::{Buyer, PeerQuote, PeerQuoteStatus, Totals, Transaction, TransactionState},
    AppState,
};

const AGENT: &str = "did:stateset:agent:expiry";

async fn build(keys: Vec<ApiKeyInfo>) -> AppState {
    let mut cfg = Config::for_test();
    cfg.enable_demo_keys = false;
    cfg.webhook_url = Some("https://hooks.example/global".into());
    cfg.webhook_secret = Some("global".into());
    cfg.api_keys_json = Some(serde_json::to_string(&keys).unwrap());
    let state = build_app_state(&cfg).await.expect("state");
    // Build the router so SSE subscribers exist (not strictly needed
    // for these tests but mirrors the production wiring).
    let _ = build_router(state.clone());
    state
}

fn key(name: &str, tenant: &str) -> ApiKeyInfo {
    ApiKeyInfo {
        key: format!("k_{name}"),
        tenant_id: tenant.to_string(),
        name: name.to_string(),
        rate_limit_per_minute: None,
        allowed_agents: None,
        expires_at: None,
    }
}

fn txn(
    id: &str,
    tenant: &str,
    state: TransactionState,
    quote_expires_at: Option<chrono::DateTime<Utc>>,
) -> Transaction {
    let now = Utc::now();
    Transaction {
        id: id.to_string(),
        state,
        agent_id: AGENT.to_string(),
        tenant_id: tenant.to_string(),
        mandate_jti: None,
        currency: "USD".into(),
        jurisdiction: None,
        buyer: Buyer::default(),
        ship_to: None,
        bill_to: None,
        line_items: Vec::new(),
        totals: Totals::default(),
        order_id: None,
        quote_expires_at,
        created_at: now,
        updated_at: now,
        external_refs: Default::default(),
    }
}

fn pq(
    id: &str,
    tenant: &str,
    status: PeerQuoteStatus,
    expires_at: chrono::DateTime<Utc>,
) -> PeerQuote {
    use stateset_icp_handler::models::A2aServiceSpec;
    let now = Utc::now();
    PeerQuote {
        id: id.to_string(),
        status,
        tenant_id: tenant.to_string(),
        requester_agent_id: AGENT.to_string(),
        peer_agent_id: "did:stateset:agent:peer".to_string(),
        service: A2aServiceSpec {
            kind: stateset_icp_handler::models::A2aServiceKind::ImageGeneration,
            description: "test".into(),
            params: serde_json::json!({}),
        },
        price: None,
        created_at: now,
        updated_at: now,
        expires_at,
        accepted_at: None,
        charge_transaction_id: None,
        mandate_jti: None,
        reference_id: None,
    }
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn expired_pre_auth_transactions_transition_to_expired() {
    let state = build(vec![key("a", "tenant_a")]).await;
    let now = Utc::now();

    // Stale quoted txn — should expire.
    state.service.transactions.insert(txn(
        "stale_quoted",
        "tenant_a",
        TransactionState::Quoted,
        Some(now - Duration::minutes(5)),
    ));
    // Stale draft txn — should also expire.
    state.service.transactions.insert(txn(
        "stale_draft",
        "tenant_a",
        TransactionState::Draft,
        Some(now - Duration::minutes(5)),
    ));
    // Fresh quoted txn — within validity, must stay.
    state.service.transactions.insert(txn(
        "fresh_quoted",
        "tenant_a",
        TransactionState::Quoted,
        Some(now + Duration::minutes(10)),
    ));
    // Authorized txn past expiry — caller already moved past the
    // quote-validity window. MUST NOT be touched.
    state.service.transactions.insert(txn(
        "stale_authorized",
        "tenant_a",
        TransactionState::Authorized,
        Some(now - Duration::hours(1)),
    ));
    // Already-completed txn — terminal, NOT touched.
    state.service.transactions.insert(txn(
        "stale_completed",
        "tenant_a",
        TransactionState::Completed,
        Some(now - Duration::hours(1)),
    ));
    // No quote_expires_at at all — never expires.
    state
        .service
        .transactions
        .insert(txn("no_expiry", "tenant_a", TransactionState::Quoted, None));

    let report = state.service.tick_expiries(now).await;
    assert_eq!(report.transactions_expired, 2);
    assert_eq!(report.peer_quotes_expired, 0);

    let post = |id: &str| state.service.transactions.get(id).unwrap().state;
    assert_eq!(post("stale_quoted"), TransactionState::Expired);
    assert_eq!(post("stale_draft"), TransactionState::Expired);
    assert_eq!(
        post("fresh_quoted"),
        TransactionState::Quoted,
        "in-window quotes must NOT be touched"
    );
    assert_eq!(
        post("stale_authorized"),
        TransactionState::Authorized,
        "past pre-auth phase — sweeper must NOT touch"
    );
    assert_eq!(post("stale_completed"), TransactionState::Completed);
    assert_eq!(post("no_expiry"), TransactionState::Quoted);
}

#[tokio::test]
async fn expired_pre_auth_peer_quotes_transition_to_expired() {
    let state = build(vec![key("a", "tenant_a")]).await;
    let now = Utc::now();

    state.service.peer_quotes.insert(pq(
        "stale_pending",
        "tenant_a",
        PeerQuoteStatus::Pending,
        now - Duration::seconds(1),
    ));
    state.service.peer_quotes.insert(pq(
        "stale_quoted",
        "tenant_a",
        PeerQuoteStatus::Quoted,
        now - Duration::seconds(1),
    ));
    state.service.peer_quotes.insert(pq(
        "fresh_quoted",
        "tenant_a",
        PeerQuoteStatus::Quoted,
        now + Duration::minutes(10),
    ));
    // Accepted past expiry — terminal, NOT touched.
    state.service.peer_quotes.insert(pq(
        "stale_accepted",
        "tenant_a",
        PeerQuoteStatus::Accepted,
        now - Duration::hours(1),
    ));

    let report = state.service.tick_expiries(now).await;
    assert_eq!(report.peer_quotes_expired, 2);

    let post = |id: &str| state.service.peer_quotes.get(id).unwrap().status;
    assert_eq!(post("stale_pending"), PeerQuoteStatus::Expired);
    assert_eq!(post("stale_quoted"), PeerQuoteStatus::Expired);
    assert_eq!(post("fresh_quoted"), PeerQuoteStatus::Quoted);
    assert_eq!(
        post("stale_accepted"),
        PeerQuoteStatus::Accepted,
        "terminal status must NOT be reverted"
    );
}

#[tokio::test]
async fn expiry_emits_per_tenant_webhook_events() {
    let state = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;
    let now = Utc::now();

    state.service.transactions.insert(txn(
        "txn_a",
        "tenant_a",
        TransactionState::Quoted,
        Some(now - Duration::minutes(1)),
    ));
    state.service.transactions.insert(txn(
        "txn_b",
        "tenant_b",
        TransactionState::Quoted,
        Some(now - Duration::minutes(1)),
    ));

    let _ = state.service.tick_expiries(now).await;

    // The global fallback is configured, so each expiry adds one
    // outbox row stamped with the originating tenant.
    let recent = state.service.webhook_outbox.list_recent(10);
    let a_count = recent
        .iter()
        .filter(|d| d.tenant_id == "tenant_a" && d.event_type == "transaction.expired")
        .count();
    let b_count = recent
        .iter()
        .filter(|d| d.tenant_id == "tenant_b" && d.event_type == "transaction.expired")
        .count();
    assert_eq!(a_count, 1, "tenant A's expiry → tenant A's outbox row");
    assert_eq!(b_count, 1, "tenant B's expiry → tenant B's outbox row");
}

#[tokio::test]
async fn second_sweep_after_already_expired_is_a_noop() {
    // Idempotency property — the sweeper running twice in quick
    // succession (e.g. two ticks landed within a sub-second) must
    // not re-emit events or churn state.
    let state = build(vec![key("a", "tenant_a")]).await;
    let now = Utc::now();

    state.service.transactions.insert(txn(
        "stale",
        "tenant_a",
        TransactionState::Quoted,
        Some(now - Duration::minutes(5)),
    ));

    let first = state.service.tick_expiries(now).await;
    assert_eq!(first.transactions_expired, 1);

    let second = state.service.tick_expiries(now).await;
    assert_eq!(
        second.transactions_expired, 0,
        "already-Expired transactions must not be re-swept"
    );

    // Outbox rows: only the first sweep enqueued one.
    let outbox_count = state
        .service
        .webhook_outbox
        .list_recent(10)
        .iter()
        .filter(|d| d.event_type == "transaction.expired")
        .count();
    assert_eq!(outbox_count, 1, "no double-enqueue on the second sweep");
}

#[tokio::test]
async fn transaction_expired_event_payload_carries_re_quote_context() {
    // Operators receiving `transaction.expired` need enough context
    // to either auto-re-quote or surface to the customer without
    // re-fetching the transaction. Mirrors the dunning-event
    // enrichment story.
    use stateset_icp_handler::models::{Money, Totals};

    let state = build(vec![key("a", "tenant_a")]).await;
    let mut events = state.service.events.subscribe();
    let now = Utc::now();

    fn money(amount: i64) -> Money {
        Money {
            amount_minor: amount,
            amount_display: None,
            currency: "USD".into(),
        }
    }

    // Build a Quoted txn with realistic shape: priced totals and
    // buyer email. Line items are skipped — only `totals.total`
    // is read by the event payload.
    let mut t = txn(
        "stale",
        "tenant_a",
        TransactionState::Quoted,
        Some(now - Duration::minutes(5)),
    );
    t.buyer.email = Some("alice@example.com".into());
    t.totals = Totals {
        subtotal: Some(money(2500)),
        discount: None,
        shipping: None,
        tax: None,
        total: Some(money(2500)),
    };
    state.service.transactions.insert(t);

    while events.try_recv().is_ok() {} // drain setup

    let _ = state.service.tick_expiries(now).await;

    let mut payload: Option<Value> = None;
    while let Ok(ev) = events.try_recv() {
        if ev.r#type == "transaction.expired" {
            payload = Some(ev.payload);
            break;
        }
    }
    let payload = payload.expect("transaction.expired event must be emitted");

    // Pre-existing fields — back-compat.
    assert_eq!(payload["transaction_id"], "stale");
    assert!(payload["expired_at"].is_string());

    // New triage fields.
    assert_eq!(
        payload["previous_state"], "quoted",
        "operators handle Draft-vs-Quoted differently"
    );
    assert!(
        payload["quote_expires_at"].is_string(),
        "the original deadline lets operators scope log searches to the quote window"
    );
    assert_eq!(payload["buyer_email"], "alice@example.com");
    assert_eq!(payload["amount_minor"], 2500);
    assert_eq!(payload["currency"], "USD");
    assert_eq!(payload["next_action"], "re_quote_required");
}

#[tokio::test]
async fn peer_quote_expired_event_payload_carries_re_quote_context() {
    use stateset_icp_handler::models::Money;

    let state = build(vec![key("a", "tenant_a")]).await;
    let mut events = state.service.events.subscribe();
    let now = Utc::now();

    let mut q = pq(
        "stale_pq",
        "tenant_a",
        stateset_icp_handler::models::PeerQuoteStatus::Quoted,
        now - Duration::seconds(1),
    );
    q.price = Some(Money {
        amount_minor: 7500,
        amount_display: None,
        currency: "USD".into(),
    });
    state.service.peer_quotes.insert(q);

    while events.try_recv().is_ok() {} // drain setup

    let _ = state.service.tick_expiries(now).await;

    let mut payload: Option<Value> = None;
    while let Ok(ev) = events.try_recv() {
        if ev.r#type == "peer_quote.expired" {
            payload = Some(ev.payload);
            break;
        }
    }
    let payload = payload.expect("peer_quote.expired event must be emitted");

    // Pre-existing fields — back-compat.
    assert_eq!(payload["peer_quote_id"], "stale_pq");
    assert!(payload["expired_at"].is_string());

    // New triage fields.
    assert_eq!(
        payload["previous_status"], "quoted",
        "operators handle Pending-vs-Quoted differently"
    );
    assert_eq!(payload["requester_agent_id"], AGENT);
    assert_eq!(payload["peer_agent_id"], "did:stateset:agent:peer");
    assert_eq!(
        payload["service_kind"], "image_generation",
        "service_kind serializes via the existing snake_case enum"
    );
    assert_eq!(payload["price_amount_minor"], 7500);
    assert_eq!(payload["price_currency"], "USD");
    assert_eq!(payload["next_action"], "re_quote_required");
}

#[test]
fn expiry_metrics_path_wired() {
    use stateset_icp_handler::service::ExpiryTickReport;

    let before_ticks = EXPIRY_SWEEPER_TICKS.get();
    let before_txn = EXPIRIES.with_label_values(&["transaction"]).get();
    let before_pq = EXPIRIES.with_label_values(&["peer_quote"]).get();

    let report = ExpiryTickReport {
        transactions_expired: 3,
        peer_quotes_expired: 2,
    };
    stateset_icp_handler::metrics::record_expiry_tick(&report);

    assert!(EXPIRY_SWEEPER_TICKS.get() > before_ticks);
    assert!(EXPIRIES.with_label_values(&["transaction"]).get() >= before_txn + 3);
    assert!(EXPIRIES.with_label_values(&["peer_quote"]).get() >= before_pq + 2);

    // Zero-effect tick still bumps liveness.
    let liveness_before = EXPIRY_SWEEPER_TICKS.get();
    stateset_icp_handler::metrics::record_expiry_tick(&ExpiryTickReport::default());
    assert!(
        EXPIRY_SWEEPER_TICKS.get() > liveness_before,
        "no-op tick must still bump liveness"
    );
}
