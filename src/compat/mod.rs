//! Compatibility surfaces.
//!
//! ICP is a *superset* of the two primary open commerce protocols:
//!   - **ACP** — OpenAI Agentic Commerce Protocol (`/checkout_sessions/...`)
//!   - **UCP** — StateSet Universal Commerce Protocol (`/api/checkout-sessions/...`)
//!
//! Each submodule here exposes the protocol's native wire contract but
//! routes every write through the same `IcpService::handle_intent`
//! pipeline, so compat traffic produces the same transactions, the same
//! events, and the same signed receipts as first-class ICP traffic.
//!
//! Authorization model: the tenant's bearer key *is* the self-mandate. A
//! merchant calling its own handler under ACP or UCP is by definition
//! operating within its own authority, so the compat paths call
//! `handle_intent` with `skip_mandate_check = true`. The underlying
//! transaction record still stamps `mandate_jti = None` to make the
//! provenance explicit.

pub mod acp;
pub mod ucp;
