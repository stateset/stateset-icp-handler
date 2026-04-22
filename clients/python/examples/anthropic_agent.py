"""Claude driving an ICP merchant end-to-end via tool use.

Registers five ICP intents as Anthropic tools, then runs the standard
tool-use loop until the model emits `end_turn`. Every commerce call goes
through the handwritten Python client in this package — no bespoke glue
between Claude and the merchant.

This is the substrate test: if a Python developer can wire Claude into
a running StateSet ICP handler in under 200 lines (most of which is
tool-schema JSON), the "agent framework as a first-class consumer"
claim is real.

Run locally::

    # Terminal 1 — start the handler
    cd ../../..
    cargo run --release

    # Terminal 2 — install and run
    cd clients/python
    pip install -e . anthropic
    export ANTHROPIC_API_KEY=sk-ant-...
    python examples/anthropic_agent.py

By default the model receives a hard-coded prompt that walks the
buy lifecycle; override with `ICP_PROMPT="..."` to drive something
different.
"""

from __future__ import annotations

import json
import os
import sys

try:
    from anthropic import Anthropic  # type: ignore
except ImportError:
    sys.exit(
        "anthropic package not installed. Run `pip install anthropic` and try again."
    )

from stateset_icp import (
    Client,
    Ed25519KeyPair,
    IcpError,
    create_mandate_payload,
    sign_mandate,
)

# --------------------------------------------------------------------------
# Config
# --------------------------------------------------------------------------

ICP_URL = os.environ.get("ICP_URL", "http://localhost:8082")
API_KEY = os.environ.get("ICP_API_KEY", "icp_demo_key_123")
AGENT_ID = os.environ.get("ICP_AGENT_ID", "did:stateset:agent:claude-demo")

# `claude-sonnet-4-6` is a good default for tool use: fast enough for an
# interactive loop, cheap enough for a demo, and capable enough to run
# multi-step plans without prompting tricks. Swap for `claude-opus-4-7`
# if you want the tighter planning / fewer tool calls.
MODEL = os.environ.get("ANTHROPIC_MODEL", "claude-sonnet-4-6")

DEFAULT_PROMPT = (
    "I'm shopping for a friend. Please quote two WIDGET-001s at roughly "
    "$29.99 each, shipped to Alice Smith, 1 Market St, San Francisco, CA "
    "94105, US. If the quoted total looks reasonable, go ahead and "
    "authorize and buy it with a test card (method=card, token=tok_demo, "
    "last_digits=4242, brand=visa), then tell me the order id and receipt jti."
)
PROMPT = os.environ.get("ICP_PROMPT", DEFAULT_PROMPT)

# --------------------------------------------------------------------------
# Tool catalog
# --------------------------------------------------------------------------
#
# Five tools cover the lifecycle Claude needs to drive:
# search → quote → authorize → buy → track.
#
# Schemas kept minimal and directly mirror the Python client's kwargs so
# `dispatch()` can forward tool_input as `**kwargs` without translation.

TOOLS = [
    {
        "name": "icp_search",
        "description": (
            "Search the merchant's catalog. Use this first when the user "
            "asks about an item by name rather than SKU."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
            },
            "required": ["query"],
        },
    },
    {
        "name": "icp_quote",
        "description": (
            "Request a priced quote for a basket. `items` is an array of "
            "objects with `sku`, `quantity`, and optional "
            "`unit_price_hint: {amount_minor, currency}`. `buyer` and "
            "`ship_to` are optional objects."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {"type": "object"}},
                "buyer": {"type": "object"},
                "ship_to": {"type": "object"},
                "currency": {"type": "string", "default": "USD"},
            },
            "required": ["items"],
        },
    },
    {
        "name": "icp_authorize",
        "description": (
            "Authorize a transaction previously produced by icp_quote. "
            "Pass the transaction id from the quote's response."
        ),
        "input_schema": {
            "type": "object",
            "properties": {"transaction_id": {"type": "string"}},
            "required": ["transaction_id"],
        },
    },
    {
        "name": "icp_buy",
        "description": (
            "Capture payment and place the order. Call after icp_authorize. "
            "`payment` is an object with `method` (e.g. 'card'), `token`, "
            "`last_digits`, `brand`."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "transaction_id": {"type": "string"},
                "payment": {"type": "object"},
            },
            "required": ["transaction_id", "payment"],
        },
    },
    {
        "name": "icp_track",
        "description": "Check fulfillment + shipment status for a transaction.",
        "input_schema": {
            "type": "object",
            "properties": {"transaction_id": {"type": "string"}},
            "required": ["transaction_id"],
        },
    },
]


