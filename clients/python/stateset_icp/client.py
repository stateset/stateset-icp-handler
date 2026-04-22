"""Synchronous HTTP client for ICP handlers.

Derived entirely from the handler's `/openapi.json` document and
`docs/specification/ICP_SPEC.md`. No imports from the Rust source — if
this client works against a running handler, that's evidence the spec is
implementable by a stranger.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from typing import Any, Optional, Sequence

import httpx


# --- errors ---------------------------------------------------------------


@dataclass
class IcpError(Exception):
    """Structured ICP error envelope (ICP_SPEC §12).

    The handler returns a JSON body shaped like::

        { "error": {
            "type": "mandate_out_of_scope",
            "code": "mandate_out_of_scope",
            "message": "...",
            "retriable": false,
            "docs_url": "..." } }

    Any missing fields collapse to `None` / empty string. The exception
    stringifies to `<code> (<status>): <message>` so tracebacks are
    readable without digging into the envelope.
    """

    status_code: int
    type: str
    code: str
    message: str
    retriable: bool = False
    param: Optional[str] = None
    intent_id: Optional[str] = None
    docs_url: Optional[str] = None
    raw: Optional[dict[str, Any]] = None

    def __str__(self) -> str:  # pragma: no cover - trivial
        return f"{self.code or self.type or 'icp_error'} ({self.status_code}): {self.message}"

    @classmethod
    def from_response(cls, response: httpx.Response) -> "IcpError":
        try:
            body = response.json()
        except Exception:
            body = {}
        err = body.get("error", body) if isinstance(body, dict) else {}
        if not isinstance(err, dict):
            err = {}
        return cls(
            status_code=response.status_code,
            type=str(err.get("type", "")),
            code=str(err.get("code", "")),
            message=str(err.get("message", response.text or "")),
            retriable=bool(err.get("retriable", False)),
            param=err.get("param"),
            intent_id=err.get("intent_id"),
            docs_url=err.get("docs_url"),
            raw=body if isinstance(body, dict) else None,
        )


# --- client ---------------------------------------------------------------


class Client:
    """Thin synchronous wrapper over the ICP HTTP surface.

    Example::

        with Client("http://localhost:8082",
                    api_key="icp_demo_key_123",
                    agent_id="did:stateset:agent:demo-alice") as icp:
            resp = icp.submit_intent({
                "intent": "intent.quote",
                "agent_id": "did:stateset:agent:demo-alice",
                "params": { "items": [...] },
            }, mandate_jws=compact_jws)
            print(resp["transaction"]["id"])
    """

    def __init__(
        self,
        base_url: str,
        *,
        api_key: str,
        agent_id: str,
        timeout: float = 30.0,
        verify_tls: bool = True,
        transport: Optional[httpx.BaseTransport] = None,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._api_key = api_key
        self._agent_id = agent_id
        default_headers = {
            "authorization": f"Bearer {api_key}",
            "icp-agent-id": agent_id,
            "accept": "application/json",
        }
        self._http = httpx.Client(
            base_url=self._base_url,
            headers=default_headers,
            timeout=timeout,
            verify=verify_tls,
            transport=transport,
        )

    # ---- lifecycle -------------------------------------------------------

    def close(self) -> None:
        self._http.close()

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    # ---- discovery (no auth required by spec, but we send it anyway
    #      because handlers may reject anonymous requests for rate limiting).

    def discovery(self) -> dict[str, Any]:
        return self._get("/.well-known/icp")

    def jwks(self) -> dict[str, Any]:
        return self._get("/.well-known/icp/jwks.json")

    # ---- core intent pipeline -------------------------------------------

    def submit_intent(
        self,
        envelope: dict[str, Any],
        *,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
        request_id: Optional[str] = None,
        trace_id: Optional[str] = None,
    ) -> dict[str, Any]:
        """POST /icp/v1/intents with optional mandate and idempotency key."""
        headers: dict[str, str] = {"content-type": "application/json"}
        if mandate_jws is not None:
            headers["icp-mandate"] = mandate_jws
        if idempotency_key is not None:
            headers["icp-idempotency-key"] = idempotency_key
        if request_id is not None:
            headers["icp-request-id"] = request_id
        if trace_id is not None:
            headers["icp-trace-id"] = trace_id
        return self._post("/icp/v1/intents", json=envelope, headers=headers)

    def get_transaction(self, transaction_id: str) -> dict[str, Any]:
        return self._get(f"/icp/v1/transactions/{transaction_id}")

    def get_subscription(self, subscription_id: str) -> dict[str, Any]:
        return self._get(f"/icp/v1/subscriptions/{subscription_id}")

    def get_peer_quote(self, peer_quote_id: str) -> dict[str, Any]:
        return self._get(f"/icp/v1/peer_quotes/{peer_quote_id}")

    def get_receipt(self, jti: str) -> dict[str, Any]:
        return self._get(f"/icp/v1/receipts/{jti}")

    def get_mandate_usage(self, jti: str) -> dict[str, Any]:
        return self._get(f"/icp/v1/mandates/{jti}/usage")

    # ---- ergonomic buy lifecycle ---------------------------------------

    def quote(
        self,
        items: Sequence[dict[str, Any]],
        *,
        buyer: Optional[dict[str, Any]] = None,
        ship_to: Optional[dict[str, Any]] = None,
        currency: str = "USD",
        jurisdiction: Optional[str] = None,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Request a priced quote. Returns the full intent response body."""
        params: dict[str, Any] = {"items": list(items)}
        if buyer is not None:
            params["buyer"] = buyer
        if ship_to is not None:
            params["ship_to"] = ship_to
        envelope: dict[str, Any] = {
            "intent": "intent.quote",
            "agent_id": self._agent_id,
            "params": params,
            "context": {"currency": currency},
        }
        if jurisdiction is not None:
            envelope["context"]["jurisdiction"] = jurisdiction
        return self.submit_intent(
            envelope,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    def authorize(
        self,
        transaction_id: str,
        *,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        envelope = {
            "intent": "intent.authorize",
            "agent_id": self._agent_id,
            "params": {"transaction_id": transaction_id},
        }
        return self.submit_intent(
            envelope,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    def buy(
        self,
        transaction_id: str,
        payment: dict[str, Any],
        *,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        envelope = {
            "intent": "intent.buy",
            "agent_id": self._agent_id,
            "params": {
                "transaction_id": transaction_id,
                "payment": payment,
            },
        }
        return self.submit_intent(
            envelope,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    # ---- transport layer ------------------------------------------------

    def _get(self, path: str) -> dict[str, Any]:
        return self._send("GET", path)

    def _post(
        self,
        path: str,
        *,
        json: Any,
        headers: Optional[dict[str, str]] = None,
    ) -> dict[str, Any]:
        return self._send("POST", path, json=json, headers=headers)

    def _send(
        self,
        method: str,
        path: str,
        *,
        json: Any = None,
        headers: Optional[dict[str, str]] = None,
    ) -> dict[str, Any]:
        # Auto-generate a request id if the caller didn't set one, so
        # server logs always have a correlatable identifier.
        merged = dict(headers or {})
        merged.setdefault("icp-request-id", f"req-{uuid.uuid4().hex}")
        response = self._http.request(method, path, json=json, headers=merged)
        if response.status_code >= 400:
            raise IcpError.from_response(response)
        if not response.content:
            return {}
        return response.json()
