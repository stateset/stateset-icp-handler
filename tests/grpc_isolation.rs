use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo,
    build_app_state,
    config::Config,
    grpc::{
        proto::{
            icp_handler_server::IcpHandler, Envelope, GetReceiptRequest, GetTransactionRequest,
            IntentRequest, VerifyReceiptRequest,
        },
        GrpcHandler,
    },
};
use tonic::Request;

const AGENT: &str = "did:stateset:agent:grpc";

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

async fn handler() -> GrpcHandler {
    let mut cfg = Config::for_test();
    handler_with_config(&mut cfg).await
}

async fn handler_with_config(cfg: &mut Config) -> GrpcHandler {
    cfg.enable_demo_keys = false;
    cfg.api_keys_json =
        Some(serde_json::to_string(&vec![key("a", "tenant_a"), key("b", "tenant_b")]).unwrap());
    let state = build_app_state(cfg).await.expect("state");
    GrpcHandler {
        service: state.service,
        keys: state.keys,
    }
}

fn auth<T>(mut req: Request<T>, key: &str) -> Request<T> {
    req.metadata_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    req
}

fn envelope() -> Envelope {
    Envelope {
        icp_version: "2026-04-21".into(),
        agent_id: AGENT.into(),
        request_id: "req_grpc_test".into(),
        idempotency_key: String::new(),
        mandate_jws: String::new(),
        trace_id: String::new(),
    }
}

async fn grpc_quote(h: &GrpcHandler, key: &str) -> Value {
    let body = quote_body("WIDGET");
    let resp = h
        .submit_intent(auth(
            Request::new(IntentRequest {
                envelope: Some(envelope()),
                payload_json: serde_json::to_vec(&body).unwrap(),
            }),
            key,
        ))
        .await
        .expect("submit")
        .into_inner();
    serde_json::from_slice(&resp.payload_json).unwrap()
}

fn quote_body(sku: &str) -> Value {
    json!({
        "intent": "intent.quote",
        "agent_id": AGENT,
        "params": {
            "items": [{
                "sku": sku,
                "quantity": 1,
                "unit_price_hint": { "amount_minor": 100, "currency": "USD" }
            }]
        }
    })
}

#[tokio::test]
async fn grpc_reads_require_auth_and_are_tenant_scoped() {
    let h = handler().await;
    let body = grpc_quote(&h, "k_a").await;
    let txn_id = body["transaction"]["id"].as_str().unwrap().to_string();
    let receipt_jti = body["receipt"]["jti"].as_str().unwrap().to_string();

    let unauth = h
        .get_transaction(Request::new(GetTransactionRequest {
            envelope: Some(envelope()),
            transaction_id: txn_id.clone(),
        }))
        .await
        .unwrap_err();
    assert_eq!(unauth.code(), tonic::Code::Unauthenticated);

    let cross_tenant = h
        .get_transaction(auth(
            Request::new(GetTransactionRequest {
                envelope: Some(envelope()),
                transaction_id: txn_id.clone(),
            }),
            "k_b",
        ))
        .await
        .unwrap_err();
    assert_eq!(cross_tenant.code(), tonic::Code::NotFound);

    let own = h
        .get_transaction(auth(
            Request::new(GetTransactionRequest {
                envelope: Some(envelope()),
                transaction_id: txn_id,
            }),
            "k_a",
        ))
        .await
        .expect("same tenant transaction")
        .into_inner();
    assert!(!own.payload_json.is_empty());

    let cross_receipt = h
        .get_receipt(auth(
            Request::new(GetReceiptRequest {
                envelope: Some(envelope()),
                receipt_jti: receipt_jti.clone(),
            }),
            "k_b",
        ))
        .await
        .unwrap_err();
    assert_eq!(cross_receipt.code(), tonic::Code::NotFound);

    let own_receipt = h
        .get_receipt(auth(
            Request::new(GetReceiptRequest {
                envelope: Some(envelope()),
                receipt_jti,
            }),
            "k_a",
        ))
        .await
        .expect("same tenant receipt")
        .into_inner();
    assert!(!own_receipt.receipt_jws.is_empty());

    let verified = h
        .verify_receipt(auth(
            Request::new(VerifyReceiptRequest {
                envelope: Some(envelope()),
                receipt_jws: own_receipt.receipt_jws,
                expected_body_json: Vec::new(),
            }),
            "k_a",
        ))
        .await
        .expect("verify receipt")
        .into_inner();
    assert!(verified.valid, "receipt should verify: {}", verified.reason);
}

