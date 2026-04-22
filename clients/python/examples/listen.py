"""Live SSE listener — tail the handler's event stream and print each
event to stdout.

Run locally::

    # Terminal 1: start the handler
    cd ../../..
    cargo run --release

    # Terminal 2: listen
    cd clients/python
    pip install -e .
    python examples/listen.py

    # Terminal 3: drive some events (e.g. run examples/buy_flow.py
    # repeatedly, or hit /icp/v1/intents with any write).

Pairs with `buy_flow.py`: one script produces events, this one consumes
them. Together they cover the "agent-as-merchant" and
"agent-as-observer" halves of an ICP-Full deployment.

Ctrl-C exits cleanly — the context manager closes the underlying HTTP
stream so no sockets leak.
"""

from __future__ import annotations

import os
import sys
from datetime import datetime

from stateset_icp import Client, IcpError, SseEvent

ICP_URL = os.environ.get("ICP_URL", "http://localhost:8082")
API_KEY = os.environ.get("ICP_API_KEY", "icp_demo_key_123")
AGENT_ID = os.environ.get("ICP_AGENT_ID", "did:stateset:agent:observer")

# Fixed-width columns so the output tails nicely in a terminal.
TYPE_WIDTH = 28


def _summary(event: SseEvent) -> str:
    """One-line summary of the most interesting fields in the payload.
    Picks the fields every ICP event type carries a variant of so this
    stays readable across all three event families."""
    d = event.data or {}
    parts: list[str] = []
    for key in (
        "transaction_id",
        "subscription_id",
        "peer_quote_id",
        "order_id",
        "agent_id",
    ):
        if key in d:
            parts.append(f"{key}={d[key]}")
    # Totals are nice when present.
    totals = d.get("totals") or {}
    total = totals.get("total") if isinstance(totals, dict) else None
    if isinstance(total, dict) and "amount_minor" in total:
        parts.append(
            f"total={total.get('currency', '?')} "
            f"{int(total['amount_minor']) / 100:.2f}"
        )
    return "  ".join(parts) if parts else ""


def main() -> int:
    print(f"connecting to {ICP_URL} as {AGENT_ID}")
    with Client(ICP_URL, api_key=API_KEY, agent_id=AGENT_ID) as icp:
        try:
            with icp.events() as stream:
                print(f"listening on {ICP_URL}/icp/v1/events:stream ... (ctrl-c to exit)")
                for event in stream:
                    ts = datetime.now().isoformat(timespec="seconds")
                    name = (event.type or "<untyped>").ljust(TYPE_WIDTH)
                    summary = _summary(event)
                    print(f"[{ts}] {name} {summary}")
        except KeyboardInterrupt:
            print("\nstopped.")
            return 0
        except IcpError as e:
            print(f"\nhandler rejected stream: {e}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
