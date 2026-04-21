# Contributing to StateSet ICP Handler

Thanks for your interest! The ICP handler is the reference
implementation of the Intelligent Commerce Protocol. Changes here travel
through to every agent that targets ICP, so we keep a high bar for
clarity, determinism, and spec alignment.

## Ground rules

1. **Spec first.** If your change alters the wire format, update
   [`docs/specification/ICP_SPEC.md`](./docs/specification/ICP_SPEC.md)
   *before* changing code. Spec is authoritative; implementation follows.
2. **Additive by default.** New intents, fields, and extensions are
   additive and date-bumped. Breaking changes require a new major line
   and a separate RFC.
3. **Deterministic execution.** The intent pipeline must produce
   equivalent outputs for equivalent inputs across handlers. If you
   introduce non-determinism, guard it behind an explicit config knob.
4. **No untested receipts.** Every code path that emits a receipt is
   exercised by at least one integration test.

## Development setup

```bash
# Required: a sibling checkout of stateset-icommerce
cd ..
git clone https://github.com/stateset/stateset-icommerce
cd stateset-icp-handler

cargo build
cargo test
cargo run
```

The `stateset-icommerce` path dependency is relative (`../stateset-icommerce`).
If you prefer another layout, vendor the engine or add a patch override.

## Commit conventions

- Prefix with an area tag: `spec:`, `service:`, `mandate:`, `signing:`,
  `grpc:`, `docs:`, `tests:`, `infra:`.
- Keep the first line under 72 characters; wrap the body at 80.
- Reference the spec section you touched when applicable.

## Pull requests

- Small, focused changes. If you find unrelated cleanup mid-change, land it
  as a separate PR.
- Include a brief summary, a test plan, and (if protocol-facing) the spec
  sections you updated.
- Pass `cargo fmt --all --check`, `cargo clippy --workspace --all-targets`,
  and `cargo test`.

## Coding conventions

- Keep the intent router in `service.rs` flat — avoid indirection that
  makes it hard to see the full lifecycle of an intent at a glance.
- Do not leak engine types through the public surface; `models.rs` is the
  wire contract.
- Prefer `serde_jcs` for any canonicalization. Don't roll your own.
- Public error messages should never leak database internals, API keys,
  or mandate contents.

## Reporting bugs

File issues with:
- Handler version (`cargo run --version` + `git rev-parse HEAD`).
- Request/response pair (redact PII).
- Expected vs actual.
- Relevant log lines (structured JSON).

For security issues, see [`SECURITY.md`](./SECURITY.md).
