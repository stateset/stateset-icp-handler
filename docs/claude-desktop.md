# Using ICP from Claude Desktop (and other MCP clients)

The `icp-mcp-stdio` binary speaks JSON-RPC 2.0 over stdin/stdout, the
transport Claude Desktop and most desktop MCP clients use to spawn an
MCP server. Once configured, every implemented ICP intent appears as a
tool the model can call directly.

## 1. Build the binary

```bash
cd stateset-icp-handler
cargo build --release --bin icp-mcp-stdio
sudo install -m 755 target/release/icp-mcp-stdio /usr/local/bin/
```

## 2. Add it to Claude Desktop's config

Edit `~/Library/Application Support/Claude/claude_desktop_config.json`
on macOS (`%APPDATA%\Claude\claude_desktop_config.json` on Windows) and
add an entry under `mcpServers`:

```json
{
  "mcpServers": {
    "icp": {
      "command": "/usr/local/bin/icp-mcp-stdio",
      "args": [
        "--api-key", "icp_demo_key_123",
        "--agent-id", "did:stateset:agent:claude-desktop",
        "--commerce-db", "/Users/you/Library/Application Support/Claude/icp-commerce.db"
      ]
    }
  }
}
```

Restart Claude Desktop. You should see 9 new tools (`icp_search`,
`icp_quote`, `icp_buy`, `icp_track`, `icp_return`, …) available to the
assistant.

## 3. (Optional) Production hardening

For production use point at a real merchant API key and turn on
mandate enforcement so the model can't bypass spending limits:

```json
{
  "mcpServers": {
    "icp": {
      "command": "/usr/local/bin/icp-mcp-stdio",
      "args": [
        "--api-key", "${MERCHANT_KEY}",
        "--agent-id", "did:stateset:agent:claude-desktop-fleet-v1",
        "--commerce-db", "/var/lib/icp/commerce.db",
        "--require-mandate",
        "--verify-signatures"
      ]
    }
  }
}
```

With `--require-mandate` the assistant must present an `ICP-Mandate`
JWS to call any scope-gated intent. With `--verify-signatures` that
mandate's Ed25519 signature is verified against the principal's
resolved keyset (`did:key` and `did:web` supported out of the box).

## Logging

`icp-mcp-stdio` logs to **stderr** only — stdout is reserved for
JSON-RPC framing. Claude Desktop surfaces stderr in its server
panel; set `LOG_LEVEL=debug` in the env block to see every dispatch.

```json
"icp": {
  "command": "/usr/local/bin/icp-mcp-stdio",
  "args": ["--api-key", "...", "--agent-id", "..."],
  "env": { "LOG_LEVEL": "debug" }
}
```

## Sanity check

You can drive the binary manually with `printf` to verify the install:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"shell","version":"1.0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
| icp-mcp-stdio --api-key icp_demo_key_123 \
                 --agent-id did:stateset:agent:shell-test
```

You should see two response frames on stdout — one for `initialize`,
one for `tools/list` listing 9 tools.

## Uniform audit story

A request that reaches the handler over stdio, HTTP, gRPC, ACP compat,
or UCP compat all run through the *same* `IcpService::handle_intent`
pipeline. That means:

- Every state-changing tool call produces an Ed25519-signed receipt.
- Every receipt is retrievable by `jti` from the same store, regardless
  of which transport produced it.
- A merchant's audit trail is complete whether the agent talked to the
  handler over stdio (Claude Desktop), HTTP (their custom agent), or
  ACP (their ChatGPT integration).
