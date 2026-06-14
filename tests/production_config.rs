use serde_json::json;
use stateset_icp_handler::{
    agent::{AgentIdentifier, ApiKeyInfo},
    config::Config,
    errors::ApiError,
    models::IntentEnvelope,
    service::{IcpService, IntentInput},
    signing::ReceiptSigner,
};

const AGENT: &str = "did:stateset:agent:prod-safety";

fn production_ready_config() -> Config {
    let mut cfg = Config::for_test();
    cfg.deployment_env = "production".into();
    cfg.api_keys_json =
        Some(r#"[{"key":"prod_key","tenant_id":"merchant_prod","name":"Production key"}]"#.into());
    cfg.enable_demo_keys = false;
    cfg.signing_key_pem_env = Some("placeholder parsed after engine opens".into());
    cfg.allow_insecure_urls = false;
    cfg.public_base_url = "https://icp.example.com".into();
    cfg.cors_allow_origins = vec!["https://merchant.example.com".into()];
    cfg.require_mandate = true;
    cfg.verify_mandate_signatures = true;
    cfg.require_icp_version = true;
    cfg.require_request_id = true;
    cfg.require_idempotency_key = true;
    cfg.state_db_path = format!("/tmp/icp_prod_state_{}.db", uuid::Uuid::new_v4().simple());
    cfg.commerce_enabled = true;
    cfg.commerce_db_path = format!(
        "/tmp/icp_prod_commerce_{}.db",
        uuid::Uuid::new_v4().simple()
    );
    cfg.redis_url = Some("redis://127.0.0.1:6379".into());
    cfg.payment_execution_mode = "external_required".into();
    cfg
}

#[test]
fn production_profile_rejects_dev_defaults() {
    let mut cfg = Config::for_test();
    cfg.deployment_env = "production".into();
    let err = cfg.validate_runtime().unwrap_err().to_string();
    assert!(err.contains("ICP_SIGNING_KEY_PEM is required"));
    assert!(err.contains("ICP_ALLOW_INSECURE_URLS must be false"));
    assert!(err.contains("ICP_REQUIRE_REQUEST_ID must be true"));
    assert!(err.contains("ICP_PAYMENT_EXECUTION_MODE must be external_required"));
    assert!(err.contains("REDIS_URL is required"));
}

#[test]
fn production_requires_webhook_secret_when_webhook_url_set() {
    // A global webhook URL with no secret ships unsigned deliveries, which
    // receivers cannot authenticate — production must reject it.
    let mut cfg = production_ready_config();
    cfg.validate_runtime()
        .expect("baseline production config should be valid");

    cfg.webhook_url = Some("https://merchant.example.com/hook".into());
    cfg.webhook_secret = None;
    let err = cfg.validate_runtime().unwrap_err().to_string();
    assert!(
        err.contains("ICP_WEBHOOK_SECRET is required"),
        "unexpected error: {err}"
    );

    // Supplying the secret clears the issue.
    cfg.webhook_secret = Some("whsec_test".into());
    cfg.validate_runtime()
        .expect("config with webhook secret should be valid");
}

#[test]
fn invalid_payment_mode_is_rejected() {
    let mut cfg = Config::for_test();
    cfg.payment_execution_mode = "fake_mode".into();
    let err = cfg.validate_runtime().unwrap_err().to_string();
    assert!(err.contains("ICP_PAYMENT_EXECUTION_MODE"));
}

#[test]
fn disabling_insecure_urls_requires_https_urls() {
    let mut cfg = Config::for_test();
    cfg.allow_insecure_urls = false;
    cfg.public_base_url = "http://127.0.0.1:8082".into();
    let err = cfg.validate_runtime().unwrap_err().to_string();
    assert!(err.contains("ICP_PUBLIC_BASE_URL must use https://"));
}

#[tokio::test]
async fn production_startup_rejects_invalid_commerce_database_path() {
    let mut cfg = production_ready_config();
    cfg.commerce_db_path = "/tmp".into();

    let err = match stateset_icp_handler::build_app_state(&cfg).await {
        Ok(_) => panic!("production startup should fail for an invalid commerce database path"),
        Err(err) => err.to_string(),
    };
    assert!(
        err.contains("COMMERCE_DB_PATH must be a database file path"),
        "unexpected error: {err}"
    );
}

fn production_service_without_engine() -> IcpService {
    let mut cfg = Config::for_test();
    cfg.deployment_env = "production".into();
    cfg.payment_execution_mode = "external_required".into();
    cfg.require_mandate = false;
    IcpService::new(cfg, None, ReceiptSigner::generate("prod-safety-test"))
}

fn tenant() -> ApiKeyInfo {
    ApiKeyInfo {
        key: "prod_key".into(),
        tenant_id: "merchant_prod".into(),
        name: "Production tenant".into(),
        rate_limit_per_minute: None,
        allowed_agents: None,
        expires_at: None,
    }
}

fn input(envelope: IntentEnvelope) -> IntentInput<'static> {
    IntentInput::for_icp(
        envelope,
        AgentIdentifier::parse(AGENT),
        tenant(),
        None,
        "req_prod_safety".into(),
        None,
    )
}

