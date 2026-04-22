"""Python client for StateSet ICP handlers.

This package is intentionally thin — it wraps the HTTP surface described
by the handler's `/openapi.json` document and signs mandates per
ICP_SPEC §6. No code generation; no Rust source imports. If this client
keeps working as the spec evolves, that's evidence the contract is real.
"""

from stateset_icp.client import Client, IcpError
from stateset_icp.mandate import (
    Ed25519KeyPair,
    create_mandate_payload,
    did_key_from_public_key,
    sign_mandate,
)

__all__ = [
    "Client",
    "IcpError",
    "Ed25519KeyPair",
    "create_mandate_payload",
    "did_key_from_public_key",
    "sign_mandate",
]

__version__ = "0.2.0"
