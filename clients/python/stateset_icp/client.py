"""Synchronous HTTP client for ICP handlers.

Derived entirely from the handler's `/openapi.json` document and
`docs/specification/ICP_SPEC.md`. No imports from the Rust source — if
this client works against a running handler, that's evidence the spec is
implementable by a stranger.
"""

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass, field
from typing import Any, Iterator, Optional, Sequence

import httpx


# Spec revision this client targets, sent as the `ICP-Version` header on
# every request (handlers require it by default). Keep in sync with
# `ICP_VERSION` in the handler's src/constants.rs.
ICP_VERSION = "2026-04-21"


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


# --- server-sent events --------------------------------------------------


@dataclass
class SseEvent:
    """A single event parsed from `GET /icp/v1/events:stream`.

    * `id` — the SSE `id:` field; stable identifier for resume.
    * `type` — the SSE `event:` field (e.g. `transaction.quoted`,
      `subscription.renewed`, `peer_quote.accepted`).
    * `data` — the `data:` body, JSON-parsed if it is valid JSON,
      otherwise `None`.
    * `raw` — the exact `data:` body bytes as received (pre-parse).
    """

    id: Optional[str] = None
    type: Optional[str] = None
    data: Optional[dict] = None
    raw: str = ""


class EventStream:
    """Context-managed iterator over the handler's SSE event stream.

    Usage::

        with icp.events() as stream:
            for event in stream:
                if event.type == "transaction.completed":
                    handle(event.data)

    Opens the connection on `__enter__`, closes it on `__exit__` (or on
    the first `IcpError` if the handler responds with 4xx/5xx before
    emitting any events). Parses `text/event-stream` inline per the
    HTML Living Standard so no extra dependency is needed.
    """

    def __init__(
        self,
        http: httpx.Client,
        path: str,
        headers: dict[str, str],
    ) -> None:
        self._http = http
        self._path = path
        self._headers = headers
        self._cm: Any = None
        self._response: Optional[httpx.Response] = None

    def __enter__(self) -> "EventStream":
        self._cm = self._http.stream("GET", self._path, headers=self._headers)
        self._response = self._cm.__enter__()
        if self._response.status_code >= 400:
            # Drain the body once so IcpError.from_response sees it.
            self._response.read()
            err = IcpError.from_response(self._response)
            self._cm.__exit__(None, None, None)
            raise err
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        if self._cm is not None:
            self._cm.__exit__(exc_type, exc, tb)
        self._cm = None
        self._response = None

    def __iter__(self) -> Iterator[SseEvent]:
        if self._response is None:
            raise RuntimeError("EventStream must be used as a context manager")
        yield from _parse_sse(self._response.iter_lines())