#[tokio::test]
async fn production_subscribe_fails_closed_without_commerce_engine() {
    let service = production_service_without_engine();
    let envelope: IntentEnvelope = serde_json::from_value(json!({
        "intent": "intent.subscribe",
        "agent_id": AGENT,
        "params": {
            "items": [{
                "sku": "PLAN-PRO",
                "quantity": 1,
                "unit_price_hint": { "amount_minor": 4999, "currency": "USD" }
            }],
            "buyer": { "first_name": "Alice", "email": "alice@example.com" },
            "cadence": "monthly",
            "payment": {
                "method": "external_authorization",
                "provider": "stripe",
                "authorization_id": "auth_prod_123"
            }
        },
        "context": { "currency": "USD" }
    }))
    .unwrap();

    let err = service.handle_intent(input(envelope)).await.unwrap_err();
    assert!(
        matches!(err, ApiError::EngineUnavailable(_)),
        "production subscribe must not synthesize completed charges without the engine, got {err:?}"
    );
}

#[tokio::test]
async fn production_a2a_pay_requires_external_authorization() {
    let service = production_service_without_engine();
    let envelope: IntentEnvelope = serde_json::from_value(json!({
        "intent": "intent.a2a_pay",
        "agent_id": AGENT,
        "params": {
            "peer_agent_id": "did:stateset:agent:peer",
            "amount": { "amount_minor": 2500, "currency": "USD" },
            "from": "acct_prod_123"
        },
        "context": { "currency": "USD" }
    }))
    .unwrap();

    let err = service.handle_intent(input(envelope)).await.unwrap_err();
    assert!(
        matches!(err, ApiError::PreconditionFailed(_)),
        "production a2a_pay must require external authorization, got {err:?}"
    );
}

#[tokio::test]
async fn production_a2a_pay_accepts_external_authorization() {
    let service = production_service_without_engine();
    let envelope: IntentEnvelope = serde_json::from_value(json!({
        "intent": "intent.a2a_pay",
        "agent_id": AGENT,
        "params": {
            "peer_agent_id": "did:stateset:agent:peer",
            "amount": { "amount_minor": 2500, "currency": "USD" },
            "from": "acct_prod_123",
            "payment": {
                "method": "external_authorization",
                "provider": "stripe",
                "authorization_id": "auth_a2a_prod_123",
                "instrument_hint": "card_4242"
            }
        },
        "context": { "currency": "USD" }
    }))
    .unwrap();

    let body = service.handle_intent(input(envelope)).await.unwrap();
    assert_eq!(
        body.transaction.external_refs.get("settlement_provider"),
        Some(&"stripe".to_string())
    );
    assert_eq!(
        body.transaction
            .external_refs
            .get("settlement_authorization_id"),
        Some(&"auth_a2a_prod_123".to_string())
    );
    assert_eq!(
        body.transaction
            .external_refs
            .get("settlement_instrument_hint"),
        Some(&"card_4242".to_string())
    );
}
