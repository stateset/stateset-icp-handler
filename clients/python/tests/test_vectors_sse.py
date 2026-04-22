"""Cross-language SSE-parser interop test.

Loads the shared fixture at ``docs/specification/vectors/sse_events.json``
and asserts the Python text/event-stream parser (in
``stateset_icp.client._parse_sse``) produces the expected ``SseEvent``
sequence for each case.

Paired with the equivalent Go test in
``clients/go/stateset-icp-go/vectors_sse_test.go`` — together they pin
the on-wire framing across languages. If either parser drifts, its
fixture assertion breaks and the drift surfaces at test time rather
than in production.
"""

from __future__ import annotations

import json
from pathlib import Path

from stateset_icp.client import _parse_sse

REPO_ROOT = Path(__file__).resolve().parents[3]
VECTORS = REPO_ROOT / "docs" / "specification" / "vectors"


def _load() -> dict:
    return json.loads((VECTORS / "sse_events.json").read_text())


def _run(lines: list[str]):
    """Drive ``_parse_sse`` with the concrete line stream the wire
    would produce. The parser's input contract is an iterator of
    lines (no trailing newlines) — same shape httpx's ``iter_lines``
    yields, so this mirrors the end-to-end runtime path exactly."""
    return list(_parse_sse(iter(lines)))


def test_sse_vectors_match_reference():
    file = _load()
    assert file["vectors"], "sse_events.json has no vectors"

    for case in file["vectors"]:
        name = case["name"]
        events = _run(case["lines"])
        expected = case["expected_events"]

        assert len(events) == len(expected), (
            f"{name}: event count {len(events)} != expected {len(expected)}\n"
            f"  got:  {events}\n"
            f"  want: {expected}"
        )

        for i, (got, want) in enumerate(zip(events, expected)):
            # The fixture uses `""` as the canonical "unset" marker
            # for id/type (JSON has no distinction between absent and
            # empty-string-valued fields, and the Go parser zero-values
            # `""`). Python's parser uses None to mean the same thing;
            # normalize both to the empty-string form for comparison.
            want_id = want["id"] or None
            want_type = want["type"] or None
            got_id = got.id or None
            got_type = got.type or None
            assert got_id == want_id, (
                f"{name}[{i}].id = {got.id!r}, want {want['id']!r}"
            )
            assert got_type == want_type, (
                f"{name}[{i}].type = {got.type!r}, want {want_type!r}"
            )
            assert got.raw == want["raw"], (
                f"{name}[{i}].raw mismatch\n"
                f"  got:  {got.raw!r}\n"
                f"  want: {want['raw']!r}"
            )
            assert got.data == want["data"], (
                f"{name}[{i}].data mismatch\n"
                f"  got:  {got.data!r}\n"
                f"  want: {want['data']!r}"
            )
