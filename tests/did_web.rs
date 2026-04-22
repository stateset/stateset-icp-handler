//! `did:web` resolver integration tests.
//!
//! Spins up an in-process `axum` mock server that serves a W3C DID
//! document at `/.well-known/did.json`, then exercises the
//! `DidWebResolver` directly *and* through the full mandate-verification
//! pipeline. Both `publicKeyMultibase` and `publicKeyJwk` Ed25519
//! encodings are tested. Cache behavior is verified by counting upstream
//! fetches.

use axum::{extract::State, response::IntoResponse, routing::get, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use serde_json::{json, Value};
use stateset_icp_handler::resolver::{
    encode_did_key, encode_multibase_key, DidWebResolver, PrincipalResolver,
};
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

// --------------------------------------------------------------------------
// Mock did:web host
// --------------------------------------------------------------------------

#[derive(Clone)]
struct MockState {
    did_doc: Arc<Value>,
    fetch_count: Arc<AtomicU32>,
}

async fn serve_did(State(state): State<MockState>) -> impl IntoResponse {
    state.fetch_count.fetch_add(1, Ordering::SeqCst);
    Json((*state.did_doc).clone())
}

struct MockServer {
    addr: SocketAddr,
    fetch_count: Arc<AtomicU32>,
    handle: JoinHandle<()>,
}

impl MockServer {
    /// Start a mock server that serves `did_doc` at `path`. Returns the
    /// bound address and a fetch counter.
    async fn start(did_doc: Value, path: &str) -> Self {
        let fetch_count = Arc::new(AtomicU32::new(0));
        let state = MockState {
            did_doc: Arc::new(did_doc),
            fetch_count: fetch_count.clone(),
        };
        let app = Router::new().route(path, get(serve_did)).with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // Yield so the server actually begins accepting before we
        // return — minor but eliminates startup races on slow CI.
        tokio::task::yield_now().await;
        MockServer {
            addr,
            fetch_count,
            handle,
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn http_resolver(ttl: Duration) -> DidWebResolver {
    DidWebResolver::with_options(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        "http",
        ttl,
    )
}

/// Builds a DID document advertising one Ed25519 key in the
/// `publicKeyMultibase` encoding.
fn did_doc_multibase(did: &str, key: &SigningKey) -> Value {
    json!({
        "@context": ["https://www.w3.org/ns/did/v1"],
        "id": did,
        "verificationMethod": [{
            "id": format!("{did}#key-1"),
            "type": "Ed25519VerificationKey2020",
            "controller": did,
            "publicKeyMultibase": encode_multibase_key(&key.verifying_key()),
        }],
        "authentication": [format!("{did}#key-1")],
    })
}

/// Builds a DID document advertising one Ed25519 key in JWK form.
fn did_doc_jwk(did: &str, key: &SigningKey) -> Value {
    let x = URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes());
    json!({
        "@context": ["https://www.w3.org/ns/did/v1"],
        "id": did,
        "verificationMethod": [{
            "id": format!("{did}#key-1"),
            "type": "JsonWebKey2020",
            "controller": did,
            "publicKeyJwk": { "kty": "OKP", "crv": "Ed25519", "x": x },
        }]
    })
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn did_web_resolves_multibase_key() {
    let signing = SigningKey::generate(&mut OsRng);
    let server = MockServer::start(
        did_doc_multibase("did:web:does-not-matter", &signing),
        "/.well-known/did.json",
    )
    .await;
    let did = format!("did:web:localhost%3A{}", server.addr.port());
    let resolver = http_resolver(Duration::from_secs(0));

    let keys = resolver.resolve(&did).await.expect("resolve");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].alg, "EdDSA");
    assert_eq!(keys[0].key.to_bytes(), signing.verifying_key().to_bytes());
}

#[tokio::test]
async fn did_web_resolves_jwk_key() {
    let signing = SigningKey::generate(&mut OsRng);
    let server = MockServer::start(
        did_doc_jwk("did:web:does-not-matter", &signing),
        "/.well-known/did.json",
    )
    .await;
    let did = format!("did:web:localhost%3A{}", server.addr.port());
    let resolver = http_resolver(Duration::from_secs(0));

    let keys = resolver.resolve(&did).await.expect("resolve");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key.to_bytes(), signing.verifying_key().to_bytes());
}

#[tokio::test]
async fn did_web_path_segments_use_nested_url() {
    let signing = SigningKey::generate(&mut OsRng);
    let did_str = "did:web:does-not-matter:users:alice";
    let server = MockServer::start(
        did_doc_multibase(did_str, &signing),
        "/users/alice/did.json",
    )
    .await;
    // Override the host while keeping the path segments — the resolver
    // must construct `/users/alice/did.json`, NOT `/.well-known/did.json`.
    let did = format!("did:web:localhost%3A{}:users:alice", server.addr.port());
    let resolver = http_resolver(Duration::from_secs(0));

    let keys = resolver.resolve(&did).await.expect("resolve");
    assert_eq!(keys.len(), 1);
}

#[tokio::test]
async fn did_web_caches_repeat_fetches() {
    let signing = SigningKey::generate(&mut OsRng);
    let server = MockServer::start(
        did_doc_multibase("did:web:cache-test", &signing),
        "/.well-known/did.json",
    )
    .await;
    let did = format!("did:web:localhost%3A{}", server.addr.port());
    let resolver = http_resolver(Duration::from_secs(60));

    for _ in 0..5 {
        let _ = resolver.resolve(&did).await.unwrap();
    }
    assert_eq!(
        server.fetch_count.load(Ordering::SeqCst),
        1,
        "TTL cache should collapse 5 calls to 1 upstream fetch"
    );
}

#[tokio::test]
async fn did_web_zero_ttl_disables_cache() {
    let signing = SigningKey::generate(&mut OsRng);
    let server = MockServer::start(
        did_doc_multibase("did:web:no-cache", &signing),
        "/.well-known/did.json",
    )
    .await;
    let did = format!("did:web:localhost%3A{}", server.addr.port());
    let resolver = http_resolver(Duration::from_secs(0));

    for _ in 0..3 {
        let _ = resolver.resolve(&did).await.unwrap();
    }
    assert_eq!(
        server.fetch_count.load(Ordering::SeqCst),
        3,
        "ZERO TTL should bypass cache and fetch every time"
    );
}

#[tokio::test]
async fn did_web_rejects_404() {
    // No mock server — fetch will fail to connect.
    let resolver = http_resolver(Duration::from_secs(0));
    let did = "did:web:localhost%3A1";
    let err = resolver.resolve(did).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("network error") || msg.contains("Network"),
        "expected network error, got: {msg}"
    );
}