def _parse_sse(lines: Iterator[str]) -> Iterator[SseEvent]:
    """Parse the text/event-stream framing.

    A line of the form ``field: value`` sets the corresponding piece of
    the next event. A blank line dispatches the accumulated event. A
    line beginning with ``:`` is a comment (typically a keep-alive) and
    is discarded. Unknown fields are ignored per the spec.
    Multiple `data:` lines concatenate with `\\n`.
    """
    event_id: Optional[str] = None
    event_type: Optional[str] = None
    data_parts: list[str] = []

    for line in lines:
        # httpx strips the trailing newline; a blank line here means the
        # server sent "\n\n" which dispatches the event.
        if line == "":
            if data_parts or event_type is not None or event_id is not None:
                raw = "\n".join(data_parts)
                data: Optional[dict]
                try:
                    parsed = json.loads(raw) if raw else None
                    data = parsed if isinstance(parsed, dict) else None
                except (ValueError, json.JSONDecodeError):
                    data = None
                yield SseEvent(id=event_id, type=event_type, data=data, raw=raw)
            event_id = None
            event_type = None
            data_parts = []
            continue
        if line.startswith(":"):
            # Comment (keep-alive). Spec says discard.
            continue
        field_name, sep, value = line.partition(":")
        if not sep:
            # Field-only line per SSE spec equals empty value.
            field_name, value = line, ""
        # The spec says a single leading space on value is stripped.
        if value.startswith(" "):
            value = value[1:]
        if field_name == "id":
            event_id = value
        elif field_name == "event":
            event_type = value
        elif field_name == "data":
            data_parts.append(value)
        # retry, and unknown fields: silently ignored per spec.


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
        icp_version: str = ICP_VERSION,
        timeout: float = 30.0,
        verify_tls: bool = True,
        transport: Optional[httpx.BaseTransport] = None,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._api_key = api_key
        self._agent_id = agent_id
        # Handlers default to requiring ICP-Version on every intent write
        # (ICP_REQUIRE_VERSION=true), so send it on every request — omitting
        # it makes a default-configured server reject all writes with 400.
        default_headers = {
            "authorization": f"Bearer {api_key}",
            "icp-agent-id": agent_id,
            "icp-version": icp_version,
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

    def events(self) -> EventStream:
        """Open the SSE event stream at `/icp/v1/events:stream`.

        Returns an `EventStream` context manager that yields `SseEvent`
        items for each `transaction.*`, `subscription.*`, and
        `peer_quote.*` event the handler emits. The connection is kept
        open for as long as the caller iterates; exit the `with` block
        to close it.
        """
        return EventStream(
            self._http,
            "/icp/v1/events:stream",
            headers={"accept": "text/event-stream"},
        )

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

    def pay(
        self,
        transaction_id: str,
        payment: dict[str, Any],
        *,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Direct payment (no preceding `authorize` required)."""
        return self._call(
            "intent.pay",
            {"transaction_id": transaction_id, "payment": payment},
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    # ---- read-side ------------------------------------------------------

    def search(
        self,
        *,
        query: Optional[str] = None,
        filters: Optional[dict[str, str]] = None,
        limit: Optional[int] = None,
        cursor: Optional[str] = None,
        mandate_jws: Optional[str] = None,
    ) -> dict[str, Any]:
        """Catalog search. Read-only; typically does not require a mandate."""
        params: dict[str, Any] = {}
        if query is not None:
            params["query"] = query
        if filters is not None:
            params["filters"] = filters
        if limit is not None:
            params["limit"] = limit
        if cursor is not None:
            params["cursor"] = cursor
        return self._call("intent.search", params, mandate_jws=mandate_jws)

    def describe(
        self,
        *,
        product_id: Optional[str] = None,
        sku: Optional[str] = None,
        mandate_jws: Optional[str] = None,
    ) -> dict[str, Any]:
        """Fetch product detail by either `product_id` or `sku`."""
        if (product_id is None) == (sku is None):
            raise ValueError("pass exactly one of product_id or sku")
        params: dict[str, Any] = {}
        if product_id is not None:
            params["product_id"] = product_id
        if sku is not None:
            params["sku"] = sku
        return self._call("intent.describe", params, mandate_jws=mandate_jws)

    def track(
        self,
        transaction_id: str,
        *,
        mandate_jws: Optional[str] = None,
    ) -> dict[str, Any]:
        """Shipment + fulfillment status for a transaction."""
        return self._call(
            "intent.track",
            {"transaction_id": transaction_id},
            mandate_jws=mandate_jws,
        )

    # ---- post-sale ------------------------------------------------------

    def return_(
        self,
        transaction_id: str,
        items: Sequence[dict[str, Any]],
        *,
        reason: Optional[str] = None,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Initiate a return. Named `return_` (trailing underscore) because
        `return` is a Python reserved keyword."""
        params: dict[str, Any] = {
            "transaction_id": transaction_id,
            "items": list(items),
        }
        if reason is not None:
            params["reason"] = reason
        return self._call(
            "intent.return",
            params,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    def refund_request(
        self,
        transaction_id: str,
        *,
        amount: Optional[dict[str, Any]] = None,
        reason: Optional[str] = None,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Request a refund. Full refund if `amount` is omitted."""
        params: dict[str, Any] = {"transaction_id": transaction_id}
        if amount is not None:
            params["amount"] = amount
        if reason is not None:
            params["reason"] = reason
        return self._call(
            "intent.refund_request",
            params,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    def confirm_receipt(
        self,
        transaction_id: str,
        *,
        note: Optional[str] = None,
        mandate_jws: Optional[str] = None,
    ) -> dict[str, Any]:
        """Buyer's acknowledgement of physical receipt — the escrow-release
        trigger on A2A and stablecoin flows."""
        params: dict[str, Any] = {"transaction_id": transaction_id}
        if note is not None:
            params["note"] = note
        return self._call(
            "intent.confirm_receipt", params, mandate_jws=mandate_jws
        )

    def negotiate(
        self,
        transaction_id: str,
        *,
        proposed_total: Optional[dict[str, Any]] = None,
        discount_pct: Optional[float] = None,
        message: Optional[str] = None,
        mandate_jws: Optional[str] = None,
    ) -> dict[str, Any]:
        """Counter-offer on a quoted transaction. Provide exactly one of
        `proposed_total` (whole-basket override) or `discount_pct`
        (0.0–90.0)."""
        if (proposed_total is None) == (discount_pct is None):
            raise ValueError(
                "pass exactly one of proposed_total or discount_pct"
            )
        params: dict[str, Any] = {"transaction_id": transaction_id}
        if proposed_total is not None:
            params["proposed_total"] = proposed_total
        if discount_pct is not None:
            params["discount_pct"] = discount_pct
        if message is not None:
            params["message"] = message
        return self._call(
            "intent.negotiate", params, mandate_jws=mandate_jws
        )

    # ---- subscriptions --------------------------------------------------

    def subscribe(
        self,
        items: Sequence[dict[str, Any]],
        *,
        cadence: str,
        buyer: Optional[dict[str, Any]] = None,
        ship_to: Optional[dict[str, Any]] = None,
        payment: Optional[dict[str, Any]] = None,
        currency: str = "USD",
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Start a recurring subscription. `cadence` is one of `weekly`,
        `monthly`, `annual` (handler-defined enum)."""
        params: dict[str, Any] = {
            "items": list(items),
            "cadence": cadence,
            "currency": currency,
        }
        if buyer is not None:
            params["buyer"] = buyer
        if ship_to is not None:
            params["ship_to"] = ship_to
        if payment is not None:
            params["payment"] = payment
        return self._call(
            "intent.subscribe",
            params,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    def renew(
        self,
        subscription_id: str,
        *,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Force a renewal charge now, resetting the dunning failure counter."""
        return self._call(
            "intent.renew",
            {"subscription_id": subscription_id},
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    def pause(
        self,
        subscription_id: str,
        *,
        mandate_jws: Optional[str] = None,
    ) -> dict[str, Any]:
        """Pause auto-billing. Returns to `active` via a manual `renew`."""
        return self._call(
            "intent.pause",
            {"subscription_id": subscription_id},
            mandate_jws=mandate_jws,
        )

    def cancel_subscription(
        self,
        subscription_id: str,
        *,
        mandate_jws: Optional[str] = None,
    ) -> dict[str, Any]:
        """Terminal cancel — no further charges, no reactivation."""
        return self._call(
            "intent.cancel_subscription",
            {"subscription_id": subscription_id},
            mandate_jws=mandate_jws,
        )

    # ---- agent-to-agent -------------------------------------------------

    def a2a_quote(
        self,
        *,
        peer_agent_id: str,
        service: dict[str, Any],
        price_hint: Optional[dict[str, Any]] = None,
        expires_in_secs: Optional[int] = None,
        reference_id: Optional[str] = None,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Ask a peer agent for a quote on a service. `service` is typed
        by `A2aServiceKind` (`compute` / `data_feed` / `image_generation`
        / `ad_hoc`) plus free-form params."""
        params: dict[str, Any] = {
            "peer_agent_id": peer_agent_id,
            "service": service,
        }
        if price_hint is not None:
            params["price_hint"] = price_hint
        if expires_in_secs is not None:
            # Server field is `expires_in_seconds` (src/models.rs); the old
            # `expires_in_secs` key was silently ignored, defaulting to 300s.
            params["expires_in_seconds"] = expires_in_secs
        if reference_id is not None:
            params["reference_id"] = reference_id
        return self._call(
            "intent.a2a_quote",
            params,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    def a2a_pay(
        self,
        *,
        from_wallet: str,
        peer_quote_id: Optional[str] = None,
        peer_agent_id: Optional[str] = None,
        amount: Optional[dict[str, Any]] = None,
        memo: Optional[str] = None,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Pay a peer. Two shapes:

        * Pay against a prior quote: pass `peer_quote_id` only.
        * Direct payment: pass `peer_agent_id` + `amount`.
        """
        if peer_quote_id is not None and (peer_agent_id or amount):
            raise ValueError(
                "pay-against-quote: pass only peer_quote_id (not peer_agent_id / amount)"
            )
        if peer_quote_id is None and (peer_agent_id is None or amount is None):
            raise ValueError(
                "direct-pay requires peer_agent_id and amount"
            )
        params: dict[str, Any] = {"from": from_wallet}
        if peer_quote_id is not None:
            params["peer_quote_id"] = peer_quote_id
        if peer_agent_id is not None:
            params["peer_agent_id"] = peer_agent_id
        if amount is not None:
            params["amount"] = amount
        if memo is not None:
            params["memo"] = memo
        return self._call(
            "intent.a2a_pay",
            params,
            mandate_jws=mandate_jws,
            idempotency_key=idempotency_key,
        )

    # ---- shared envelope builder ---------------------------------------

    def _call(
        self,
        intent: str,
        params: dict[str, Any],
        *,
        mandate_jws: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> dict[str, Any]:
        """Build the standard IntentEnvelope and submit."""
        envelope: dict[str, Any] = {
            "intent": intent,
            "agent_id": self._agent_id,
            "params": params,
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
