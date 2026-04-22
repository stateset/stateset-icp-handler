# stateset-icp-go

Go client for [StateSet ICP](https://github.com/stateset/stateset-icp-handler)
handlers — the reference implementation of the Intelligent Commerce
Protocol.

**Handwritten from `/openapi.json` and
[`ICP_SPEC.md`](../../../docs/specification/ICP_SPEC.md) alone.** No
code imported or generated from the Rust handler; stdlib-only (no
external deps). The third polyglot client alongside
[Python](../../python/) and the two npm packages — together they're
the substrate proof that ICP is genuinely implementable across
languages.

## Status

**v0.0.4 (Unreleased).** The Go client is now at feature parity with
the Python client. Full merchant integration, including streaming:

- Transport + typed errors
- Low-level `SubmitIntent` + every read-side endpoint
- Ergonomic wrappers for all 17 ICP-Full intents
- Ed25519 `did:key` minting + compact-JWS mandate signing,
  byte-identical to the Rust reference and Python client against the
  shared golden vectors at `docs/specification/vectors/`
- **SSE event-stream iteration** (this release) — context-cancellable,
  leak-free iterator over `GET /icp/v1/events:stream`

51 tests, zero runtime dependencies beyond the Go standard library.

## Install

```bash
go get github.com/stateset/stateset-icp-go@v0.0.1
```

## Quick start

```go
package main

import (
    "fmt"
    "log"

    icp "github.com/stateset/stateset-icp-go"
)

func main() {
    c := icp.New(
        "http://localhost:8082",
        "icp_demo_key_123",
        "did:stateset:agent:demo-alice",
    )

    disco, err := c.Discovery()
    if err != nil { log.Fatal(err) }
    fmt.Println("tier:", disco["conformance"].(map[string]any)["tier"])

    resp, err := c.SubmitIntent(
        icp.IntentEnvelope{
            Intent:  "intent.quote",
            AgentID: "did:stateset:agent:demo-alice",
            Params: map[string]any{
                "items": []any{
                    map[string]any{
                        "sku": "WIDGET-001", "quantity": 2,
                        "unit_price_hint": map[string]any{
                            "amount_minor": 2999, "currency": "USD",
                        },
                    },
                },
            },
            Context: map[string]any{"currency": "USD"},
        },
        icp.SubmitOptions{MandateJWS: compactJWS}, // see §6 of ICP_SPEC
    )
    if err != nil {
        if icpErr, ok := err.(*icp.IcpError); ok {
            log.Fatalf("%s (%d): %s", icpErr.Code, icpErr.StatusCode, icpErr.Message)
        }
        log.Fatal(err)
    }
    fmt.Println("transaction:", resp["transaction"])
}
```

## What's covered

### Transport + read-side

| Endpoint | Method |
|---|---|
| `GET /.well-known/icp` | `Discovery()` |
| `GET /.well-known/icp/jwks.json` | `JWKS()` |
| `GET /icp/v1/transactions/{id}` | `GetTransaction(id)` |
| `GET /icp/v1/subscriptions/{id}` | `GetSubscription(id)` |
| `GET /icp/v1/peer_quotes/{id}` | `GetPeerQuote(id)` |
| `GET /icp/v1/receipts/{jti}` | `GetReceipt(jti)` |
| `GET /icp/v1/mandates/{jti}/usage` | `GetMandateUsage(jti)` |
| `POST /icp/v1/intents` (raw) | `SubmitIntent(env, opts)` |

### Per-intent wrappers (17 of 17 ICP-Full intents)

| Intent | Method | Notes |
|---|---|---|
| `intent.search` | `Search(query, limit, opts)` | Read-only |
| `intent.describe` | `Describe(productID, sku, opts)` | Exactly-one validated client-side |
| `intent.quote` | `Quote(QuoteParams, opts)` | Typed params; `Context` emitted when `Currency`/`Jurisdiction` set |
| `intent.authorize` | `Authorize(txnID, opts)` | |
| `intent.buy` | `Buy(txnID, payment, opts)` | Post-authorize |
| `intent.pay` | `Pay(txnID, payment, opts)` | Direct, skips authorize |
| `intent.track` | `Track(txnID, opts)` | Read-only |
| `intent.return` | `Return(txnID, items, reason, opts)` | Go keyword collision is lowercase-only, so `Return` works |
| `intent.refund_request` | `RefundRequest(txnID, amount, reason, opts)` | `amount == nil` = full refund |
| `intent.subscribe` | `Subscribe(SubscribeParams, opts)` | |
| `intent.renew` | `Renew(subID, opts)` | Forces a charge |
| `intent.pause` | `Pause(subID, opts)` | |
| `intent.cancel_subscription` | `CancelSubscription(subID, opts)` | Terminal |
| `intent.a2a_quote` | `A2AQuote(A2AQuoteParams, opts)` | |
| `intent.a2a_pay` | `A2APay(A2APayParams, opts)` | Two shapes: pay-against-quote xor direct-pay (validated) |
| `intent.negotiate` | `Negotiate(NegotiateParams, opts)` | `ProposedTotal` xor `DiscountPct` (validated) |
| `intent.confirm_receipt` | `ConfirmReceipt(txnID, note, opts)` | Escrow-release trigger |

Error envelopes (ICP_SPEC §12) are parsed into `*IcpError` with
`StatusCode`, `Code`, `Message`, `Retriable`, etc. Both the wrapped
`{"error": {...}}` shape and the flat shape are handled.

## Mandate signing

```go
kp, err := icp.GenerateKeyPair()               // fresh did:key
// or: kp, err := icp.KeyPairFromSeed(seedBytes)  // deterministic

payload, _ := icp.NewMandatePayload(icp.MandateOpts{
    Issuer:            kp.DID,
    Subject:           "did:stateset:agent:demo",
    Scope:             []string{"quote", "authorize", "buy"},
    BudgetCurrency:    "USD",
    BudgetAmountMinor: 50_000, // $500 ceiling
})
jws, _ := icp.SignMandate(payload, kp)

// Pass the JWS to any scope-gated intent:
resp, err := c.Quote(
    icp.QuoteParams{Items: items},
    icp.SubmitOptions{MandateJWS: jws},
)
```

**Byte-identical output** to the Python client
(`stateset_icp.sign_mandate`) and the Rust reference
(`serde_jcs` + `ed25519_dalek`) given the same inputs. Verified by
`TestVectorsMandateJWS` which loads the canonical fixtures at
`docs/specification/vectors/mandate_jws.json` and asserts every
segment of the produced JWS matches byte-for-byte.

## Streaming

```go
ctx, cancel := context.WithCancel(context.Background())
defer cancel()

stream, err := c.Events(ctx)
if err != nil {
    log.Fatal(err)
}
defer stream.Close()

for stream.Next() {
    ev := stream.Event()
    if ev.Type == "transaction.completed" {
        handleCompleted(ev.Data) // auto-parsed JSON
    }
}
if err := stream.Err(); err != nil {
    log.Fatal(err)
}
```

Idiomatic Go iterator — `Next()` / `Event()` / `Err()` / `Close()`,
same shape as `sql.Rows` and `bufio.Scanner`. Canceling the context
reliably unblocks `Next()` so long-running consumers don't leak
goroutines or sockets on shutdown (verified by
`TestEventsContextCancellationUnblocksNext`).

## Examples

```bash
# Terminal 1: start the handler
cd ../../..
cargo run --release

# Terminal 2: run the end-to-end demo
cd clients/go/stateset-icp-go
go run ./examples/buyflow
```

Two runnable demos live under [`examples/`](./examples/):

- **[`examples/buyflow/`](./examples/buyflow)** mints a `did:key`,
  signs a mandate, and walks the full lifecycle: discovery → quote →
  authorize → buy → receipt roundtrip → mandate usage. Parallels
  [`clients/python/examples/buy_flow.py`](../../python/examples/buy_flow.py).
  Run with `go run ./examples/buyflow`.
- **[`examples/listen/`](./examples/listen)** tails the SSE event
  stream and prints each event with a fixed-width type column and a
  one-line summary. Parallels
  [`clients/python/examples/listen.py`](../../python/examples/listen.py).
  Ctrl-C exits cleanly via `signal.NotifyContext` cancelling the
  iterator's context. Run with `go run ./examples/listen`.

Run `buyflow` and `listen` in two terminals against the same handler
and the events from the producer show up in the consumer within a
few hundred milliseconds.

Both demos are configurable via `ICP_URL` / `ICP_API_KEY` /
`ICP_AGENT_ID` env, matching the Python examples' contracts.

## Roadmap

| Feature | Status |
|---|---|
| ACP / UCP / MCP compat surfaces | Out of scope — use the native surfaces directly |
| gRPC | Out of scope — use the generated `proto/icp_handler/v1/*` |
| Published binary on GitHub Releases | Planned (pair with handler release tagging) |

## Design

- **Zero non-stdlib deps.** `net/http`, `encoding/json`, `crypto/rand`
  for request-id generation. Stays trivial to audit.
- **Untyped response bodies.** `map[string]any` rather than a typed
  struct per response. Additive field changes on the handler side
  don't break the client.
- **Low-level + high-level.** `SubmitIntent(env, opts)` is the low
  level that covers all 17 ICP-Full intents today; per-intent
  wrappers get added as syntactic sugar without replacing it.
- **Concurrent-safe.** The underlying `*http.Client` is safe for
  shared use across goroutines; `*Client` wraps no mutable state.

## License

MIT OR Apache-2.0.
