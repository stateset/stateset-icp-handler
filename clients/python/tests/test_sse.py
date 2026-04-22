"""Tests for the SSE event-stream wrapper.

Uses `httpx.MockTransport` to return a canned `text/event-stream` body,
then iterates through it via `Client.events()` and asserts the parsed
`SseEvent` shape. Runs offline.
"""

from __future__ import annotations

import httpx

from stateset_icp import Client, EventStream, IcpError, SseEvent
from stateset_icp.client import _parse_sse


def _mock_client(handler) -> Client:
    return Client(
        "http://handler.example",
        api_key="k",
        agent_id="did:stateset:agent:test",
        transport=httpx.MockTransport(handler),
    )


def _sse_body(lines: list[str]) -> bytes:
    """Serialize to the on-wire SSE format (CRLF or LF separators both OK,
    but we use LF — httpx.iter_lines handles both)."""
    return ("\n".join(lines) + "\n").encode("utf-8")


# ------------------------------------------------------------------
# Low-level parser
# ------------------------------------------------------------------


def test_parser_dispatches_on_blank_line():
    events = list(
        _parse_sse(
            iter(
                [
                    "id: 1",
                    "event: transaction.quoted",
                    'data: {"transaction_id": "txn_7"}',
                    "",
                    "id: 2",
                    "event: subscription.renewed",
                    'data: {"subscription_id": "sub_3"}',
                    "",
                ]
            )
        )
    )
    assert len(events) == 2
    assert events[0].id == "1"
    assert events[0].type == "transaction.quoted"
    assert events[0].data == {"transaction_id": "txn_7"}
    assert events[1].type == "subscription.renewed"


def test_parser_concatenates_multiline_data():
    events = list(
        _parse_sse(
            iter(
                [
                    "event: bulk.message",
                    "data: first line",
                    "data: second line",
                    "",
                ]
            )
        )
    )
    assert len(events) == 1
    assert events[0].raw == "first line\nsecond line"
    # Not JSON → data is None, raw preserves everything.
    assert events[0].data is None


def test_parser_discards_keepalive_comments():
    events = list(
        _parse_sse(
            iter(
                [
                    ": keep-alive",
                    ":",
                    "event: ping",
                    "data: pong",
                    "",
                ]
            )
        )
    )
    assert len(events) == 1
    assert events[0].type == "ping"


def test_parser_strips_single_leading_space_per_spec():
    events = list(
        _parse_sse(
            iter(
                [
                    # Per HTML Living Standard §9.2.6: a single leading
                    # space after the colon is stripped; additional
                    # spaces are preserved.
                    'data:  two-space-preserved',
                    "",
                ]
            )
        )
    )
    assert events[0].raw == " two-space-preserved"


def test_parser_ignores_unknown_fields():
    events = list(
        _parse_sse(
            iter(
                [
                    "retry: 5000",
                    "event: foo",
                    "unknown: whatever",
                    "data: ok",
                    "",
                ]
            )
        )
    )
    assert len(events) == 1
    assert events[0].type == "foo"
    assert events[0].raw == "ok"


def test_parser_non_json_data_stays_raw():
    events = list(
        _parse_sse(
            iter(
                [
                    "event: plain",
                    "data: not json at all",
                    "",
                ]
            )
        )
    )
    assert events[0].data is None
    assert events[0].raw == "not json at all"


# ------------------------------------------------------------------
# End-to-end via MockTransport
# ------------------------------------------------------------------


def test_events_yields_parsed_events_end_to_end():
    body = _sse_body(
        [
            "id: evt_1",
            "event: transaction.quoted",
            'data: {"transaction_id": "txn_1", "state": "quoted"}',
            "",
            "id: evt_2",
            "event: transaction.completed",
            'data: {"transaction_id": "txn_1", "state": "completed"}',
            "",
        ]
    )

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/icp/v1/events:stream"
        assert request.headers["accept"] == "text/event-stream"
        assert request.headers["authorization"].startswith("Bearer ")
        return httpx.Response(
            200,
            content=body,
            headers={"content-type": "text/event-stream"},
        )

    seen: list[SseEvent] = []
    with _mock_client(handler) as icp:
        with icp.events() as stream:
            assert isinstance(stream, EventStream)
            for event in stream:
                seen.append(event)

    assert [e.type for e in seen] == [
        "transaction.quoted",
        "transaction.completed",
    ]
    assert seen[0].data == {"transaction_id": "txn_1", "state": "quoted"}


def test_events_raises_icp_error_on_4xx():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            401,
            json={"error": {"type": "auth", "code": "auth_failed", "message": "nope"}},
        )

    with _mock_client(handler) as icp:
        try:
            with icp.events() as stream:
                for _ in stream:
                    pass
        except IcpError as e:
            assert e.status_code == 401
            assert e.code == "auth_failed"
        else:
            raise AssertionError("expected IcpError for 401")


def test_events_must_be_used_as_context_manager():
    """Iterating without entering the context raises — prevents users
    from leaking an open HTTP connection via a for-loop that forgets
    the `with` block."""
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=b"\n")

    with _mock_client(handler) as icp:
        stream = icp.events()
        try:
            for _ in stream:
                pass
        except RuntimeError as e:
            assert "context manager" in str(e)
        else:
            raise AssertionError("expected RuntimeError for unmanaged iteration")


def test_events_context_closes_even_on_iteration_break():
    """If the caller breaks out of the loop early, the HTTP stream still
    cleans up (no ResourceWarning / leaked socket)."""
    body = _sse_body(
        [
            "event: one",
            "data: {}",
            "",
            "event: two",
            "data: {}",
            "",
            "event: three",
            "data: {}",
            "",
        ]
    )

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200, content=body, headers={"content-type": "text/event-stream"}
        )

    with _mock_client(handler) as icp:
        with icp.events() as stream:
            for event in stream:
                if event.type == "one":
                    break
        # Exiting the `with icp.events()` block must not raise.
