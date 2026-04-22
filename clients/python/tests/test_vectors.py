"""Interop tests against the shared golden vectors.

These load the fixtures from `docs/specification/vectors/` and assert
the Python client produces byte-identical output to the Rust reference.
If these pass, the two implementations agree on the wire format and a
Python-signed mandate will verify against the Rust handler (and vice
versa).

Run with::

    cd clients/python
    pip install -e .
    pytest tests/test_vectors.py
"""

from __future__ import annotations

import json
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric import ed25519

from stateset_icp import (
    Ed25519KeyPair,
    did_key_from_public_key,
    sign_mandate,
)

# Resolve `<repo>/docs/specification/vectors/` from this file's location.
# __file__ is at <repo>/clients/python/tests/test_vectors.py, so climb
# three parents to reach the repo root.
REPO_ROOT = Path(__file__).resolve().parents[3]
VECTORS = REPO_ROOT / "docs" / "specification" / "vectors"


def _load(name: str) -> dict:
    return json.loads((VECTORS / name).read_text())


# ------------------------------------------------------------------
# did:key encoding
# ------------------------------------------------------------------


def test_did_key_vectors_match_reference():
    data = _load("did_key.json")
    assert data["vectors"], "did_key.json has no vectors"
    for v in data["vectors"]:
        pk = ed25519.Ed25519PublicKey.from_public_bytes(
            bytes.fromhex(v["public_key_hex"])
        )
        got = did_key_from_public_key(pk)
        assert got == v["expected_did"], (
            f"did:key mismatch for `{v['name']}`: "
            f"got {got!r}, want {v['expected_did']!r}"
        )


# ------------------------------------------------------------------
# Mandate JWS construction
# ------------------------------------------------------------------


def test_mandate_jws_bytes_match_reference():
    """Python `sign_mandate` must produce byte-identical output to Rust.

    Since Ed25519 is deterministic, this only holds when the pre-sign
    bytes (b64url(JCS(header)) + "." + b64url(JCS(payload))) are also
    byte-identical — proving both implementations canonicalize the same
    way.
    """
    data = _load("mandate_jws.json")
    assert data["vectors"], "mandate_jws.json has no vectors"

    for v in data["vectors"]:
        seed = bytes.fromhex(v["private_key_seed_hex"])
        keypair = Ed25519KeyPair.from_private_bytes(seed)

        # did:key derived from the seed must match the fixture.
        assert keypair.did == v["issuer_did"], (
            f"did:key mismatch for `{v['name']}`: "
            f"got {keypair.did!r}, want {v['issuer_did']!r}"
        )
        assert keypair.kid == v["kid"]

        # Full JWS produced by the Python client must be byte-identical
        # to the Rust reference. If this fails, either JCS serialization
        # drifted or the signer's pre-sign bytes are different.
        jws = sign_mandate(v["payload"], keypair)
        assert jws == v["expected_compact_jws"], (
            f"compact JWS mismatch for `{v['name']}`:\n"
            f"  got:  {jws}\n"
            f"  want: {v['expected_compact_jws']}\n"
            f"Most likely cause: JSON canonicalization drift. Compare\n"
            f"  Python json.dumps(sort_keys=True, separators=(',',':'))\n"
            f"vs. Rust serde_jcs::to_vec on the same payload."
        )

        # Decompose and verify each segment — makes a failure easier to
        # pinpoint than the full compact form.
        parts = jws.split(".")
        assert parts[0] == v["expected_header_b64url"], "header b64url drift"
        assert parts[1] == v["expected_payload_b64url"], "payload b64url drift"
        assert parts[2] == v["expected_signature_b64url"], "signature drift"
