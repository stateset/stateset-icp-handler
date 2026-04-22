# Changelog — `stateset-icp` (Python)

Release notes for the Python client only. Cross-package changes (Rust
handler, spec, vectors) live in the [monorepo
changelog](../../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this package tracks the ICP spec-version it targets (see README).

## [Unreleased]

### Added
- **`examples/anthropic_agent.py`** — Claude driving an ICP merchant
  end-to-end via tool use. Registers five ICP intents
  (`icp_search` / `icp_quote` / `icp_authorize` / `icp_buy` /
  `icp_track`) as Anthropic tools and runs the standard `tool_use` /
  `tool_result` loop until the model emits `end_turn`. Mandate signed
  once and reused across every write; reads go unmandated.
  `dispatch()` forwards tool_input as kwargs directly into the Python
  client — no glue translation layer. `anthropic` is an optional
  runtime dep (`pip install anthropic` alongside `-e .`). Demonstrates
  the "agent framework as first-class consumer" claim without any
  code between Claude and the merchant that the client library
  doesn't already ship.

### Added
- **ICP-Full parity** — ergonomic wrappers for all 17 intents in the
  spec catalog: `search`, `describe`, `quote`, `authorize`, `buy`,
  `pay`, `track`, `return_` (trailing `_` avoids Python's reserved
  keyword), `refund_request`, `subscribe`, `renew`, `pause`,
  `cancel_subscription`, `a2a_quote`, `a2a_pay` (pay-against-quote
  vs direct-pay with client-side arg validation), `negotiate`
  (mutually-exclusive `proposed_total` / `discount_pct`), and
  `confirm_receipt`.
- **SSE event stream wrapper** — `Client.events()` returns an
  `EventStream` context manager yielding `SseEvent` items parsed from
  `GET /icp/v1/events:stream`. Inline `text/event-stream` parser follows
  the HTML Living Standard (blank-line dispatch, comment discard,
  multi-line `data:`, single-leading-space strip). Auto-parses JSON
  `data:` bodies; falls back to `raw` for anything else. Exiting the
  `with` block closes the underlying HTTP stream even on early
  `break`.
- **`examples/listen.py`** — live SSE tail that pairs with
  `buy_flow.py`. Prints `transaction.*` / `subscription.*` /
  `peer_quote.*` events with a one-line summary.
- Envelope-contract test suite (`tests/test_client_envelopes.py`,
  22 tests) using `httpx.MockTransport` — asserts the wire shape
  every wrapper produces, so client-side regressions surface here
  before a live handler would HTTP 400.
- SSE parser test suite (`tests/test_sse.py`, 10 tests) covering the
  low-level framing rules + the `Client.events()` happy path + 4xx
  error path + unmanaged-iteration guard + clean break-out.

## [0.2.0] — 2026-04-21

Initial public release. Paired with `stateset-icp-handler` 0.2.0.

### Added
- **`Client`** — synchronous `httpx`-based HTTP client targeting any
  conforming ICP handler.
  - `discovery()`, `jwks()` — read the handler's `/.well-known/icp`
    document + JWKS signing keys.
  - `submit_intent(envelope, *, mandate_jws, idempotency_key,
    request_id, trace_id)` — raw envelope submit with full header
    control.
  - `get_transaction(id)`, `get_subscription(id)`, `get_peer_quote(id)`,
    `get_receipt(jti)`, `get_mandate_usage(jti)` — read-side lookups.
  - Ergonomic wrappers for the core buy lifecycle: `quote()`,
    `authorize()`, `buy()`.
  - Auto-generated `ICP-Request-Id` on every call for correlation with
    handler-side logs.
- **`IcpError`** — structured exception mirroring the ICP_SPEC §12
  error envelope (`type`, `code`, `message`, `retriable`, `param`,
  `intent_id`, `docs_url`). Built via `IcpError.from_response(r)` on
  any 4xx/5xx.
- **Mandate signing** (`stateset_icp.mandate`):
  - `Ed25519KeyPair.generate()` / `.from_private_bytes()` — keypair
    wrapper that computes its own `did:key` inline (multicodec
    `0xed 0x01` + base58btc multibase, no extra crypto deps).
  - `did_key_from_public_key(pk)` — standalone encoder.
  - `create_mandate_payload(...)` — builds the ICP_SPEC §6 payload
    shape.
  - `sign_mandate(payload, keypair)` — produces the compact-JWS string
    (header.payload.signature) that a signature-verifying handler
    accepts.
- **Golden-vector interop tests** (`tests/test_vectors.py`) load the
  canonical fixtures from `docs/specification/vectors/` and assert
  byte-identical output against the Rust reference. **Proven
  end-to-end:** a mandate signed by this client decodes and verifies
  under the Rust handler.
- **`examples/buy_flow.py`** — annotated end-to-end demo:
  keypair → mandate → discovery → quote → authorize → buy →
  receipt fetch → mandate usage check.

### Design principles

- **Handwritten from the spec** — no imports from the Rust source, no
  auto-generated client. If this keeps working as the spec evolves,
  the spec is implementable.
- **Two runtime deps only:** `httpx` (HTTP) + `cryptography` (Ed25519).
  No SDK sprawl.
- **Synchronous by default** — simpler to reason about for single-flow
  scripts; a future `AsyncClient` is a straightforward companion when
  a caller needs it.