#[tokio::test]
async fn grpc_submit_intent_replays_same_idempotency_key() {
    let h = handler().await;
    let body = json!({
        "intent": "intent.quote",
        "agent_id": AGENT,
        "params": {
            "items": [{
                "sku": "WIDGET",
                "quantity": 1,
                "unit_price_hint": { "amount_minor": 100, "currency": "USD" }
            }]
        }
    });
    let mut env = envelope();
    env.idempotency_key = "grpc-idem-1".into();
    let request = IntentRequest {
        envelope: Some(env.clone()),
        payload_json: serde_json::to_vec(&body).unwrap(),
    };

    let first = h
        .submit_intent(auth(Request::new(request.clone()), "k_a"))
        .await
        .expect("first")
        .into_inner();
    let second = h
        .submit_intent(auth(Request::new(request), "k_a"))
        .await
        .expect("second")
        .into_inner();
    assert_eq!(first.payload_json, second.payload_json);
    assert_eq!(first.receipt_jws, second.receipt_jws);

    let first_body: Value = serde_json::from_slice(&first.payload_json).unwrap();
    let second_body: Value = serde_json::from_slice(&second.payload_json).unwrap();
    assert_eq!(
        first_body["transaction"]["id"],
        second_body["transaction"]["id"]
    );
    assert_eq!(h.service.transactions.len(), 1);
}

#[tokio::test]
async fn grpc_submit_intent_rejects_reused_idempotency_key_with_different_body() {
    let h = handler().await;
    let mut env = envelope();
    env.idempotency_key = "grpc-idem-conflict".into();
    let body_a = json!({
        "intent": "intent.quote",
        "agent_id": AGENT,
        "params": { "items": [{ "sku": "A", "quantity": 1 }] }
    });
    let body_b = json!({
        "intent": "intent.quote",
        "agent_id": AGENT,
        "params": { "items": [{ "sku": "B", "quantity": 1 }] }
    });
    h.submit_intent(auth(
        Request::new(IntentRequest {
            envelope: Some(env.clone()),
            payload_json: serde_json::to_vec(&body_a).unwrap(),
        }),
        "k_a",
    ))
    .await
    .expect("first");

    let err = h
        .submit_intent(auth(
            Request::new(IntentRequest {
                envelope: Some(env),
                payload_json: serde_json::to_vec(&body_b).unwrap(),
            }),
            "k_a",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::AlreadyExists);
}

#[tokio::test]
async fn grpc_requires_idempotency_key_when_configured() {
    let mut cfg = Config::for_test();
    cfg.require_idempotency_key = true;
    let h = handler_with_config(&mut cfg).await;
    let body = json!({
        "intent": "intent.quote",
        "agent_id": AGENT,
        "params": { "items": [{ "sku": "A", "quantity": 1 }] }
    });

    let err = h
        .submit_intent(auth(
            Request::new(IntentRequest {
                envelope: Some(envelope()),
                payload_json: serde_json::to_vec(&body).unwrap(),
            }),
            "k_a",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn grpc_submit_intent_is_tenant_rate_limited() {
    let mut cfg = Config::for_test();
    cfg.rate_limit_per_minute = 1;
    cfg.pre_auth_rate_limit_per_minute = 100;
    let h = handler_with_config(&mut cfg).await;

    let _first = grpc_quote(&h, "k_a").await;
    let err = h
        .submit_intent(auth(
            Request::new(IntentRequest {
                envelope: Some(envelope()),
                payload_json: serde_json::to_vec(&quote_body("WIDGET-2")).unwrap(),
            }),
            "k_a",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
}

#[tokio::test]
async fn grpc_fake_bearer_floods_are_pre_auth_limited() {
    let mut cfg = Config::for_test();
    cfg.rate_limit_per_minute = 100;
    cfg.pre_auth_rate_limit_per_minute = 1;
    let h = handler_with_config(&mut cfg).await;

    let request = || {
        auth(
            Request::new(IntentRequest {
                envelope: Some(envelope()),
                payload_json: serde_json::to_vec(&quote_body("WIDGET")).unwrap(),
            }),
            "does_not_exist",
        )
    };

    let first = h.submit_intent(request()).await.unwrap_err();
    assert_eq!(first.code(), tonic::Code::Unauthenticated);

    let second = h.submit_intent(request()).await.unwrap_err();
    assert_eq!(second.code(), tonic::Code::ResourceExhausted);
}
