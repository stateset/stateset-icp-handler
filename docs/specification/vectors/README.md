# ICP Test Vectors

Byte-exact golden fixtures for the wire-level operations every ICP
implementation must agree on:

- **`did_key.json`** — Ed25519 public key bytes → `did:key:z…` identifier
  (multicodec `0xed 0x01` prefix + base58btc multibase encoding).
- **`mandate_jws.json`** — compact-JWS mandate signing. Pins the header
  JSON, the payload JSON (JCS-canonicalized per RFC 8785), the b64url
  encodings, and the resulting three-segment JWS given a fixed 32-byte
  Ed25519 seed.
- **`sse_events.json`** — `text/event-stream` parsing. Pins the HTML
  Living Standard §9.2.6 rules (blank-line dispatch, comment
  discard, multi-line `data:` concatenation, single-leading-space
  strip, unknown-field tolerance) as concrete input → output pairs.

## How to use these

Implementers of a non-reference handler or client should run these
vectors through their code and assert byte-equality. A passing
implementation **will interoperate** with the reference implementation
on mandate signing, `did:key` construction, and SSE parsing; a failing
one will see `signature did not verify against principal keyset` errors
even when the crypto is correct (for mandate drift), or silently drop
events (for SSE drift).

The three language clients in this repository exercise the vectors:

- **Rust** (mandate + did:key): `cargo test --test vectors` — regression
  against any change in the reference implementation.
- **Python** (mandate + did:key + SSE): `cd clients/python && pytest
  tests/test_vectors.py tests/test_vectors_sse.py` — interop proof.
- **Go** (mandate + did:key + SSE): `cd clients/go/stateset-icp-go && go
  test -run TestVectors ./...` — interop proof.

## Determinism properties the vectors depend on

1. **Ed25519 is deterministic** (RFC 8032 §5.1.6): signing the same
   bytes with the same key always produces the same signature. No
   randomness in the signing path.
2. **JCS is deterministic** (RFC 8785): sorted keys by UTF-16 code
   unit, no whitespace, ECMA-262 number formatting. Any two conforming
   JCS serializers produce byte-identical output for the same
   JSON-compatible value.
3. **base58btc is deterministic**: fixed alphabet and leading-zero
   convention per the multibase spec.

Together these mean the pre-sign bytes — `base64url(header) + "." +
base64url(payload)` — are identical across implementations, and the
signature over them is therefore identical.

## Regenerating (maintainers only)

The fixtures are generated from `tests/vectors.rs` with
`ICP_REGENERATE_VECTORS=1`:

```bash
ICP_REGENERATE_VECTORS=1 cargo test --test vectors -- --nocapture
```

This **overwrites** the JSON files in place. Review the diff before
committing — any change is a breaking wire-format change for
third-party implementers.
