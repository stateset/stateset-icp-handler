//! Protocol-level constants.

/// Wire version of the ICP spec this handler implements.
pub const ICP_VERSION: &str = "2026-04-21";

/// Request header names used by ICP.
pub mod headers {
    pub const ICP_VERSION: &str = "icp-version";
    pub const ICP_AGENT_ID: &str = "icp-agent-id";
    pub const ICP_AGENT_KEY_ID: &str = "icp-agent-key-id";
    pub const ICP_MANDATE: &str = "icp-mandate";
    pub const ICP_REQUEST_ID: &str = "icp-request-id";
    pub const ICP_IDEMPOTENCY_KEY: &str = "icp-idempotency-key";
    pub const ICP_TRACE_ID: &str = "icp-trace-id";
    pub const ICP_SIGNATURE: &str = "icp-signature";
    pub const ICP_RECEIPT: &str = "icp-receipt";
    pub const ICP_RECEIPT_KID: &str = "icp-receipt-kid";
}

/// Maximum accepted request body size, in bytes. Same as sibling handlers.
pub const MAX_REQUEST_BODY_BYTES: usize = 1 * 1024 * 1024; // 1 MiB

/// Default session / transaction TTL (seconds). A transaction that has not
/// reached a terminal state after this many seconds is eligible for GC.
pub const DEFAULT_TRANSACTION_TTL_SECS: u64 = 21_600; // 6h
