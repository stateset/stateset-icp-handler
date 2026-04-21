# ICP Handler Architecture

## Process topology

The ICP handler is a **single Rust binary** that hosts, in one process:

1. An **HTTP server** (axum) on `:8082`.
2. A **gRPC server** (tonic) on `:50052`.
3. An **embedded commerce engine** (`stateset-icommerce`) backed by SQLite
   (or PostgreSQL with the `postgres` feature).
4. An **event bus** (tokio broadcast) consumed by SSE and gRPC streaming.
5. A **receipt signer** (Ed25519) whose public key is advertised at
   `/.well-known/icp/jwks.json`.

There is **no separate database**, no remote commerce API, and no control
plane. Deployment is `docker run`.

```
         stateset-icp-handler (single process)
┌───────────────────────────────────────────────────────┐
│  axum router                                         │
│   ├─ /.well-known/icp         ← discovery            │
│   ├─ /.well-known/icp/jwks.json ← signing keys       │
│   ├─ /icp/v1/intents          ← intent pipeline      │
│   ├─ /icp/v1/transactions/..  ← reads                │
│   ├─ /icp/v1/receipts/..      ← receipt lookup       │
│   ├─ /icp/v1/events:stream    ← SSE                  │
│   ├─ /health, /ready, /metrics                       │
│                                                      │
│  tonic IcpHandler server                             │
│   ├─ GetDiscovery / SubmitIntent / StreamEvents      │
│   └─ GetTransaction / GetReceipt / ...               │
│                                                      │
│  IcpService                                          │
│   ├─ intent parser + scope gate                      │
│   ├─ mandate evaluator (budget/window/merchant)      │
│   ├─ transaction store                               │
│   ├─ receipt signer  ──► ReceiptStore                │
│   └─ event bus       ──► broadcast consumers         │
│                                                      │
│  CommerceEngine  (Arc<stateset_embedded::Commerce>)  │
│   └── SQLite file / PG pool                          │
└───────────────────────────────────────────────────────┘
```

## Request lifecycle (happy path for `intent.buy`)

1. Agent sends `POST /icp/v1/intents` with `Authorization: Bearer <key>`,
   `ICP-Agent-Id`, `ICP-Mandate`, and a JSON body whose `intent` is
   `intent.buy`.
2. The handler resolves the tenant via the bearer key, parses the agent id,
   and evaluates the mandate against the intent's scope (`buy`) and the
   requested amount.
3. The intent is dispatched to `IcpService::do_buy`, which loads the
   transaction, validates state, and calls into the embedded iCommerce
   engine to persist a real order (if buyer email is present).
4. The transaction is transitioned to `Completed`.
5. The mandate's spend counter is incremented by the captured total.
6. The response body is JCS-canonicalized, hashed (SHA-256), wrapped in
   receipt claims, and Ed25519-signed. The compact JWS is returned in the
   `ICP-Receipt` header and inlined in the response payload.
7. An event `transaction.completed` is pushed onto the broadcast channel
   for any live SSE or gRPC subscriber.

## Layering

The handler is intentionally shallow. The layers are:

| Layer | Responsibility |
|---|---|
| `main.rs` / `lib.rs` | Wiring: config load, state build, serve. |
| `auth.rs` / `middleware.rs` | Transport-level cross-cutting (API keys, header defaults). |
| `service.rs` | Intent router — the only place that knows about the intent catalog. |
| `mandate.rs` | Mandate decoding, validation, budget bookkeeping. |
| `commerce.rs` | The only direct consumer of `stateset_embedded::Commerce`. |
| `signing.rs` / `receipts.rs` | Ed25519 JWS receipts and persistent store. |
| `events.rs` | In-process event bus. |
| `grpc.rs` | gRPC surface (payloads are JSON bytes by spec). |
| `discovery.rs` | `/.well-known/icp` document assembly. |

## What the handler intentionally does *not* do in v0.1

- Perform PSP charges (delegated-vault token redemption is the compat path
  for v0.1; direct card capture is out of scope for this release).
- Resolve principal DIDs for mandate signature verification (the mandate
  is scope/budget/window checked, but the signature is currently trusted
  if the JWS is well-formed — verification plugs in via `PrincipalResolver`
  in v0.2).
- Calculate real tax (placeholder 8.75%; engine tax wiring is a v0.2
  deliverable).
- Apply promotions.
- Live-stream from the engine's sync layer; events are in-process only.

These are explicit gaps — the spec, proto, and pipeline are designed so
they slot in without breaking anything that already works.
