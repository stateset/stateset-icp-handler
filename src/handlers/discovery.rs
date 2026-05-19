//! `/.well-known/icp` and `/.well-known/icp/jwks.json` endpoints.
//!
//! The discovery document itself is constructed in [`crate::discovery`];
//! this module just wires it to the HTTP surface.

use axum::{extract::State, response::IntoResponse, Json};

use crate::AppState;

#[utoipa::path(
    get,
    path = "/.well-known/icp",
    tag = "ICP Core",
    responses(
        (status = 200, description = "ICP discovery document — advertised intents, signing keys, capabilities, interop surfaces."),
    ),
)]
pub async fn discovery_handler(State(state): State<AppState>) -> impl IntoResponse {
    let doc = crate::discovery::build(&state.config, &state.service.signer);
    Json(doc)
}

#[utoipa::path(
    get,
    path = "/.well-known/icp/jwks.json",
    tag = "ICP Core",
    responses(
        (status = 200, description = "JWKS (Ed25519 verifying keys) used to verify receipt signatures."),
    ),
)]
pub async fn jwks_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({ "keys": [state.service.signer.jwk()] }))
}