#[tokio::test]
async fn did_web_rejects_doc_with_no_ed25519_keys() {
    let server = MockServer::start(
        json!({
            "@context": ["https://www.w3.org/ns/did/v1"],
            "id": "did:web:no-keys",
            "verificationMethod": [{
                "id": "did:web:no-keys#k1",
                "type": "JsonWebKey2020",
                "publicKeyJwk": { "kty": "RSA", "n": "...", "e": "AQAB" }
            }]
        }),
        "/.well-known/did.json",
    )
    .await;
    let did = format!("did:web:localhost%3A{}", server.addr.port());
    let resolver = http_resolver(Duration::from_secs(0));
    let err = resolver.resolve(&did).await.unwrap_err();
    assert!(format!("{err}").contains("no Ed25519"));
}

#[tokio::test]
async fn did_web_falls_through_to_did_key_in_composite() {
    // CompositeResolver::default_set tries did:key first, then did:web.
    // A did:key DID must succeed without contacting any did:web server.
    use stateset_icp_handler::resolver::CompositeResolver;
    let signing = SigningKey::generate(&mut OsRng);
    let did = encode_did_key(&signing.verifying_key());

    let resolver = CompositeResolver::default_set();
    let keys = resolver.resolve(&did).await.expect("resolve");
    assert_eq!(keys[0].key.to_bytes(), signing.verifying_key().to_bytes());
}

