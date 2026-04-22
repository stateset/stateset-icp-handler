# Changelog — `stateset-icp-go`

Release notes for the Go client only. Cross-package changes (Rust
handler, spec, vectors) live in the [monorepo
changelog](../../../CHANGELOG.md).

## [Unreleased]

### Added

- **`examples/listen/main.go`** — live SSE tail paralleling
  `clients/python/examples/listen.py`. Completes the producer/
  consumer pair for Go: run `buyflow` and `listen` in two terminals
  against the same handler and events from the producer surface in
  the consumer within ~200 ms.
  - Configurable via `ICP_URL` / `ICP_API_KEY` / `ICP_AGENT_ID`
    env, matching the Python demo's contract.
  - Prints each event with an ISO-timestamp prefix, a
    fixed-width (28-char) type column, and a one-line `summary()`
    that pulls the interesting fields from the three event
    families (`transaction.*`, `subscription.*`, `peer_quote.*`)
    — totals rendered as `USD 59.98`-style text when present.
  - **Ctrl-C exits cleanly** via `signal.NotifyContext` —
    SIGINT/SIGTERM cancels the context, which unblocks `Next()`
    and releases the HTTP connection via `defer stream.Close()`.
    No leaked goroutines or sockets, confirmed by the parent
    library's `TestEventsContextCancellationUnblocksNext`.
  - Distinguishes clean EOF ("server closed stream") from Ctrl-C
    ("stopped.") in its exit message — useful for operators
    distinguishing scheduled handler shutdowns from their own
    cancellation.
