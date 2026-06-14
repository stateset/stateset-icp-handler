"""Contract tests for the ergonomic client wrappers.

Uses `httpx.MockTransport` to capture every outbound request without
touching a real handler. Each test asserts three things about the
wrapper it exercises:

1. The right HTTP method + URL (spec compliance).
2. The right IntentEnvelope shape in the request body (no drift between
   the Python client and the Rust server's parsing).
3. Correct header propagation (mandate, idempotency).

If any intent wrapper changes shape in a way that would fail server-side
parsing, the corresponding test fails here — well before a live handler
would reject it with HTTP 400.
"""

from __future__ import annotations

import json
from typing import Any

import httpx

from stateset_icp import Client


DEMO_URL = "http://handler.example"
DEMO_AGENT = "did:stateset:agent:test"


def _mock_client(handler_fn) -> Client:
    """Build a Client whose HTTP transport is backed by `handler_fn`.

    `handler_fn(httpx.Request) -> httpx.Response` — runs synchronously
    in-process on every outbound call, so the test can inspect the
    request shape and return a canned response.
    """
    transport = httpx.MockTransport(handler_fn)
    return Client(
        DEMO_URL,
        api_key="test-key",
        agent_id=DEMO_AGENT,
        transport=transport,
    )


def _capture(store: list, status: int = 200, body: dict | None = None):
    """Return an httpx handler that records requests into `store`."""

    def handler(request: httpx.Request) -> httpx.Response:
        body_json = None
        if request.content:
            body_json = json.loads(request.content.decode("utf-8"))
        store.append(
            {
                "method": request.method,
                "url": str(request.url),
                "headers": {k.lower(): v for k, v in request.headers.items()},
                "body": body_json,
            }
        )
        return httpx.Response(status, json=body or {"ok": True})

    return handler


# ------------------------------------------------------------------
# Read-side endpoints
# ------------------------------------------------------------------


def test_discovery_hits_well_known():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.discovery()
    assert reqs[0]["method"] == "GET"
    assert reqs[0]["url"].endswith("/.well-known/icp")


def test_jwks_hits_jwks_path():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.jwks()
    assert reqs[0]["url"].endswith("/.well-known/icp/jwks.json")


def test_get_helpers_hit_their_paths():
    for method, arg, expect in [
        ("get_transaction", "txn_1", "/icp/v1/transactions/txn_1"),
        ("get_subscription", "sub_1", "/icp/v1/subscriptions/sub_1"),
        ("get_peer_quote", "pq_1", "/icp/v1/peer_quotes/pq_1"),
        ("get_receipt", "rec_1", "/icp/v1/receipts/rec_1"),
        ("get_mandate_usage", "mx", "/icp/v1/mandates/mx/usage"),
    ]:
        reqs: list = []
        with _mock_client(_capture(reqs)) as icp:
            getattr(icp, method)(arg)
        assert reqs[0]["method"] == "GET", method
        assert reqs[0]["url"].endswith(expect), method


# ------------------------------------------------------------------
# Buy lifecycle
# ------------------------------------------------------------------


def _post_intent(reqs: list) -> dict[str, Any]:
    """The single POST /icp/v1/intents envelope from a captured run."""
    posts = [r for r in reqs if r["method"] == "POST"]
    assert len(posts) == 1, f"expected 1 POST, got {len(posts)}"
    assert posts[0]["url"].endswith("/icp/v1/intents")
    return posts[0]


def test_quote_envelope_shape():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.quote(
            items=[{"sku": "WIDGET", "quantity": 2}],
            buyer={"first_name": "Alice"},
            ship_to={"city": "SF"},
            currency="USD",
            jurisdiction="US-CA",
            mandate_jws="jws.here",
            idempotency_key="idem-1",
        )
    req = _post_intent(reqs)
    assert req["body"]["intent"] == "intent.quote"
    assert req["body"]["agent_id"] == DEMO_AGENT
    assert req["body"]["params"]["items"] == [{"sku": "WIDGET", "quantity": 2}]
    assert req["body"]["params"]["buyer"] == {"first_name": "Alice"}
    assert req["body"]["params"]["ship_to"] == {"city": "SF"}
    assert req["body"]["context"] == {"currency": "USD", "jurisdiction": "US-CA"}
    assert req["headers"]["icp-mandate"] == "jws.here"
    assert req["headers"]["icp-idempotency-key"] == "idem-1"


