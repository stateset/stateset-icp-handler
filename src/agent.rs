//! Agent identity & tenant credential resolution.
//!
//! Every ICP request identifies *two* security principals:
//!   1. The **tenant** (merchant), resolved via the bearer API key.
//!   2. The **agent**, identified by the `ICP-Agent-Id` header.
//!
//! This module focuses on tenant / API-key resolution and basic agent-id
//! parsing. Mandate verification lives in `mandate.rs`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyInfo {
    pub key: String,
    pub tenant_id: String,
    pub name: String,
    #[serde(default)]
    pub rate_limit_per_minute: Option<u32>,
    #[serde(default)]
    pub allowed_agents: Option<Vec<String>>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct ApiKeyStore {
    inner: Arc<HashMap<String, ApiKeyInfo>>,
}

impl ApiKeyStore {
    pub fn new(keys: Vec<ApiKeyInfo>) -> Self {
        let mut map = HashMap::with_capacity(keys.len());
        for k in keys {
            map.insert(k.key.clone(), k);
        }
        Self {
            inner: Arc::new(map),
        }
    }

    pub fn demo() -> Self {
        Self::new(vec![ApiKeyInfo {
            key: "icp_demo_key_123".to_string(),
            tenant_id: "merchant_demo".to_string(),
            name: "Bundled demo key".to_string(),
            rate_limit_per_minute: Some(300),
            allowed_agents: None,
            expires_at: None,
        }])
    }

    pub fn lookup(&self, bearer: &str) -> Option<ApiKeyInfo> {
        self.inner.get(bearer).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl ApiKeyInfo {
    pub fn is_expired_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    pub fn permits_agent(&self, agent_id: &str) -> bool {
        self.allowed_agents
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|a| a == "*" || a == agent_id))
    }
}

/// Parsed form of the `ICP-Agent-Id` header. Accepts DID strings, HTTPS
/// URLs, and opaque IDs (for backwards compatibility in dev).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentifier {
    pub raw: String,
    pub kind: AgentIdKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentIdKind {
    DidStateset,
    DidKey,
    DidWeb,
    HttpsProfile,
    Opaque,
}

impl AgentIdentifier {
    pub fn parse(raw: &str) -> Self {
        let kind = if raw.starts_with("did:stateset:agent:") {
            AgentIdKind::DidStateset
        } else if raw.starts_with("did:key:") {
            AgentIdKind::DidKey
        } else if raw.starts_with("did:web:") {
            AgentIdKind::DidWeb
        } else if raw.starts_with("https://") {
            AgentIdKind::HttpsProfile
        } else {
            AgentIdKind::Opaque
        };
        Self {
            raw: raw.to_string(),
            kind,
        }
    }
}