# --------------------------------------------------------------------------
# Tool dispatch
# --------------------------------------------------------------------------


def dispatch(icp: Client, mandate: str, name: str, tool_input: dict) -> dict:
    """Route a Claude tool-use call to the Python client.

    Mandate-bearing intents get the compact JWS attached; read-only ones
    go without. The return value is a plain dict that Claude sees as the
    tool_result content — keeps the surface small.
    """
    if name == "icp_search":
        return icp.search(**tool_input)
    if name == "icp_quote":
        return icp.quote(mandate_jws=mandate, **tool_input)
    if name == "icp_authorize":
        return icp.authorize(mandate_jws=mandate, **tool_input)
    if name == "icp_buy":
        return icp.buy(mandate_jws=mandate, **tool_input)
    if name == "icp_track":
        return icp.track(**tool_input)
    raise ValueError(f"unknown tool: {name}")


# --------------------------------------------------------------------------
# Conversation loop
# --------------------------------------------------------------------------


def run(prompt: str, icp: Client, mandate: str) -> None:
    anthropic = Anthropic()

    messages: list[dict] = [{"role": "user", "content": prompt}]
    print(f"\n[user] {prompt}\n")

    # Cap iterations defensively — a well-behaved agent converges in a
    # handful of turns; anything past this is usually a schema mismatch.
    for _ in range(20):
        response = anthropic.messages.create(
            model=MODEL,
            max_tokens=1024,
            tools=TOOLS,
            messages=messages,
        )

        # Echo whatever text + tool_use blocks the model emitted.
        for block in response.content:
            if block.type == "text" and block.text:
                print(f"[assistant] {block.text}")
            elif block.type == "tool_use":
                print(f"[tool_use] {block.name}({_compact(block.input)})")

        # Append the assistant's full content so the next turn sees it.
        messages.append({"role": "assistant", "content": response.content})

        if response.stop_reason != "tool_use":
            return

        # Run every tool_use block in order, build a single user message
        # with the matching tool_result blocks, and loop.
        tool_results: list[dict] = []
        for block in response.content:
            if block.type != "tool_use":
                continue
            try:
                result = dispatch(icp, mandate, block.name, dict(block.input))
                tool_results.append(
                    {
                        "type": "tool_result",
                        "tool_use_id": block.id,
                        "content": json.dumps(result, default=str),
                    }
                )
                print(f"[tool_result] {block.name} → ok")
            except IcpError as e:
                tool_results.append(
                    {
                        "type": "tool_result",
                        "tool_use_id": block.id,
                        "content": json.dumps(
                            {
                                "error": True,
                                "code": e.code,
                                "status": e.status_code,
                                "message": e.message,
                            }
                        ),
                        "is_error": True,
                    }
                )
                print(f"[tool_result] {block.name} → {e.code} ({e.status_code})")

        messages.append({"role": "user", "content": tool_results})

    print("(iteration cap reached — giving up)", file=sys.stderr)


def _compact(obj: dict) -> str:
    """One-line tool_input summary for the trace output."""
    s = json.dumps(obj, default=str)
    return s if len(s) < 120 else s[:117] + "..."


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def main() -> int:
    if not os.environ.get("ANTHROPIC_API_KEY"):
        print("ANTHROPIC_API_KEY is not set.", file=sys.stderr)
        return 1

    # Mint a fresh did:key + mandate covering the buy lifecycle. The
    # budget caps what Claude can spend across the whole conversation;
    # set high enough that the demo doesn't bounce off it, low enough
    # that a runaway agent cannot drain a real account.
    keypair = Ed25519KeyPair.generate()
    mandate = sign_mandate(
        create_mandate_payload(
            issuer=keypair.did,
            subject=AGENT_ID,
            scope=["quote", "authorize", "buy"],
            budget_currency="USD",
            budget_amount_minor=50_000,  # $500 ceiling
            merchants=["*"],
            valid_for_secs=600,
        ),
        keypair,
    )

    with Client(ICP_URL, api_key=API_KEY, agent_id=AGENT_ID) as icp:
        run(PROMPT, icp, mandate)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