def test_authorize_and_buy_reference_the_same_transaction_id():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.authorize("txn_42")
        icp.buy("txn_42", payment={"method": "card", "token": "tok"})
    envelopes = [r["body"] for r in reqs if r["method"] == "POST"]
    assert envelopes[0]["intent"] == "intent.authorize"
    assert envelopes[0]["params"] == {"transaction_id": "txn_42"}
    assert envelopes[1]["intent"] == "intent.buy"
    assert envelopes[1]["params"]["transaction_id"] == "txn_42"
    assert envelopes[1]["params"]["payment"]["token"] == "tok"


def test_pay_builds_direct_payment_envelope():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.pay("txn_7", payment={"method": "stablecoin", "asset": "USDC"})
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.pay"
    assert body["params"]["transaction_id"] == "txn_7"
    assert body["params"]["payment"]["asset"] == "USDC"


# ------------------------------------------------------------------
# Read-side intents
# ------------------------------------------------------------------


def test_search_includes_only_provided_fields():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.search(query="widget", limit=10)
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.search"
    assert body["params"] == {"query": "widget", "limit": 10}


def test_describe_requires_exactly_one_of_product_id_or_sku():
    # Neither -> error
    try:
        _mock_client(_capture([])).describe()
    except ValueError:
        pass
    else:
        raise AssertionError("describe() must require product_id or sku")

    # Both -> error
    try:
        _mock_client(_capture([])).describe(product_id="p1", sku="s1")
    except ValueError:
        pass
    else:
        raise AssertionError("describe(both) must raise")

    # One -> OK
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.describe(sku="WIDGET")
    assert _post_intent(reqs)["body"]["params"] == {"sku": "WIDGET"}


def test_track_envelope():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.track("txn_1")
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.track"
    assert body["params"] == {"transaction_id": "txn_1"}


# ------------------------------------------------------------------
# Post-sale
# ------------------------------------------------------------------


def test_return_envelope_and_keyword_collision_workaround():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.return_(
            "txn_1",
            items=[{"sku": "WIDGET", "quantity": 1}],
            reason="damaged",
        )
    body = _post_intent(reqs)["body"]
    # Wire intent name is exactly `intent.return` despite the Python
    # method name being `return_` (keyword collision workaround).
    assert body["intent"] == "intent.return"
    assert body["params"]["items"] == [{"sku": "WIDGET", "quantity": 1}]
    assert body["params"]["reason"] == "damaged"


def test_refund_request_with_and_without_amount():
    # Full refund
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.refund_request("txn_1")
    assert _post_intent(reqs)["body"]["params"] == {"transaction_id": "txn_1"}

    # Partial refund
    reqs2: list = []
    with _mock_client(_capture(reqs2)) as icp:
        icp.refund_request(
            "txn_1", amount={"amount_minor": 500, "currency": "USD"}
        )
    params = _post_intent(reqs2)["body"]["params"]
    assert params["amount"]["amount_minor"] == 500


def test_confirm_receipt_passes_optional_note():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.confirm_receipt("txn_1", note="received intact")
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.confirm_receipt"
    assert body["params"] == {
        "transaction_id": "txn_1",
        "note": "received intact",
    }


def test_negotiate_requires_exactly_one_of_proposed_or_discount():
    # Neither
    try:
        _mock_client(_capture([])).negotiate("txn_1")
    except ValueError:
        pass
    else:
        raise AssertionError("negotiate() must require proposed_total or discount_pct")

    # Both
    try:
        _mock_client(_capture([])).negotiate(
            "txn_1",
            proposed_total={"amount_minor": 100, "currency": "USD"},
            discount_pct=10.0,
        )
    except ValueError:
        pass
    else:
        raise AssertionError("negotiate(both) must raise")

    # Discount OK
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.negotiate("txn_1", discount_pct=15.0, message="take it or leave it")
    params = _post_intent(reqs)["body"]["params"]
    assert params["discount_pct"] == 15.0
    assert params["message"] == "take it or leave it"
    assert "proposed_total" not in params


# ------------------------------------------------------------------
# Subscriptions
# ------------------------------------------------------------------


