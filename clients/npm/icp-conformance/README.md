# @stateset/icp-conformance

One-command conformance tester for [ICP](https://github.com/stateset/stateset-icp-handler)
(Intelligent Commerce Protocol) handlers.

```bash
npx @stateset/icp-conformance \
  --url http://localhost:8082 \
  --api-key icp_demo_key_123 \
  --agent-id did:stateset:agent:conformance
```

Validates any handler URL against [`ICP_SPEC.md`](../../../docs/specification/ICP_SPEC.md):
discovery shape, JWKS, intent.quote lifecycle, receipt digest
reproducibility, Ed25519 signature verification, error envelope shape,
version header, and more. Exit code `0` = all passed, `1` = at least
one failure.

The heavy lifting is the Rust reference binary
[`icp-conformance`](../../../src/bin/icp_conformance.rs) — 968 lines,
deliberately importing **nothing** from the handler library so that
passing its suite is evidence a *different* handler conforms, not that
it matches StateSet's internals. This npm package is a thin Node
launcher so non-Rust developers can run that binary from one command.

## How it finds the binary

Checked in order; first hit wins:

1. **`ICP_CONFORMANCE_BIN`** — explicit path override (CI pinning).
2. Cached platform binary at
   `~/.cache/stateset/icp-conformance/<version>/icp-conformance`.
   Reserved for a future post-install hook that downloads from a GitHub
   release; not populated automatically in v0.1.
3. **`icp-conformance` on `PATH`** — e.g. `cargo install stateset-icp-handler --bin icp-conformance`.
4. **`cargo run --bin icp-conformance --`** — works when run inside a
   checkout of the monorepo with Cargo installed.

If none of the above resolve, you'll see a single message naming the
install paths and an exit code of `127`.

## With a mandate

The scope-gated conformance tests (`intent.quote`, `intent.buy`,
`intent.return`) need a valid mandate. Supply one with `--mandate`:

```bash
npx @stateset/icp-conformance \
  --url http://localhost:8082 \
  --api-key icp_demo_key_123 \
  --agent-id did:stateset:agent:conformance \
  --mandate "$COMPACT_JWS"
```

Mint a signed mandate with the Python client
(`clients/python/stateset_icp.mandate.sign_mandate`) or any library
that produces an Ed25519 JWS matching the
[ICP_SPEC §6](../../../docs/specification/ICP_SPEC.md#6-mandates) shape.

## Full CLI reference

Run `npx @stateset/icp-conformance --help` — the flag list is the
Rust binary's own, forwarded verbatim.

## License

MIT OR Apache-2.0.
