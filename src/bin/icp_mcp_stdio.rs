//! `icp-mcp-stdio` — MCP server over stdin/stdout for desktop clients.
//!
//! Frames JSON-RPC 2.0 messages as line-delimited JSON on stdin/stdout
//! so MCP clients that spawn subprocesses (Claude Desktop, Cursor MCP
//! integration, etc.) can drive an ICP handler directly without HTTP.
//!
//! All commerce execution flows through the same `IcpService` that the
//! HTTP binary uses, so a stdio session and an HTTP session share the
//! same transaction store, signer, and (when configured) embedded
//! engine database.
//!
//! ## Configuration
//!
//! Pass tenant credentials via flags:
//!
//! ```text
//! icp-mcp-stdio \
//!     --api-key icp_demo_key_123 \
//!     --agent-id did:stateset:agent:claude-desktop \
//!     --commerce-db /var/lib/icp/commerce.db
//! ```
//!
//! ## Logging
//!
//! Logs go to **stderr only**. stdout is reserved for JSON-RPC framing.
//! Setting `LOG_LEVEL=debug` is safe and won't corrupt the protocol.
//!
//! ## Claude Desktop integration
//!
//! Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "icp": {
//!       "command": "/usr/local/bin/icp-mcp-stdio",
//!       "args": ["--api-key", "icp_demo_key_123",
//!                "--agent-id", "did:stateset:agent:claude-desktop"]
//!     }
//!   }
//! }
//! ```

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use stateset_icp_handler::{
    build_app_state,
    config::Config,
    mcp,
    mcp::{JsonRpcRequest, JsonRpcResponse},
    AppState,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, error, info, warn};

// --------------------------------------------------------------------------
// CLI
// --------------------------------------------------------------------------

struct Args {
    api_key: String,
    agent_id: String,
    commerce_db: Option<String>,
    require_mandate: bool,
    verify_signatures: bool,
}

fn print_help() {
    eprintln!(
        r#"icp-mcp-stdio — MCP server for desktop clients (Claude Desktop, Cursor, …)

USAGE:
    icp-mcp-stdio --api-key <KEY> --agent-id <DID> [options]

REQUIRED:
    --api-key <KEY>          Tenant bearer key the MCP client should appear to use
    --agent-id <DID>         Agent identifier (DID) for receipts and event correlation

OPTIONAL:
    --commerce-db <PATH>     Embedded iCommerce SQLite path (default: in-memory)
    --require-mandate        Reject scope-gated tool calls without an ICP-Mandate
                             (default: off, fine for local development)
    --verify-signatures      Cryptographically verify mandate signatures
                             against resolved principal DIDs
    --help, -h               Show this help

Speaks JSON-RPC 2.0 over stdin/stdout, line-delimited. Logs to stderr."#
    );
}

fn parse_args() -> Result<Args, String> {
    let mut api_key = None;
    let mut agent_id = None;
    let mut commerce_db = None;
    let mut require_mandate = false;
    let mut verify_signatures = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--api-key" => api_key = it.next(),
            "--agent-id" => agent_id = it.next(),
            "--commerce-db" => commerce_db = it.next(),
            "--require-mandate" => require_mandate = true,
            "--verify-signatures" => verify_signatures = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Args {
        api_key: api_key.ok_or("--api-key required")?,
        agent_id: agent_id.ok_or("--agent-id required")?,
        commerce_db,
        require_mandate,
        verify_signatures,
    })
}

// --------------------------------------------------------------------------
// State
// --------------------------------------------------------------------------

async fn build_state(args: &Args) -> anyhow::Result<AppState> {
    let mut config = Config::for_test(); // permissive defaults
    config.require_mandate = args.require_mandate;
    config.verify_mandate_signatures = args.verify_signatures;
    if let Some(path) = &args.commerce_db {
        config.commerce_enabled = true;
        config.commerce_db_path = path.clone();
    } else {
        config.commerce_enabled = false;
    }
    // Pre-seed an inline single-key JSON config so `build_app_state`
    // doesn't log its empty-keystore warning. We immediately overwrite
    // the keystore below; this just shapes the warning out.
    config.api_keys_json = Some(format!(
        r#"[{{"key":"{}","tenant_id":"mcp-{}","name":"icp-mcp-stdio"}}]"#,
        args.api_key.replace('"', "\\\""),
        args.agent_id.replace(':', "-").replace('"', "\\\""),
    ));
    config.enable_demo_keys = false;
    build_app_state(&config).await
}

// --------------------------------------------------------------------------
// Stdio loop
// --------------------------------------------------------------------------

#[tokio::main]
async fn main() -> ExitCode {
    // Logging to stderr only — stdout is reserved for JSON-RPC frames.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            return ExitCode::from(2);
        }
    };

    let state = match build_state(&args).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!("failed to build state: {e}");
            return ExitCode::from(1);
        }
    };

    info!(
        "icp-mcp-stdio ready; api_key=***{} agent_id={} engine={}",
        args.api_key
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>(),
        args.agent_id,
        if args.commerce_db.is_some() {
            "enabled"
        } else {
            "disabled"
        },
    );

    // Synthesize the headers MCP::dispatch expects. They never change
    // for the lifetime of this process.
    let headers = synth_headers(&args);

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = std::io::stdout().lock();

    while let Some(line) = match reader.next_line().await {
        Ok(maybe) => maybe,
        Err(e) => {
            error!("stdin read error: {e}");
            return ExitCode::from(1);
        }
    } {
        if line.trim().is_empty() {
            continue;
        }
        debug!(?line, "stdin frame");

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!("invalid JSON-RPC frame: {e}");
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: None,
                    result: None,
                    error: Some(stateset_icp_handler::mcp::JsonRpcError {
                        code: -32700,
                        message: format!("parse error: {e}"),
                        data: None,
                    }),
                };
                write_frame(&mut stdout, &resp);
                continue;
            }
        };

        let resp = mcp::dispatch(state.as_ref(), &headers, req).await;
        write_frame(&mut stdout, &resp);
    }

    info!("stdin closed; shutting down");
    ExitCode::SUCCESS
}

fn synth_headers(args: &Args) -> http::HeaderMap {
    let mut h = http::HeaderMap::new();
    h.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", args.api_key).parse().unwrap(),
    );
    if let Ok(v) = args.agent_id.parse() {
        h.insert("icp-agent-id", v);
    }
    h
}

fn write_frame(stdout: &mut std::io::StdoutLock<'_>, resp: &JsonRpcResponse) {
    let json = match serde_json::to_string(resp) {
        Ok(s) => s,
        Err(e) => {
            error!("serialize response: {e}");
            return;
        }
    };
    if let Err(e) = writeln!(stdout, "{json}") {
        error!("stdout write error: {e}");
    }
    let _ = stdout.flush();
}