def test_subscribe_envelope():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.subscribe(
            items=[{"sku": "COFFEE", "quantity": 1}],
            cadence="monthly",
            currency="USD",
            payment={"method": "card", "token": "tok"},
        )
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.subscribe"
    assert body["params"]["cadence"] == "monthly"
    assert body["params"]["items"] == [{"sku": "COFFEE", "quantity": 1}]
    assert body["params"]["payment"]["token"] == "tok"


def test_subscription_lifecycle_envelopes():
    for method, wire in [
        ("renew", "intent.renew"),
        ("pause", "intent.pause"),
        ("cancel_subscription", "intent.cancel_subscription"),
    ]:
        reqs: list = []
        with _mock_client(_capture(reqs)) as icp:
            getattr(icp, method)("sub_1")
        body = _post_intent(reqs)["body"]
        assert body["intent"] == wire, method
        assert body["params"] == {"subscription_id": "sub_1"}, method


# ------------------------------------------------------------------
# A2A
# ------------------------------------------------------------------


def test_a2a_quote_envelope():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.a2a_quote(
            peer_agent_id="did:stateset:agent:compute-provider",
            service={"kind": "compute", "params": {"gpus": 1}},
            price_hint={"amount_minor": 10_000, "currency": "USD"},
            expires_in_secs=300,
            reference_id="job-42",
        )
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.a2a_quote"
    assert body["params"]["peer_agent_id"].endswith("compute-provider")
    assert body["params"]["service"]["kind"] == "compute"
    # Wire field is `expires_in_seconds` (matches the server); the old
    # `expires_in_secs` key was silently dropped by the handler.
    assert body["params"]["expires_in_seconds"] == 300
    assert body["params"]["reference_id"] == "job-42"


def test_a2a_pay_against_quote():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.a2a_pay(from_wallet="0xabc", peer_quote_id="pq_1", memo="for job 42")
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.a2a_pay"
    assert body["params"]["from"] == "0xabc"
    assert body["params"]["peer_quote_id"] == "pq_1"
    assert body["params"]["memo"] == "for job 42"
    assert "peer_agent_id" not in body["params"]


def test_a2a_pay_direct():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.a2a_pay(
            from_wallet="0xabc",
            peer_agent_id="did:stateset:agent:p",
            amount={"amount_minor": 5_000, "currency": "USD"},
        )
    body = _post_intent(reqs)["body"]
    assert body["intent"] == "intent.a2a_pay"
    assert body["params"]["peer_agent_id"].endswith(":p")
    assert body["params"]["amount"]["amount_minor"] == 5_000


def test_a2a_pay_rejects_hybrid_shape():
    try:
        _mock_client(_capture([])).a2a_pay(
            from_wallet="0xabc",
            peer_quote_id="pq_1",
            peer_agent_id="did:x",
        )
    except ValueError:
        pass
    else:
        raise AssertionError(
            "a2a_pay(peer_quote_id + peer_agent_id) must raise — ambiguous intent"
        )


def test_a2a_pay_direct_requires_both_agent_and_amount():
    # peer_agent_id without amount
    try:
        _mock_client(_capture([])).a2a_pay(
            from_wallet="0xabc", peer_agent_id="did:x"
        )
    except ValueError:
        pass
    else:
        raise AssertionError("direct-pay without amount must raise")


# ------------------------------------------------------------------
# Headers
# ------------------------------------------------------------------


def test_mandate_and_idempotency_headers_propagate_through_any_wrapper():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        # Pick one of the new wrappers that supports both headers.
        icp.subscribe(
            items=[{"sku": "X", "quantity": 1}],
            cadence="weekly",
            mandate_jws="mandate.jws",
            idempotency_key="idem-abc",
        )
    h = reqs[0]["headers"]
    assert h["icp-mandate"] == "mandate.jws"
    assert h["icp-idempotency-key"] == "idem-abc"
    assert h["icp-agent-id"] == DEMO_AGENT
    assert h["authorization"].startswith("Bearer ")
    # Handlers require ICP-Version by default — every intent POST must send it.
    assert h["icp-version"] == "2026-04-21"


def test_every_post_carries_auto_generated_request_id():
    reqs: list = []
    with _mock_client(_capture(reqs)) as icp:
        icp.track("txn_1")
    assert reqs[0]["headers"]["icp-request-id"].startswith("req-")
