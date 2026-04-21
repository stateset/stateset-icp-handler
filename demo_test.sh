#!/usr/bin/env bash
# End-to-end demo of the ICP handler: discovery → quote → authorize → buy.
#
# Usage:
#   ./demo_test.sh                  (against http://localhost:8082 with demo keys)
#   ICP_URL=https://... ./demo_test.sh
set -euo pipefail

URL="${ICP_URL:-http://localhost:8082}"
KEY="${ICP_API_KEY:-icp_demo_key_123}"
AGENT="${ICP_AGENT_ID:-did:stateset:agent:demo-alice}"

green() { printf "\033[32m%s\033[0m\n" "$*"; }
dim()   { printf "\033[2m%s\033[0m\n" "$*"; }
hr()    { printf -- '-%.0s' {1..70}; printf '\n'; }

hr; green "1. Discovery"
curl -fsS "${URL}/.well-known/icp" | jq '{icp_version, intents: (.intents | length), currencies, signing_keys: (.signing_keys | length)}'

hr; green "2. Quote"
QUOTE_RESP="$(curl -fsS -X POST "${URL}/icp/v1/intents" \
  -H "Authorization: Bearer ${KEY}" \
  -H "ICP-Agent-Id: ${AGENT}" \
  -H "Content-Type: application/json" \
  -d '{
    "intent": "intent.quote",
    "agent_id": "'"${AGENT}"'",
    "params": {
      "items": [
        { "sku": "WIDGET-001", "quantity": 2,
          "unit_price_hint": { "amount_minor": 2999, "currency": "USD" } }
      ],
      "buyer": { "first_name": "Alice", "last_name": "Smith", "email": "alice@example.com" },
      "ship_to": { "name": "Alice Smith", "line_one": "1 Market St",
                   "city": "San Francisco", "state": "CA",
                   "postal_code": "94105", "country": "US" }
    },
    "context": { "currency": "USD", "jurisdiction": "US-CA" }
  }')"

echo "$QUOTE_RESP" | jq '{transaction: .transaction | {id, state, totals}, receipt: .receipt | {jti, kid}}'

TXN_ID="$(echo "$QUOTE_RESP" | jq -r '.transaction.id')"
dim  "  transaction_id=${TXN_ID}"

hr; green "3. Authorize"
curl -fsS -X POST "${URL}/icp/v1/intents" \
  -H "Authorization: Bearer ${KEY}" \
  -H "ICP-Agent-Id: ${AGENT}" \
  -H "Content-Type: application/json" \
  -d "{
    \"intent\": \"intent.authorize\",
    \"agent_id\": \"${AGENT}\",
    \"params\": { \"transaction_id\": \"${TXN_ID}\" }
  }" | jq '.transaction | {id, state, totals}'

hr; green "4. Buy"
BUY_RESP="$(curl -fsS -X POST "${URL}/icp/v1/intents" \
  -H "Authorization: Bearer ${KEY}" \
  -H "ICP-Agent-Id: ${AGENT}" \
  -H "Content-Type: application/json" \
  -d "{
    \"intent\": \"intent.buy\",
    \"agent_id\": \"${AGENT}\",
    \"params\": {
      \"transaction_id\": \"${TXN_ID}\",
      \"payment\": {
        \"method\": \"card\",
        \"token\": \"tok_demo\",
        \"last_digits\": \"4242\",
        \"brand\": \"visa\"
      }
    }
  }")"
echo "$BUY_RESP" | jq '{order, transaction: .transaction | {id, state}, receipt: .receipt | {jti, kid, jws: (.jws | .[0:40] + "...")}}'

RCPT_JTI="$(echo "$BUY_RESP" | jq -r '.receipt.jti')"

hr; green "5. Retrieve receipt"
curl -fsS "${URL}/icp/v1/receipts/${RCPT_JTI}" \
  -H "Authorization: Bearer ${KEY}" \
  -H "ICP-Agent-Id: ${AGENT}" | jq '{jti, kid, body_digest}'

hr; green "Done."
