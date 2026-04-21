# Security Policy

## Reporting a vulnerability

Please email **security@stateset.com** with:

- A description of the issue and its impact.
- Reproduction steps.
- Affected versions.
- Your contact for coordinated disclosure.

We aim to acknowledge within **1 business day** and to provide an initial
assessment within **5 business days**.

Please do not file public GitHub issues for security problems.

## Supported versions

Only the latest minor of each major line is supported. v0.1 is a cornerstone
release; security fixes will be backported to the `0.1.x` line until
v0.2 ships.

## Defaults

- `ICP_REQUIRE_MANDATE=true` — writes without a mandate JWS are rejected.
- `ICP_REQUIRE_VERSION=true` — requests missing `ICP-Version` are rejected.
- Maximum request body size: 1 MiB.
- Ed25519 receipt keys rotate at least every 90 days in production.
- TLS 1.2+ expected in front of the handler (terminate TLS at a proxy
  like Caddy, nginx, or the cloud load balancer).

## Threat model notes

- **Mandate replay.** Mandates are bounded by `nbf`/`exp` and their `jti`
  is persisted in the mandate ledger. Replay after `exp` is rejected;
  replay within the window is mitigated by budget exhaustion and by
  `ICP-Idempotency-Key`.
- **Body tampering.** Receipts commit to the SHA-256 of the
  JCS-canonicalized response body. Any downstream re-serialization that
  changes bytes is detectable.
- **Receipt forgery.** Public JWKS is the verification anchor. Rotating
  keys SHOULD overlap (serve both old + new kids for 24h) to avoid
  verification outages.
- **Tenant isolation.** API keys carry a `tenant_id`. All engine lookups
  scope by tenant; cross-tenant reads MUST be rejected.
- **SSRF.** Outbound webhook destinations SHOULD be validated against an
  allow-list and MUST NOT follow redirects into private address ranges.
