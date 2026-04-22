//! Discovery document (`/.well-known/icp`).
//!
//! The discovery payload advertises the handler's capabilities: transports,
//! supported intents, currencies, jurisdictions, signing keys, and interop
//! surfaces (ACP / UCP / MCP / A2A).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::intent::Intent;
use crate::signing::ReceiptSigner;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryDocument {
    pub icp_version: String,
    pub handler_id: String,
    pub service_name: String,
    pub supported_versions: Vec<String>,
    /// Declared conformance tier (see ICP_SPEC §15.1).
    ///
    /// `{ "tier": "icp-core" | "icp-full", "missing_intents": [...] }`.
    /// `missing_intents` names any ICP-Full intents absent from this
    /// handler's advertised `intents` set — empty for an `icp-full`
    /// handler, populated for `icp-core`.
    pub conformance: Value,
    pub transports: Value,
    pub intents: Vec<String>,
    pub currencies: Vec<String>,
    pub jurisdictions: Vec<String>,
    pub payment_methods: Vec<Value>,
    pub signing_keys: Vec<Value>,
    pub profile_url: String,
    pub compatibility: Value,
    pub extensions: Vec<Value>,
}

pub fn build(config: &Config, signer: &ReceiptSigner) -> DiscoveryDocument {
    let http = config.public_base_url.clone();
    let grpc = format!("grpc://{}:{}", config.grpc_host, config.grpc_port);
    let sse = format!("{}/icp/v1/events:stream", http);
    let mcp = format!("{}/mcp", http);
    let a2a = format!("{}/a2a/v1", http);

    // Only advertise intents this handler actually serves — otherwise
    // clients see the intent in `intents` and then get
    // `intent_not_supported` back when they call it (caught by
    // conformance test `discovery_intents_all_live`).
    let intents: Vec<String> = Intent::CORE
        .iter()
        .filter(|i| i.is_implemented())
        .map(|i| i.wire_name().to_string())
        .collect();

    // ICP-Full = ICP-Core + negotiate + confirm_receipt (see spec §15.1).
    // The tier is derived from the concrete intent set so a discovery
    // document can never lie about what the handler actually serves.
    let full_only = ["intent.negotiate", "intent.confirm_receipt"];
    let missing_full: Vec<String> = full_only
        .iter()
        .filter(|name| !intents.iter().any(|i| i == *name))
        .map(|s| s.to_string())
        .collect();
    let tier = if missing_full.is_empty() {
        "icp-full"
    } else {
        "icp-core"
    };
    let conformance = json!({
        "tier": tier,
        "missing_intents": missing_full,
    });

    let transports = json!({
        "http": http,
        "grpc": grpc,
        "sse_events": sse,
        "mcp": if config.mcp_enabled { Value::String(mcp) } else { Value::Null },
        "a2a": if config.a2a_enabled { Value::String(a2a) } else { Value::Null },
    });

    let payment_methods = vec![
        json!({ "id": "card", "brands": ["visa", "mastercard", "amex"] }),
        json!({
            "id": "stablecoin",
            "assets": ["USDC", "ssUSD"],
            "chains": ["base", "set", "solana"],
        }),
        json!({ "id": "delegated_vault", "spec": "acp.delegated_payment" }),
        json!({ "id": "a2a", "spec": "icp.a2a_pay" }),
    ];

    let compatibility = json!({
        "acp": if config.acp_compat_enabled {
            json!({ "version": "2025-09-29", "base_url": config.public_base_url })
        } else {
            Value::Null
        },
        "ucp": if config.ucp_compat_enabled {
            json!({
                "version": "2026-01-11",
                "base_url": format!("{}/ucp", config.public_base_url),
            })
        } else {
            Value::Null
        },
        "mcp": if config.mcp_enabled {
            json!({ "tools_url": format!("{}/mcp", config.public_base_url) })
        } else {
            Value::Null
        },
        "a2a": if config.a2a_enabled {
            json!({ "agent_card_url": format!("{}/.well-known/agent.json", config.public_base_url) })
        } else {
            Value::Null
        },
    });

    DiscoveryDocument {
        icp_version: config.icp_version.clone(),
        handler_id: config.handler_id.clone(),
        service_name: config.service_name.clone(),
        supported_versions: vec![config.icp_version.clone()],
        conformance,
        transports,
        intents,
        currencies: vec![
            "USD".into(),
            "EUR".into(),
            "GBP".into(),
            "USDC".into(),
            "ssUSD".into(),
        ],
        jurisdictions: vec![
            "US".into(),
            "CA".into(),
            "GB".into(),
            "DE".into(),
            "FR".into(),
        ],
        payment_methods,
        signing_keys: vec![signer.jwk()],
        profile_url: format!("{}/.well-known/icp/profile.json", config.public_base_url),
        compatibility,
        extensions: vec![],
    }
}