// --------------------------------------------------------------------------
// End-to-end: signed mandate + did:web principal + handler verification
// --------------------------------------------------------------------------

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use stateset_icp_handler::{
    build_app_state, build_router,
    config::Config,
    resolver::{CompositeResolver, DidKeyResolver},
};
use tower::ServiceExt;

fn sign_mandate(signing: &SigningKey, iss: &str, scopes: &[&str]) -> String {
    let header = serde_json::to_vec(&json!({
        "alg": "EdDSA",
        "typ": "JWT",
        "kid": format!("{iss}#key-1"),
    }))
    .unwrap();
    let now = Utc::now().timestamp();
    let payload = serde_json::to_vec(&json!({
        "iss": iss,
        "sub": "did:stateset:agent:test",
        "iat": now,
        "nbf": now - 60,
        "exp": now + 3600,
        "jti": format!("m_{}", Uuid::new_v4().simple()),
        "icp": {
            "version": "2026-04-21",
            "scope": scopes,
            "budget": { "currency": "USD", "amount_minor": 100_000,
                        "per_transaction": 100_000, "period": "P1D" },
            "merchants": ["*"]
        }
    }))
    .unwrap();
    let h_b64 = URL_SAFE_NO_PAD.encode(&header);
    let p_b64 = URL_SAFE_NO_PAD.encode(&payload);
    let signing_input = format!("{h_b64}.{p_b64}");
    let sig = signing.sign(signing_input.as_bytes());
    let s_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
    format!("{h_b64}.{p_b64}.{s_b64}")
}

#[tokio::test]
async fn end_to_end_did_web_signed_mandate_accepted_by_handler() {
    // 1. Spin up a did:web host serving a signing key.
    let signing = SigningKey::generate(&mut OsRng);
    let dummy_did = "did:web:placeholder"; // overwritten below
    let server = MockServer::start(
        did_doc_multibase(dummy_did, &signing),
        "/.well-known/did.json",
    )
    .await;
    let principal_did = format!("did:web:localhost%3A{}", server.addr.port());

    // 2. Build a handler whose resolver is wired to talk HTTP to that
    //    mock server (production resolver uses HTTPS).
    let mut config = Config::for_test();
    config.require_mandate = true;
    config.verify_mandate_signatures = true;

    let state = build_app_state(&config).await.expect("build_app_state");

    // Inject our test resolver: did:key + http-flavored did:web.
    let test_resolver = Arc::new(
        CompositeResolver::new()
            .with(Box::new(DidKeyResolver))
            .with(Box::new(http_resolver(Duration::from_secs(60)))),
    );
    let signer = (*state.service.signer).clone();
    let new_service = stateset_icp_handler::service::IcpService::with_resolver(
        (*state.config).clone(),
        state.service.engine.clone(),
        signer,
        test_resolver,
    );
    let new_state = stateset_icp_handler::AppState {
        service: Arc::new(new_service),
        keys: state.keys.clone(),
        config: state.config.clone(),
    };
    let app = build_router(new_state);

    // 3. Sign a mandate with the principal's key.
    let mandate = sign_mandate(&signing, &principal_did, &["quote"]);

    // 4. Submit a quote with the mandate header — the handler resolves
    //    the did:web principal over HTTP, fetches the key from the mock
    //    server, and verifies the signature.
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", "Bearer icp_demo_key_123")
        .header("ICP-Agent-Id", "did:stateset:agent:test")
        .header("ICP-Mandate", mandate)
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.quote",
                "agent_id": "did:stateset:agent:test",
                "params": {
                    "items": [
                        { "sku": "WIDGET-001", "quantity": 1,
                          "unit_price_hint": { "amount_minor": 1000, "currency": "USD" } }
                    ]
                },
                "context": { "currency": "USD" }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["transaction"]["state"], "quoted");

    // 5. Confirm the resolver actually contacted the mock server.
    assert!(
        server.fetch_count.load(Ordering::SeqCst) >= 1,
        "mock did:web host should have been fetched"
    );
}
