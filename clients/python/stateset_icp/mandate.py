"""Mandate creation and Ed25519 JWS signing (ICP_SPEC §6).

A mandate is a compact JWS:

    base64url(header) "." base64url(payload) "." base64url(signature)

where the signature covers the first two segments as UTF-8 bytes.
Handlers with `ICP_VERIFY_MANDATE_SIGNATURES=true` (the default since
2026-04-21) will reject any mandate whose signature doesn't verify
against the issuer DID's advertised keyset, so a real client needs a
real key. We build `did:key` identifiers inline so the whole flow is
self-contained — no DID registry, no out-of-band key exchange.
"""

from __future__ import annotations

import base64
import json
import time
import uuid
from dataclasses import dataclass
from typing import Any, Optional, Sequence

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ed25519


# --- base58btc (multibase "z" prefix) -------------------------------------
# Hand-rolled so we don't pull a base58 dep just for a single encode call.
_B58_ALPHABET = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def _base58btc_encode(data: bytes) -> str:
    # Count leading zero bytes → equal count of leading '1' chars.
    n = 0
    for b in data:
        if b != 0:
            break
        n += 1
    num = int.from_bytes(data, "big")
    out = bytearray()
    while num > 0:
        num, rem = divmod(num, 58)
        out.append(_B58_ALPHABET[rem])
    out.extend(b"1" * n)
    out.reverse()
    return out.decode("ascii")


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


# --- keypair wrapper ------------------------------------------------------


@dataclass
class Ed25519KeyPair:
    """An Ed25519 keypair paired with a `did:key` identifier.

    Use `Ed25519KeyPair.generate()` for a fresh ephemeral key, or
    `Ed25519KeyPair.from_private_bytes(...)` to load a stored key.
    """

    private_key: ed25519.Ed25519PrivateKey
    public_key: ed25519.Ed25519PublicKey
    did: str
    kid: str

    @classmethod
    def generate(cls) -> "Ed25519KeyPair":
        sk = ed25519.Ed25519PrivateKey.generate()
        return cls._wrap(sk)

    @classmethod
    def from_private_bytes(cls, raw: bytes) -> "Ed25519KeyPair":
        sk = ed25519.Ed25519PrivateKey.from_private_bytes(raw)
        return cls._wrap(sk)

    @classmethod
    def _wrap(cls, sk: ed25519.Ed25519PrivateKey) -> "Ed25519KeyPair":
        pk = sk.public_key()
        did = did_key_from_public_key(pk)
        # Convention: the `kid` is the full DID — so a handler's JWS
        # verifier can look up the key directly without a lookup table.
        kid = did
        return cls(private_key=sk, public_key=pk, did=did, kid=kid)

    def private_bytes_raw(self) -> bytes:
        return self.private_key.private_bytes(
            encoding=serialization.Encoding.Raw,
            format=serialization.PrivateFormat.Raw,
            encryption_algorithm=serialization.NoEncryption(),
        )


def did_key_from_public_key(pk: ed25519.Ed25519PublicKey) -> str:
    """Encode an Ed25519 public key as a `did:key` identifier.

    Follows the W3C did:key spec for Ed25519: multicodec prefix `0xed 0x01`
    followed by the 32 raw public-key bytes, base58btc-encoded with the
    multibase `z` prefix.
    """
    raw = pk.public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    )
    # Ed25519 public key multicodec varint: 0xed 0x01
    prefixed = b"\xed\x01" + raw
    return "did:key:z" + _base58btc_encode(prefixed)


# --- payload construction -------------------------------------------------


def create_mandate_payload(
    *,
    issuer: str,
    subject: str,
    scope: Sequence[str],
    budget_currency: str,
    budget_amount_minor: int,
    budget_per_transaction: Optional[int] = None,
    budget_period: str = "P1D",
    merchants: Optional[Sequence[str]] = None,
    categories: Optional[Sequence[str]] = None,
    jurisdictions: Optional[Sequence[str]] = None,
    require_receipt: bool = True,
    valid_for_secs: int = 3600,
    jti: Optional[str] = None,
    now_secs: Optional[int] = None,
    version: str = "2026-04-21",
) -> dict[str, Any]:
    """Build a MandatePayload dict matching `ICP_SPEC §6`.

    Example::

        mandate = create_mandate_payload(
            issuer=kp.did,
            subject="did:stateset:agent:demo-alice",
            scope=["quote", "authorize", "buy"],
            budget_currency="USD",
            budget_amount_minor=10_000,
            merchants=["*"],
        )
    """
    now = now_secs if now_secs is not None else int(time.time())
    return {
        "iss": issuer,
        "sub": subject,
        "iat": now,
        "nbf": now,
        "exp": now + valid_for_secs,
        "jti": jti or f"mandate-{uuid.uuid4().hex}",
        "icp": {
            "version": version,
            "scope": list(scope),
            "budget": {
                "currency": budget_currency,
                "amount_minor": int(budget_amount_minor),
                "per_transaction": budget_per_transaction,
                "period": budget_period,
            },
            "merchants": list(merchants) if merchants is not None else ["*"],
            "categories": list(categories) if categories is not None else [],
            "jurisdictions": list(jurisdictions) if jurisdictions is not None else [],
            "policies": {
                "require_receipt": require_receipt,
                "require_shipping_address_confirmation": False,
                "prohibit_subscriptions": False,
            },
            "linked_payment_methods": [],
        },
    }


# --- signing --------------------------------------------------------------


def sign_mandate(payload: dict[str, Any], keypair: Ed25519KeyPair) -> str:
    """Produce a compact-JWS mandate (header.payload.signature).

    The signature covers `base64url(header) + "." + base64url(payload)`.
    Key ordering inside the JSON segments is stable (`sort_keys=True`) so
    the same payload always serializes to the same bytes — makes
    debugging and replay tests deterministic.
    """
    header = {"alg": "EdDSA", "typ": "JWT", "kid": keypair.kid}
    header_bytes = json.dumps(header, separators=(",", ":"), sort_keys=True).encode("utf-8")
    payload_bytes = json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8")
    header_b64 = _b64url(header_bytes)
    payload_b64 = _b64url(payload_bytes)
    signing_input = f"{header_b64}.{payload_b64}".encode("ascii")
    signature = keypair.private_key.sign(signing_input)
    return f"{header_b64}.{payload_b64}.{_b64url(signature)}"
