# ICP error taxonomy

This page enumerates every error code a conforming handler may return. See
`ICP_SPEC.md` §12 for the normative top-level categories.

## Response shape

```json
{
  "error": {
    "type": "<category>",
    "code": "<specific_code>",
    "message": "Human-readable explanation.",
    "param": "$.jsonpath.to.offending.field",
    "intent_id": "int_01HYN...",
    "retriable": false,
    "docs_url": "https://docs.stateset.com/icp/errors/<specific_code>"
  }
}
```

## Codes

| Category (`type`) | HTTP | Code (`code`) | Meaning |
|---|---|---|---|
| `invalid_request` | 400 | `missing_field` | A required field in the request is absent. |
| `invalid_request` | 400 | `invalid_field` | A field is present but malformed. |
| `invalid_request` | 400 | `invalid_currency` | Currency code not recognized or not supported by this handler. |
| `invalid_request` | 400 | `invalid_jurisdiction` | Jurisdiction not supported by this handler. |
| `invalid_request` | 400 | `quote_expired` | The quoted transaction's `quote_expires_at` has passed. |
| `authentication_failed` | 401 | `missing_bearer` | `Authorization` header missing. |
| `authentication_failed` | 401 | `invalid_bearer` | Bearer token is not recognized. |
| `authentication_failed` | 401 | `missing_agent_id` | `ICP-Agent-Id` header missing. |
| `mandate_invalid` | 401 | `mandate_missing` | No `ICP-Mandate` header on a write intent. |
| `mandate_invalid` | 401 | `mandate_malformed` | Compact JWS structurally invalid. |
| `mandate_invalid` | 401 | `mandate_expired` | `now > mandate.exp`. |
| `mandate_invalid` | 401 | `mandate_not_yet_valid` | `now < mandate.nbf`. |
| `mandate_invalid` | 401 | `mandate_signature_invalid` | Signature did not verify against the principal's advertised key set. |
| `mandate_out_of_scope` | 403 | `scope_missing` | Intent's required scope is not in `mandate.icp.scope`. |
| `mandate_out_of_scope` | 403 | `merchant_not_allowed` | Mandate does not authorize the target merchant. |
| `mandate_out_of_scope` | 403 | `jurisdiction_not_allowed` | Mandate does not authorize the fulfillment jurisdiction. |
| `mandate_budget_exceeded` | 402 | `budget_exhausted` | Remaining budget is less than the intent amount. |
| `mandate_budget_exceeded` | 402 | `per_transaction_cap` | Intent amount exceeds `budget.per_transaction`. |
| `intent_not_supported` | 404 | `unknown_intent` | Intent is not in the handler's advertised catalog. |
| `intent_not_supported` | 404 | `extension_not_supported` | Extension intent referenced but not advertised. |
| `resource_not_found` | 404 | `transaction_not_found` | `transaction_id` does not exist. |
| `resource_not_found` | 404 | `order_not_found` | `order_id` does not exist. |
| `resource_not_found` | 404 | `receipt_not_found` | `receipt_jti` does not exist. |
| `conflict` | 409 | `idempotency_conflict` | Same `intent_id` submitted with a different body. |
| `precondition_failed` | 412 | `transaction_state` | Transaction state does not allow this intent (e.g. `intent.buy` on a `Completed` transaction). |
| `precondition_failed` | 412 | `payment_method_unsupported` | Payment method not in discovery. |
| `rate_limited` | 429 | `rate_limited` | Per-tenant or per-agent rate limit exceeded. |
| `engine_unavailable` | 503 | `engine_open_failed` | Embedded commerce engine failed to open. |
| `engine_unavailable` | 503 | `engine_write_failed` | Engine rejected a write (e.g. constraint violation). |
| `processing_error` | 500 | `signing_failed` | Receipt signing failed. |
| `processing_error` | 500 | `serialization_failed` | Canonicalization (JCS) failed. |

## When `retriable` is true

Only `rate_limited` and `engine_unavailable` return `retriable: true`.
Agents SHOULD apply exponential backoff starting at 500ms with jitter, up
to a configurable ceiling (the reference CLI uses 15s).

Everything else requires the agent to adjust its request (or its mandate)
before retrying.
