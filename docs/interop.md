# Interoperability

ICP is a **superset** of the existing open commerce protocols. An ICP
handler can (and should) expose compatibility paths so agents written
against ACP, UCP, MCP, or A2A continue to work — while routing through
the same intent pipeline, the same receipts, and the same engine of
record.

## ACP ⇄ ICP

[OpenAI Agentic Commerce Protocol](https://platform.openai.com/docs/agentic-commerce)
version `2025-09-29`.

| ACP | ICP |
|---|---|
| `POST /checkout_sessions` with items only | `intent.quote` |
| `POST /checkout_sessions` with items + buyer + address | `intent.quote` + `intent.authorize` |
| `POST /checkout_sessions/:id` (update) | `intent.authorize` with merged params |
| `POST /checkout_sessions/:id/complete` | `intent.buy` on same `transaction_id` |
| `POST /checkout_sessions/:id/cancel` | `intent.return` on a pre-fulfillment transaction (state → `canceled`) |
| `POST /agentic_commerce/delegate_payment` | `intent.buy` with `PaymentInstrument::DelegatedVault { token, provider }` |

**Session identity mapping.** The ACP `checkout_session_id` is stored in
`Transaction.external_refs["acp_session_id"]`. A handler serving both
surfaces looks the transaction up by either key.

**Auth.** ACP uses `Authorization: Bearer <api_key>` + `API-Version`. ICP
additionally requires `ICP-Agent-Id` and (when `ICP_REQUIRE_MANDATE=true`)
`ICP-Mandate`. On the compat path, the handler synthesizes a default
agent identity (`did:stateset:agent:acp-<merchant-id>`) and treats the
merchant's own authorization as a **self-mandate** with unbounded scope
limited to that merchant.

## UCP ⇄ ICP

StateSet's [Universal Commerce Protocol](https://github.com/stateset/stateset-ucp-handler)
version `2026-01-11`.

| UCP | ICP |
|---|---|
| `GET /.well-known/ucp` | Emitted alongside `/.well-known/icp`; shares the same capability set. |
| `POST /api/checkout-sessions` | `intent.quote` |
| `PUT /api/checkout-sessions/:id` | `intent.quote` (merge) or `intent.authorize` |
| `POST /api/checkout-sessions/:id/complete` | `intent.buy` |
| `POST /api/checkout-sessions/:id/cancel` | `intent.return` |
| UCP `tokenize` / `detokenize` | ICP tokenization extension (`com.stateset.icp.ext.tokenization`) — maps 1:1. |
| UCP `ap2.merchant_authorization` | ICP **mandate**; UCP's AP2 mandate passthrough is interpreted as a partial mandate. |

**Why use ICP compatibility instead of standing up a UCP handler
separately?** One process, one engine, one set of receipts. An agent that
only speaks UCP keeps working; an agent that upgrades to ICP sees richer
intents (negotiate, return, subscribe, a2a_pay) on the same handler.

## MCP ⇄ ICP

An ICP handler MAY expose its intent catalog as an MCP tool surface. The
mapping is mechanical:

| MCP tool name | ICP intent |
|---|---|
| `icp_search` | `intent.search` |
| `icp_describe` | `intent.describe` |
| `icp_quote` | `intent.quote` |
| `icp_authorize` | `intent.authorize` |
| `icp_buy` | `intent.buy` |
| `icp_track` | `intent.track` |
| `icp_return` | `intent.return` |
| … | … |

MCP clients can fetch the full catalog from
`GET /.well-known/icp` (field `intents`) and dynamically generate tools.
The MCP tool schemas are derived from the intent parameter types in
[`src/models.rs`](../src/models.rs).

## A2A ⇄ ICP

Google's A2A lets agents talk directly. ICP augments A2A with commerce
semantics:

| A2A | ICP |
|---|---|
| `GET /.well-known/agent.json` | Emitted alongside `/.well-known/icp`. |
| `POST /a2a/v1/message:send` with `intent` body | Routed into `IcpService::handle_intent`. |
| A2A task cancellation | `intent.return` (or `intent.cancel_subscription`). |
| A2A agent-to-agent payment | `intent.a2a_pay`. |

## x402 ⇄ ICP

Paid HTTP endpoints returning `HTTP 402 Payment Required` can use ICP as
the payment protocol. The 402 response advertises a `WWW-Authenticate:
ICP …` challenge with a target intent; the agent replies with a signed
mandate-bearing `intent.buy` + `PaymentInstrument::Stablecoin`. The x402
server verifies the receipt before serving the resource.

## Summary

| Property | ACP | UCP | ICP |
|---|---|---|---|
| Discovery | no | `/.well-known/ucp` | `/.well-known/icp` |
| Intent model | no (REST) | partial | **yes** |
| Agent identity | implicit | partial | **required** |
| Signed mandates | no | AP2 passthrough | **first-class** |
| Verifiable receipts | no | partial | **on every state change** |
| Returns/subscriptions | out of scope | out of scope | **in core** |
| Stablecoin payments | out of scope | out of scope | **in core** |
| Peer (A2A) commerce | out of scope | out of scope | **in core** |
| Embedded engine | no | optional | **by design** |

An ICP handler is therefore the smallest footprint from which you can
honor every existing agent contract *and* everything planned in the
agent-first commerce roadmap.