- **7 tests for the display helpers** (`main_test.go` in the listen
  example's package):
  - `summary` with totals: `transaction.*` events render
    `transaction_id=... total=USD X.YY`
  - `summary` for `subscription.*`: renders `subscription_id=...
    agent_id=...`
  - `summary` for `peer_quote.*`: renders exact
    `peer_quote_id=pq_N`
  - `summary` on keep-alive (nil data) returns empty
  - `padRight` pads and doesn't truncate
  - `nonEmpty` fallback behavior
  - `join` empty/one/many cases

- **`examples/buyflow/main.go`** — end-to-end runnable demo paralleling
  `clients/python/examples/buy_flow.py`. Mints a fresh `did:key`, signs
  a $500-ceiling mandate covering `quote`/`authorize`/`buy`, connects
  to a handler at `$ICP_URL` (defaults to `http://localhost:8082`),
  and walks:
  1. `Discovery()` — reports the handler's advertised conformance tier
  2. `Quote(...)` — prices a 2-widget basket
  3. `Authorize(txnID, ...)` — reserves funds
  4. `Buy(txnID, payment, ...)` — captures + receipt
  5. `GetReceipt(jti)` — round-trips the signed receipt through
     persistent storage
  6. `GetMandateUsage(jti)` — reports spend against the $500 cap
  Handler-side errors (`*IcpError`) surface via `errors.As` with
  structured code + status + message; other failures pass through
  `log.Fatalf` cleanly. Proves the Go client's 17-intent surface +
  mandate signing + read-side endpoints all compose end-to-end without
  any glue beyond what the library already ships.
- Run with `go run ./examples/buyflow` from the package root.
  Configurable via `ICP_URL` / `ICP_API_KEY` / `ICP_AGENT_ID` env vars,
  matching the Python example's contract.

### Added

- **SSE cross-language golden-vector test** (`vectors_sse_test.go`)
  — loads the shared fixture at
  `docs/specification/vectors/sse_events.json` and drives each case
  through the full `Client.Events()` → `EventStream.Next()` path
  against an `httptest` server. 7 cases covering blank-line
  dispatch, multi-line `data:` concatenation, comment discard,
  unknown-field tolerance, single-leading-space strip,
  id-only-event dispatch, and the peer-quote event family.
  Paired with an equivalent Python test; both parsers produce
  identical `SseEvent` sequences for the same wire bytes — pins
  the on-wire SSE framing across languages the same way
  `vectors_test.go` pins the mandate-JWS framing.

### Added

- **SSE event-stream iteration** (`events.go`) — closes the last
  parity gap vs the Python client.
  - `Client.Events(ctx) (*EventStream, error)` opens a streaming
    connection to `GET /icp/v1/events:stream` with an `Accept:
    text/event-stream` header. 4xx/5xx surface as `*IcpError` before
    iteration starts, matching the rest of the client surface.
  - `EventStream` is a Go-native iterator (`Next` / `Event` / `Err`
    / `Close`), matching `sql.Rows` and `bufio.Scanner`. Always
    close via `defer stream.Close()` to free the HTTP connection.
  - `SseEvent{ID, Type, Data, Raw}` — `Data` is the auto-parsed JSON
    body (nil for non-JSON); `Raw` always holds the exact `data:`
    bytes so callers can parse differently if they need to.
  - **Context cancellation reliably unblocks `Next()`** on an idle
    stream, verified by `TestEventsContextCancellationUnblocksNext`
    driving a real `httptest` server that stays open until the
    client disconnects. Long-running consumers don't leak
    goroutines or sockets on shutdown.
  - `Close()` is idempotent (can be called after the first EOF
    without panicking); `Err()` distinguishes clean EOF from a
    read error.
  - Inline text/event-stream parser follows HTML Living Standard
    §9.2.6 — blank-line dispatch, `:` comment discard, multi-line
    `data:` concatenation, single-leading-space strip, unknown
    fields silently ignored. 1 MB line cap on the scanner so a
    single oversized event can't OOM.
- **10 new tests** covering the parser rules and the end-to-end
  behavior:
  - 6 parser tests drive canned bodies through an `httptest` server
    (blank-line dispatch, multi-line `data:`, keep-alive comment
    discard, single-leading-space strip, unknown-field ignore,
    non-JSON raw preservation).
  - 4 end-to-end + lifecycle tests: `Events()` request shape +
    headers, 4xx → `*IcpError` before first `Next()`, context
    cancel unblocks Next() within 2 s, `Close()` idempotency.
- **51 total Go tests pass** (9 transport + 19 per-intent wrappers
  + 11 mandate primitives + 2 golden vectors + 10 SSE).

### Added

- **Ed25519 keypair + `did:key` + compact-JWS mandate signing**
  (`mandate.go`) — closes the parity gap with the Python client for
  every operation that matters for merchant integration.
  - `GenerateKeyPair()` mints a fresh Ed25519 keypair with its
    `did:key` identifier computed inline.
  - `KeyPairFromSeed(seed)` reconstructs a deterministic keypair —
    used by the golden-vector tests to reproduce a known-answer
    against the Rust/Python sides.
  - `DIDKeyFromPublicKey(pk)` is the standalone encoder: multicodec
    `0xed 0x01` + 32-byte pubkey, base58btc-encoded with the
    multibase `z` prefix.
  - `NewMandatePayload(MandateOpts)` constructs the `MandatePayload`
    shape from ICP_SPEC §6 with sensible defaults (`version` =
    `2026-04-21`, `period` = `P1D`, `ValidForSecs` = 3600,
    `Merchants` = `["*"]`, empty slices for categories /
    jurisdictions). Integer fields stored as `int64` so JSON
    marshaling emits plain decimals, not exponential notation —
    critical for JCS byte-compatibility with Python/Rust.
  - `SignMandate(payload, keypair)` emits
    `base64url(JCS(header)) + "." + base64url(JCS(payload)) + "." + base64url(sig)`.
    Go's `encoding/json` sorts map keys by codepoint, matching JCS
    RFC 8785 semantics for the ASCII-only mandate field set.
- **Zero non-stdlib deps.** `crypto/ed25519`, `crypto/rand`,
  `encoding/base64`, `encoding/json`, `math/big` (for base58btc).
  The whole package is still audit-trivial.
- **Cross-language golden-vector tests** (`vectors_test.go`) load the
  shared fixtures at `docs/specification/vectors/*.json` and assert
  the Go implementation produces byte-identical `did:key` and
  compact-JWS output to the Rust reference and the Python client.
  **Substrate claim extended from two languages to three:** a
  mandate signed by this Go client decodes and verifies under the
  Rust handler, and produces the exact same JWS string as the
  Python client given the same seed + payload. Ed25519 is
  deterministic (RFC 8032 §5.1.6), so matching signatures are
  evidence of matching pre-sign bytes — which is evidence of
  matching JCS canonicalization.
- **11 new mandate unit tests** (`mandate_test.go`) independent of
  the vector fixtures: `did:key` size validation + format sanity
  (every Ed25519 did:key starts with `did:key:z6Mk`), seed
  determinism, sign-and-verify roundtrip, nil-keypair rejection,
  `NewMandatePayload` defaults + required-field validation,
  base58btc Bitcoin-convention leading-zero handling +
  known-answer test (`"Hello World!" → "2NEpo7TZRRrLZSi2U"`),
  `canonicalJSON` sorts + strips whitespace.
- **41 total Go tests pass** (9 transport + 19 per-intent wrappers
  + 11 mandate primitives + 2 golden vectors).

### Added

- **Per-intent ergonomic wrappers for all 17 ICP-Full intents**
  (`intents.go`) — the Go client now matches Python feature-for-feature
  at the intent level:
  - Buy lifecycle: `Quote(QuoteParams, …)`, `Authorize`, `Buy`, `Pay`
  - Read-side: `Search`, `Describe` (exactly-one-of validated),
    `Track`
  - Post-sale: `Return` (the exported identifier is fine — Go
    keywords are lowercase-only), `RefundRequest`,
    `ConfirmReceipt`, `Negotiate(NegotiateParams, …)` with
    exactly-one-of `ProposedTotal` / `DiscountPct` validated
    client-side
  - Subscriptions: `Subscribe(SubscribeParams, …)`, `Renew`,
    `Pause`, `CancelSubscription`
  - A2A: `A2AQuote(A2AQuoteParams, …)`, `A2APay(A2APayParams, …)`
    with pay-against-quote vs direct-pay validated client-side
- **Typed params structs** (`QuoteParams`, `SubscribeParams`,
  `NegotiateParams`, `A2AQuoteParams`, `A2APayParams`) for the
  intents with enough optional fields to warrant named args.
  Simpler intents stay on plain positional args. Zero-valued fields
  are omitted from the wire envelope.
- **Shared `call(intent, params, opts)` helper** centralizes envelope
  construction so all wrappers emit the canonical `IntentEnvelope`
  shape without duplicating the builder logic.
- **19 new envelope-shape tests** (`intents_test.go`, runnable with
  `go test ./...`) using `net/http/httptest`:
  - Each wrapper's HTTP method + URL
  - Exact `IntentEnvelope` body — intent name, params, context
  - Optional-field omission when zero-valued
  - Mandate + idempotency header propagation
  - Client-side validation: `Describe` requires one of two,
    `Negotiate` requires one of two, `A2APay` rejects the three
    ambiguous arg combinations
  - A compile-time contract at the end of the file names every
    intent method so a rename or removal stops the test binary
    from compiling — cheap regression guard for the 17-method
    surface.
- **28 total Go tests pass** (9 transport + 19 intents).

## [0.0.1] — 2026-04-22

Initial MVP. Third polyglot client after
[`stateset-icp`](../../python) (Python) and
[`@stateset/icp-conformance`](../../npm/icp-conformance) /
[`@stateset/create-icp-commerce`](../../npm/create-icp-commerce) (npm).

### Added

- **`Client`** — synchronous `net/http`-based client targeting any
  conforming ICP handler. Concurrent-safe.
  - `New(baseURL, apiKey, agentID, cfg?)` with optional `Config`
    controlling timeout + transport. Defaults to a 30-second timeout.
  - `Discovery()`, `JWKS()` — read the handler's discovery document
    and receipt-signing verifying keys.
  - `SubmitIntent(envelope, opts)` — low-level call to
    `POST /icp/v1/intents`. Full control over `ICP-Mandate`,
    `ICP-Idempotency-Key`, `ICP-Request-Id`, `ICP-Trace-Id` via
    `SubmitOptions`.
  - `GetTransaction` / `GetSubscription` / `GetPeerQuote` /
    `GetReceipt` / `GetMandateUsage` — the five read-side lookups.
  - Auto-generated `ICP-Request-Id` (`req-<8 hex bytes>`) on every
    outbound call so handler logs always have a correlation id.
- **`IntentEnvelope`** — canonical body type posted to
  `/icp/v1/intents`. `Params` and `Context` typed as
  `map[string]any` so callers can emit any spec-defined shape
  without the client pre-committing a per-intent struct for every
  one of 17 intents.
- **`IcpError`** — structured error envelope (ICP_SPEC §12) with
  `StatusCode`, `Code`, `Type`, `Message`, `Param`, `IntentID`,
  `Retriable`, `DocsURL`. Parses both the wrapped (`{"error":{…}}`)
  and the flat envelope shapes; falls back to status-only on
  unparseable bodies.
- **9 tests** using `net/http/httptest` covering: read-side routing,
  submit-intent envelope + header shape, auto request-id generation,
  absence of optional headers when unset, wrapped error parsing,
  flat error parsing, unparseable-body fallback, `Error()` string
  format.

### Design principles

- **Zero non-stdlib deps.** Audit surface stays trivial.
- **Untyped response bodies.** Additive server-side field changes
  don't break the client.
- **Handwritten from spec.** No code imported or generated from the
  Rust handler. If this client keeps working as the spec evolves,
  the spec is implementable.
