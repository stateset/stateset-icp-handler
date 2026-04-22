#!/usr/bin/env bash
# Run the `icp-conformance` harness against a locally-spawned handler.
#
# Usage:
#   ./demo_conformance.sh              # starts a fresh handler, tears it down
#   ICP_URL=https://... ./demo_conformance.sh   # points at an existing handler
#
# Exit code mirrors the conformance tool: 0 on all-pass, non-zero otherwise.
set -euo pipefail

EXTERNAL="${ICP_URL:-}"

if [[ -z "$EXTERNAL" ]]; then
  PORT="${PORT:-8092}"
  GRPC_PORT="${GRPC_PORT:-50062}"
  URL="http://127.0.0.1:${PORT}"
  DB="/tmp/icp_conformance_$$.db"

  echo "Booting handler on ${URL} (db=${DB})..."
  PORT="${PORT}" \
  GRPC_PORT="${GRPC_PORT}" \
  ICP_ENABLE_DEMO_KEYS=true \
  ICP_REQUIRE_MANDATE=false \
  COMMERCE_DB_PATH="${DB}" \
  LOG_LEVEL=warn \
    cargo run --quiet --release --bin stateset-icp-handler >/dev/null 2>&1 &
  SERVER_PID=$!
  trap 'kill $SERVER_PID 2>/dev/null || true; rm -f "${DB}" "${DB}"-*' EXIT

  for _ in $(seq 1 60); do
    if curl -fsS "${URL}/health" >/dev/null 2>&1; then break; fi
    sleep 1
  done
else
  URL="$EXTERNAL"
  echo "Targeting external handler: ${URL}"
fi

echo
cargo run --quiet --release --bin icp-conformance -- \
  --url "${URL}" \
  --api-key "${ICP_API_KEY:-icp_demo_key_123}" \
  --agent-id "${ICP_AGENT_ID:-did:stateset:agent:conformance}" \
  "$@"
