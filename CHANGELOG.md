# Changelog

All notable changes to the StateSet ICP Handler (Rust crate) are
documented here. The sibling client packages each carry their own
package-scoped changelog alongside their release notes on
npm / PyPI:

- Python — [`clients/python/CHANGELOG.md`](./clients/python/CHANGELOG.md)
- npm `@stateset/icp-conformance` — [`clients/npm/icp-conformance/CHANGELOG.md`](./clients/npm/icp-conformance/CHANGELOG.md)
- npm `@stateset/create-icp-commerce` — [`clients/npm/create-icp-commerce/CHANGELOG.md`](./clients/npm/create-icp-commerce/CHANGELOG.md)
- Go — [`clients/go/stateset-icp-go/CHANGELOG.md`](./clients/go/stateset-icp-go/CHANGELOG.md)

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to date-based ICP versioning — see
[`docs/specification/ICP_SPEC.md` §16](./docs/specification/ICP_SPEC.md#16-versioning).

## [Unreleased]

## [0.3.1] — 2026-04-23

### Changed
- Tenant list endpoints for transactions, subscriptions, and peer
  quotes now use SQLite metadata indexes (`tenant_id`, lifecycle
  state/status, `created_at`) instead of materializing every JSON
  payload in the state store. Legacy blank-metadata rows still fall
  back to the old JSON scan path when needed.
- Subscription scheduler status counts now aggregate from indexed
  SQLite status metadata when available, avoiding full JSON
  materialization on every metrics tick.
- Subscription renewal and quote-expiry workers now fetch due work from
  SQLite deadline indexes (`next_charge_at`, `quote_expires_at`,
  `expires_at`) instead of scanning every subscription, transaction,
  and peer quote on each background tick.
- Mandate budget checks and post-intent spend recording now surface
  durable-ledger backend failures as retriable engine errors instead
  of crashing the process or continuing with an unsafe empty ledger
  view.

### Fixed
- HTTP SSE event streams now apply the same tenant ownership filter as
  gRPC streams before serializing events. A caller subscribed to
  `/icp/v1/events:stream` only receives events whose transaction,
  subscription, or peer quote belongs to the caller's bearer tenant.
- API key JSON/file loading now fails fast on malformed credentials
  instead of silently accepting blank fields or duplicate bearer keys
  that shadow earlier entries.
- Corrupt JSON rows in the transaction/subscription/peer-quote store
  are logged and skipped rather than panicking request handlers or
  background workers.
- Idempotency and receipt stores now log backend, lock, and corrupt-row
  failures and return miss/empty results instead of panicking the
  handler process.
- Webhook subscriber and outbox stores now handle lock, pool, query,
  update, count, and prune failures without panicking request handlers
  or background delivery workers.

## [0.3.0] — 2026-04-22

### Added
- **Expiry event payloads — re-quote context** — completes the
  background-event enrichment story (subscription scheduler in the
  prior two iterations; expiry sweeper now). The previous payloads
  carried only `{id, expired_at}` — operators had to re-fetch to
  do anything actionable. Both events now carry triage metadata
  (back-compat — old receivers still work):
  - **`transaction.expired`** adds:
    - `previous_state` — Draft vs Quoted. Operators handle them
      differently: Quoted means a customer saw a price they didn't
      act on (re-quote); Draft means they never made it past
      discovery.
    - `quote_expires_at` — the *original* deadline (not the sweep
      tick time), for log-search scoping.
    - `buyer_email` — best-effort; lets customer-facing
      automation send a re-quote email without re-fetching.
    - `amount_minor` / `currency` — quoted total the customer
      saw, for re-quote email context (best-effort; null on
      Draft txns with no totals).
    - `next_action: "re_quote_required"` — stable enum operators
      switch on.
  - **`peer_quote.expired`** adds:
    - `previous_status` — Pending (peer never priced) vs Quoted
      (priced offer aged out). Different operator playbooks.
    - `requester_agent_id` / `peer_agent_id` — both parties on
      the payload so receivers don't re-fetch.
    - `service_kind` — what was being requested. A2A operator
      dashboards segment by this.
    - `price_amount_minor` / `price_currency` — the priced
      offer that aged out (null when previous_status was
      Pending). Lets re-quote workflows decide whether to
      re-issue at the same target.
    - `next_action: "re_quote_required"` — same enum as
      transaction.expired.
  - 2 new tests in `tests/expiry_sweeper.rs` (7 total now) —
    each event's payload assertion: build a realistic source
    artifact (Quoted txn with totals + buyer email; Quoted peer
    quote with price), drive the sweeper, drain events from the
    in-process bus, assert every new field is populated.

- **`subscription.renewed` event payload — cycle context** —
  symmetric enrichment to last iteration's `past_due` work.
  The original payload was just `{subscription_id, automatic}`
  — operator automation around successful renewals had to
  re-fetch the subscription and the transaction to do anything
  useful (mail a receipt, run cycle-end accounting, send a
  customer notification). Six new fields on the renewed payload
  (back-compat — old receivers still work):
  - **`transaction_id`** — duplicates the top-level event field
    in the payload so flat-receiver consumers (one webhook, no
    Event-envelope parser) can link to the signed receipt
    without parsing two layers.
  - **`cycle_number`** — the post-increment
    `charges_completed`. The Nth successful charge against this
    subscription. Drives "cycle 12 of 12" type messaging.
  - **`current_period_start`** / **`current_period_end`** —
    period bounds for the just-charged cycle. Cycle accounting
    needs both.
  - **`next_charge_at`** — when the next auto-renewal will
    fire. Customer-facing emails set expectations from this.
    Equal to `current_period_end` on a successful renewal — both
    fields are stamped for clarity (a future iteration could
    diverge them, e.g. proration or grace periods).
  - **`amount_minor`** / **`currency`** — taken from
    `txn.totals.total` (best-effort; may be null if the engine
    didn't compute totals on the renewal txn). Lets receipt
    mailers include the charged amount without re-fetching.
  - 1 new test in `tests/dunning.rs` (9 total now) —
    `renewed_event_payload_carries_cycle_context` subscribes to
    the in-process event bus, drives a working renewal via
    `tick_subscriptions`, and asserts every new field is
    populated. Specifically pins `cycle_number` to the
    post-increment value so a regression that emits the
    pre-increment count would fail loud.

- **`subscription.past_due` event payload — triage metadata** —
  the existing payload was just `{subscription_id,
  consecutive_failures}`. An operator paged by this had no way
  from the event alone to tell *why* the renewal failed
  (declined card vs network timeout vs gateway-side outage)
  or *when* the last attempt was. They had to dig through DB
  rows or warn-level logs to do anything actionable. The
  enriched payload adds three triage fields (back-compat — old
  receivers still work):
  - **`last_error`** — the underlying charge-failure message
    captured from the engine error (`err.to_string()`). The
    operator can switch on this to choose between an automated
    retry vs surfacing to a human, and surface it to the
    customer for self-service repair (e.g. "your card was
    declined; please update payment method"). Captured
    *before* `err` moves into the `tracing::warn!` call so the
    same value lands in both the structured log and the event.
  - **`last_attempt_at`** — RFC3339 timestamp of the failure
    that triggered past_due. Lets operators scope log searches
    and correlate with payment-gateway logs.
  - **`next_action`** — operator-facing string switch
    (`"manual_renewal_required"` for now). Stable enum value
    so handlers can route the event to the right runbook.
  - Added `attempts_made` as a forward-looking alias for
    `consecutive_failures` — both fields carry the same value
    so receivers can migrate at their own pace.
  - 1 new test in `tests/dunning.rs` (8 total now) —
    `past_due_event_payload_carries_triage_metadata` subscribes
    to the in-process event bus, drives a sub through dunning
    schedule exhaustion, and asserts every new field is
    populated. Fails loud if `last_error` is the empty string
    (the most common regression: forgetting to thread the
    error message through).

- **`GET /icp/v1/receipts` list endpoint** — completes the
  resource-list quartet (transactions + subscriptions + peer_quotes
  + receipts). Operators auditing past activity can now enumerate
  signed receipts through the API instead of needing every jti
  ahead of time. Same shape as the others:
  - Tenant-scoped — receipts don't carry a `tenant_id` of their own
    (the signed claims shape is wire-stable), so ownership is
    derived per-row via `claims.icp.transaction_id` →
    transaction lookup. Identical policy to the existing
    `GET /icp/v1/receipts/:jti`.
  - `?intent=intent.buy` filter — narrows to receipts signed for
    a specific intent. Useful for audit dashboards that segment
    by flow (refunds, subscriptions, peer payments, etc).
  - `?limit=` capped at 500 (default 100). Internally over-fetches
    by 3× so cross-tenant filtering still gives the caller close
    to a full page back.
  - Newest-first sort by `claims.iat` (signed-at timestamp);
    stable across both backends, unlike `created_at` which only
    the SQLite backend tracks.
  - Empty result → **200** `{count:0,data:[]}` (never 404 — same
    convention as the other list endpoints).
  - New `ReceiptStore::list_recent(limit)` operator-level scan.
    Cross-tenant by design; the route handler does the per-row
    join + tenant filter.
  - 4 new tests in `tests/receipt_isolation.rs` (8 total now):
    list returns only caller's rows; intent filter narrows to
    matching receipts; empty list is 200 not 404; unauthenticated
    list 401s.
  - Wired into `openapi.rs` paths registry and the openapi test's
    path enumeration.

- **`PATCH /icp/v1/webhook_subscribers/:id`** — closes the secret-
  rotation gap. Previously the only way to rotate an HMAC signing
  secret (a security-best-practice operation) was to delete +
  recreate, which **rotates the id**. That breaks any caller
  holding the existing id (the operator's own dashboard, automated
  config sync, etc). The new PATCH endpoint updates `url` and/or
  `secret` in place — id stays.
  - `UpdateSubscriberBody { url: Option<String>, secret: Option<String> }`.
    Per-field semantics: omitted (or `null`) → leave alone; supplied
    → update. Empty body `{}` is a 200 no-op (only `updated_at`
    advances — that's the audit signal).
  - Same validation as create: URL must be `http(s)://` if
    supplied; secret must be non-empty if supplied. Validation
    runs **before** the existence check so a malformed request
    gets the right error class regardless of whether the
    subscriber exists.
  - Tenant-scoped: cross-tenant ids 404 (parity with
    get/disable/enable/delete — existence never confirmed across
    tenants).
  - Response redacts the secret — the rotated secret is in the
    request, never the response. Same redaction policy as the
    other read endpoints.
  - New `SubscriberStore::patch(id, url, secret, now)` primitive
    on both backends. The SQLite path uses read-modify-write so
    the per-field "None means leave alone" semantics match
    in-memory exactly without writing five SQL UPDATE permutations.
  - Wired into `openapi.rs` paths + the new `UpdateSubscriberBody`
    schema. Openapi test extended: subscribers/{id} now declares
    GET + PATCH + DELETE; PATCH must declare a typed `requestBody`.
  - 5 new tests in `tests/webhook_subscribers.rs` (18 total now):
    PATCH rotates secret in place without changing id (events
    keep flowing under the new HMAC); PATCH updates URL with
    secret unchanged; empty body is a no-op 200 with refreshed
    `updated_at`; validation rejects empty/non-http URL and empty
    secret; cross-tenant PATCH returns 404.

- **`POST /icp/v1/webhook_subscribers/:id/enable`** — closes the
  inverse of the existing `disable` endpoint. Previously a
  disabled subscriber could only be re-activated by delete +
  recreate, which **rotates both the id and the secret** —
  disruptive when an operator just wants to bring a maintenance-
  window-disabled receiver back online without churning the
  verifier configuration on the downstream side.
  - Same shape as `disable`: tenant-scoped, 404 on cross-tenant
    (existence not leaked), returns the updated row with the
    secret redacted. Idempotent — calling `enable` on an
    already-active subscriber is a no-op 200.
  - Internal refactor: `disable_webhook_subscriber` and
    `enable_webhook_subscriber` now both delegate to a shared
    `set_webhook_subscriber_active(state, id, headers, active)`
    helper so the tenant-isolation logic lives in one place.
  - Wired into `openapi.rs` paths registry; the openapi test's
    path enumeration extended to cover the new route.
  - 3 new tests in `tests/webhook_subscribers.rs` (13 total now):
    enable re-activates a disabled subscriber and the same id +
    secret are preserved (events resume flowing); enable on
    already-active is an idempotent no-op; cross-tenant enable →
    404 (parity with `get`/`disable`/`delete`).

- **Quote-expiry sweeper** — closes a real consistency bug:
  `transaction.quote_expires_at` and `peer_quote.expires_at` were
  stored at quote creation but nothing previously enforced them.
  A customer could quote at price X, walk away, then come back
  hours/days later and authorize at the original price even though
  the merchant's pricing/inventory had moved on. Same risk on the
  A2A side: a peer quote with a stale price could be paid against
  long after the peer would still honor it. The sweeper transitions
  both kinds to the terminal `Expired` state once their deadline
  passes:
  - New `IcpService::tick_expiries(now) -> ExpiryTickReport`.
    Sweeps **transactions** with `quote_expires_at <= now` AND
    state ∈ `{Draft, Quoted}` (Authorized/Captured/Completed are
    **never** touched — the caller has moved past the quote-
    validity window). Sweeps **peer quotes** with
    `expires_at <= now` AND status ∈ `{Pending, Quoted}`.
    Each transition emits a `transaction.expired` /
    `peer_quote.expired` event onto the originating tenant's
    webhook outbox + SSE stream.
  - New `scheduler::run_expiry_loop(service, period)` sweeper
    spawned from `lib.rs::main_loop` alongside the subscription
    scheduler, webhook worker, and idempotency sweeper.
  - New config knob `ICP_EXPIRY_SWEEPER_INTERVAL_SECONDS`
    (default `60` — sub-minute resolution on quote validity).
    `Config::for_test()` defaults to `0` so tests drive
    `tick_expiries(now)` directly.
  - **Two new Prometheus series**:
    - `icp_expiries_total{kind}` — counter, `kind` ∈
      `{transaction, peer_quote}`. Sudden spike can indicate the
      authorization path is broken (legitimate users can't
      convert and quotes age out).
    - `icp_expiry_sweeper_ticks_total` — liveness counter
      (advances on no-op ticks too).
  - New `PeerQuoteStore::list_all(limit)` — operator-level scan
    across all tenants. The user-facing list endpoint stays
    tenant-scoped; the sweeper needs unrestricted view.
  - 5-test integration suite (`tests/expiry_sweeper.rs`):
    Draft/Quoted txns past expiry transition to Expired while
    fresh ones stay; Authorized/Completed/no-expiry txns are
    NEVER touched even when ancient (the critical safety
    property); same shape for Pending/Quoted peer quotes vs
    Accepted; expiry events fan out per-tenant to the originating
    tenant's outbox without leaking across tenants; second sweep
    after expiry is a true no-op (no double-emission of events);
    metrics path bumps liveness counter on zero-effect ticks too.

- **Idempotency cache TTL sweeper** — closes the same
  unbounded-growth gap on the second-largest table after the
  outbox. Lazy TTL at lookup time already prevents stale entries
  from being replayed (`now - created_at > ttl` → returns `Miss`),
  but the row was never reclaimed — so the `idempotency` table
  grew forever for any tenant generating fresh idempotency keys.
  After a few months of production traffic the table dominates
  the DB just like the outbox would have. The fix is a background
  sweeper that runs every hour:
  - New `IdempotencyStore::prune(now) -> usize` primitive on
    both backends. In-memory `retain` filter, SQLite
    `DELETE WHERE created_at < ?`. Returns the row count for
    metrics. Cutoff is `now - ttl` (default 24h).
  - New `idempotency::run_sweeper_loop(store, period)` — same
    shape as `webhook::run_loop` and `scheduler::run_loop`.
    Spawned alongside the other workers in
    `lib.rs::main_loop`.
  - New config knob `ICP_IDEMPOTENCY_SWEEPER_INTERVAL_SECONDS`
    (default `3600`). `Config::for_test()` defaults to `0` so
    the background sweeper never races against test clocks —
    tests drive `prune(now)` directly.
  - **Two new Prometheus series**:
    - `icp_idempotency_pruned_total` — counter of rows reclaimed
      across the sweeper's lifetime. Sustained nonzero rate is
      the signal that retention is doing its job.
    - `icp_idempotency_sweeper_ticks_total` — liveness counter,
      bumped on every tick including no-op ticks. Alert on
      rate-of-change dropping (sweeper stuck).
  - 5-test integration suite (`tests/idempotency_sweeper.rs`):
    expired entries on both backends are deleted while in-TTL
    entries stay; no-op when nothing has expired; after a sweep,
    re-using the pruned key with a *different* body returns
    `Miss` (lets the tenant store fresh content) instead of
    leaving a hidden row that could trip future replays;
    `record_idempotency_sweep` bumps both counters and the
    liveness counter ticks even on zero-prune sweeps.

- **Webhook outbox retention sweep** — closes a real production
  durability gap: without TTL-based pruning the outbox grows
  unbounded, since every successful delivery and every dead-lettered
  row stays forever. After a few months of production traffic the
  table dominates the DB and the queue-depth gauge becomes
  meaningless. The fix runs a sweep on every worker tick:
  - Two new config knobs:
    - `ICP_WEBHOOK_RETAIN_DELIVERED_DAYS` (default `7`) — how long
      to keep `delivered` rows. Successful deliveries are
      historical only; no retry path uses them.
    - `ICP_WEBHOOK_RETAIN_DEAD_LETTERED_DAYS` (default `30`) —
      how long to keep `dead_lettered` rows. Longer than
      delivered because operators may still want to inspect a
      dead destination and trigger a manual `retry`.
    - Setting either to `0` disables that side of the sweep.
      `Config::for_test()` defaults to `0/0` so existing webhook
      tests pass byte-for-byte without churn.
  - **Pending, in_flight, and failed rows are NEVER pruned** —
    the worker still owns those and pruning them would lose
    work. Only `delivered` and `dead_lettered` rows have stable
    "this is operationally final" semantics.
  - `WebhookOutbox::prune(delivered_cutoff, dead_lettered_cutoff)`
    primitive on both backends (in-memory `retain` filter, SQLite
    `DELETE WHERE status = ? AND created_at < ?`). `WebhookWorker`
    gains `with_retention(delivered_days, dead_lettered_days)`
    and a `prune_now(now)` method that the run loop calls after
    every `tick()`. Cutoffs are computed against `created_at`
    (when the row entered the outbox), not `updated_at` —
    operator-meaningful retention is "how long has this row
    existed."
  - New Prometheus counter
    **`icp_webhook_outbox_pruned_total{reason}`** (`reason` ∈
    `{delivered, dead_lettered}`) — operators dashboard the rate
    to see the system actively bounding the outbox. A sustained
    nonzero rate means retention is doing its job.
  - 7-test integration suite (`tests/webhook_pruning.rs`):
    delivered/dead-lettered cutoffs each delete only their own
    status; `pending`/`in_flight`/`failed` rows are never touched
    even when ancient; `None` cutoff is a per-side no-op;
    `with_retention(0, 0)` is a full no-op (keeps existing tests
    passing); `with_retention(>0, >0)` computes cutoffs from `now`
    and deletes accordingly; counter advances on real prune;
    SQLite backend matches in-memory semantics exactly.

- **`GET /icp/v1/peer_quotes` list endpoint** — completes the
  resource-list trio (transactions + subscriptions + peer quotes
  now all have parallel list endpoints). A2A operators can finally
  enumerate their tenant's peer quotes through the API instead of
  needing every id ahead of time. Same shape as the other two:
  - Tenant-scoped (rows where `tenant_id` matches the bearer key).
  - `?status=pending|quoted|accepted|expired|rejected` filter;
    unknown value → **400** with the bad value echoed.
  - `?limit=` capped at 500 (default 100).
  - Newest-first sort by `created_at`; empty result → **200** with
    `{count: 0, data: []}`, never 404.
  - New `PeerQuoteStore::list_for_tenant(tenant_id, limit, status)`
    follows the established pattern.
  - 3 new tests in `tests/list_endpoints.rs` (12 total now):
    tenant isolation, status filter, unknown-status 400. Auth-401
    coverage extended to include the new path. OpenAPI test
    coverage extended to include `/icp/v1/peer_quotes`.

- **OpenAPI sync: 8 new endpoints + 4 new schemas** — every route
  added across the recent multi-tenancy + ops iterations is now
  declared in `/openapi.json`. Without these, an SDK generator
  (`openapi-generator`, `oapi-codegen`, Stainless) fed the
  handler's spec would skip these endpoints entirely — the routes
  exist on the wire but no client can model them. The fix:
  - `#[utoipa::path]` annotations added to 6 previously-unannotated
    handlers: `retry_webhook_delivery`, `create_webhook_subscriber`,
    `list_webhook_subscribers`, `get_webhook_subscriber`,
    `disable_webhook_subscriber`, `delete_webhook_subscriber`. Each
    declares its full response set (200 / 400 / 404 as
    appropriate) and request bodies where they exist.
  - The two new list endpoints from the prior iteration
    (`list_transactions`, `list_subscriptions`) wired into the
    paths registry — already had annotations, just needed
    registration.
  - Four new component schemas exposed: `WebhookDelivery`,
    `DeliveryStatus`, `WebhookSubscriber`, `CreateSubscriberBody`.
    Each gains `#[derive(utoipa::ToSchema)]` so the generated
    document carries the typed shapes SDK clients need to model the
    operator surface.
  - Two new openapi tests: one asserts the
    `/icp/v1/webhook_subscribers` collection path declares both
    GET (list) and POST (create) operations and that POST has a
    typed `requestBody`; the existing path-coverage and
    schema-coverage tests are extended to enumerate the 6 new
    paths and 4 new schemas. Drops in OpenAPI sync now fail loud
    in CI rather than silently shrinking the contract surface.

- **Tenant-scoped list endpoints for transactions and subscriptions** —
  closes a real ergonomic gap: previously a tenant had no API path to
  enumerate their own transactions or subscriptions. Operators had to
  hold every id in a side store or query the SQLite DB directly.
  Two new routes mirror the shape of `GET /icp/v1/webhook_deliveries`:
  - `GET /icp/v1/transactions` — caller's tenant rows, newest first.
    `?state=…` filters by FSM state
    (`draft|quoted|authorized|captured|fulfilled|completed|reversed|canceled|expired`).
    `?limit=…` page-size cap (default 100, hard max 500). Unknown
    state value → **400** with the bad value echoed (silent-empty
    would mask typos).
  - `GET /icp/v1/subscriptions` — same shape, `?status=…` ∈
    `{active, paused, canceled, past_due}`.
  - Empty result → **200** with `{count: 0, data: []}` (never 404 —
    no rows is a successful zero-row list, not a missing resource).
  - New `TransactionStore::list_for_tenant(tenant_id, limit, state)`
    and `SubscriptionStore::list_for_tenant(tenant_id, limit, status)`
    methods. Materialise rows from the JSON-blob store, filter by
    tenant, then optionally by state/status, sort newest-first by
    `created_at`, truncate to `limit`. The blob storage shape has no
    indexable `tenant_id` column — same constraint as
    `SubscriptionStore::status_counts` from the metrics iteration.
  - 9-test integration suite (`tests/list_endpoints.rs`):
    tenant-scoping (A never sees B's rows), state/status narrowing,
    unknown filter → 400 with echoed bad value, empty list → 200
    not 404, `?limit=` clamping, unauthenticated reads → 401.

- **Subscription scheduler Prometheus metrics** — completes the
  observability story for the second background loop in production.
  Operators now have scrape-able views for both the webhook outbox
  (last iteration) and the subscription scheduler. Three new
  series surface on `/metrics`:
  - **`icp_subscription_renewals_total{outcome}`** — counter,
    bumped per renewal transition. `outcome` is `renewed`,
    `failed`, or `past_due`. Alert on a rising `past_due` rate
    (payment infrastructure is degraded, or a customer cohort is
    failing at unusual rates) — the difference between `failed`
    and `past_due` is exactly one dunning-schedule exhaustion.
  - **`icp_subscriptions_by_status{status}`** — gauge, refreshed
    at end of every scheduler tick. `status` ∈
    `{active, paused, canceled, past_due}`. Dashboard `active`
    for headcount stability and `past_due` to alert on dunning
    failures requiring operator intervention.
  - **`icp_subscription_scheduler_ticks_total`** — liveness
    counter; alert on rate-of-change dropping (scheduler stuck or
    crashed).
  - New `SubscriptionStore::status_counts() -> SubscriptionStatusCounts`
    method materialises all subscription rows and tallies by
    status. The JSON-blob storage shape has no indexable `status`
    column so a `GROUP BY` isn't an option here (unlike
    `webhook_deliveries`); cost is bounded by the same
    `usize::MAX` list call the scheduler already does each tick.
  - `metrics::record_subscription_renewal(outcome)` is called
    inside `tick_subscriptions` per FSM transition;
    `metrics::record_subscription_scheduler_tick(&counts)` is
    called from `scheduler::run_loop` after each tick. Same
    pattern as the webhook metrics — production traffic exercises
    the metrics path.
  - 4-test integration suite (`tests/scheduler_metrics.rs`):
    `status_counts` returns correct distribution after direct
    state mutation; renewed counter advances on a pre-staged due
    subscription; failed counter advances when an A2A payment
    instrument is rejected (mirrors the existing
    `repeated_failures_transition_to_past_due` failure rig);
    `/metrics` exposes all three series with their HELP/TYPE
    headers and the gauge value matches the actual
    subscription-store snapshot.

- **Webhook outbox Prometheus metrics** — operators now have a
  scrape-able view of the durable outbox without needing to query
  the SQLite store. Three new series surface on `/metrics`:
  - **`icp_webhook_deliveries_total{outcome}`** — counter, bumped
    once per worker delivery transition. `outcome` is `delivered`,
    `failed`, or `dead_lettered`. Alert on a sustained
    `dead_lettered` rate (a destination is broken and ops needs
    to manually retry) or a high `failed` ratio (a destination is
    flapping).
  - **`icp_webhook_outbox_queue_depth{status}`** — gauge,
    refreshed at the end of every worker tick. `status` is one of
    the five FSM states (`pending`, `in_flight`, `delivered`,
    `failed`, `dead_lettered`). Alert on `pending` rising
    monotonically (backlog growing — the worker can't keep up) or
    on `dead_lettered > 0` (any dead letter requires operator
    attention).
  - **`icp_webhook_worker_ticks_total`** — counter, bumped on
    every tick (including ticks that processed zero rows). Alert
    on the rate-of-change dropping (worker stuck or crashed).
  - New `WebhookOutbox::status_counts() -> StatusCounts` method
    powers the gauge refresh — runs a single `GROUP BY status`
    query against SQLite, or a single iteration of the in-memory
    map, on each tick. The cost is bounded by the number of
    outbox rows (which is the same bound `list_recent` already
    accepts).
  - New `metrics::record_webhook_delivery(outcome)` and
    `metrics::record_webhook_tick(&counts)` helpers — called from
    `WebhookWorker::tick` (per delivery transition) and from
    `webhook::run_loop` (after the tick returns) respectively, so
    the metrics path is exercised by the same code that drives
    real production traffic.
  - 4-test integration suite (`tests/webhook_metrics.rs`):
    `status_counts` returns correct counts on both backends
    (in-memory + SQLite-backed via real intent activity); worker
    tick advances the `delivered` counter against a real test
    server; `/metrics` exposes all three series with their HELP
    and TYPE headers and the gauge value matches the actual
    outbox snapshot.

- **Tenant scoping for `/icp/v1/mandates/:jti/usage`** — closes the
  last cross-tenant leak on the resource read endpoints. Previously
  any authenticated caller could `GET /icp/v1/mandates/{jti}/usage`
  and read another tenant's spend tally — useful intel on a
  competitor's transaction velocity, mandate budgets, and renewal
  windows. Worse, it returned a `200` with `spent_minor: 0` for
  unknown jtis, which let callers enumerate the jti space by
  watching for non-zero responses.
  - **First-spender owns the readable view.** The first tenant to
    record spend against a mandate jti claims ownership of the
    tally; only that tenant can read it via the API. Subsequent
    spend recorded against the same jti from any tenant *still
    consumes the shared budget* — the principal who issued the
    mandate is protected from double-dipping by a leaked JTI — but
    those spenders cannot read the aggregated total.
  - `MandateUsage` gains a `tenant_id` field; `MandateLedger`
    gains a `usage_for_tenant(jti, tenant_id) -> Option<MandateUsage>`
    method that returns `None` for non-owners and for never-spent
    jtis. `record_spend` signature now takes `tenant_id` (call sites
    in `service.rs` + tests updated).
  - The handler endpoint now returns **404** for cross-tenant reads
    AND for never-spent jtis — same shape, so jti space is no
    longer probable. Owning-tenant reads return 200 with the full
    tally as before.
  - Schema: `tenant_id` column added to `mandate_usage` via the
    additive-migration mechanism. The UPSERT preserves the original
    owner via `COALESCE(NULLIF(mandate_usage.tenant_id, ''),
    excluded.tenant_id)` so subsequent spends never overwrite the
    first spender. Pre-multi-tenant rows backfill to '', invisible
    to any real tenant — same legacy policy as the other
    tenant-scoped tables.
  - 5-test integration + unit suite (`tests/mandate_usage_isolation.rs`):
    first-spender owns the view; second spender consumes budget but
    cannot read tally; never-spent jti returns 404 (not empty 200);
    unauthenticated read 401s; direct-ledger unit test exercises
    the same primitives end-to-end.

- **Tenant scoping for `/icp/v1/receipts/:jti`** — closes the same
  cross-tenant leak on the receipt read endpoint. Receipts contain
  the signed response body, so reading another tenant's receipt
  exposes their full transaction details (totals, customer info,
  line items, mandate jti). Unlike the other resources, receipts
  don't get a new `tenant_id` field — the signed claims shape is
  wire-stable and changing it would break receipt verifiers
  (including the conformance harness). Instead, ownership is
  *derived* at read time:
  - The handler reads the receipt's `claims.icp.transaction_id`,
    looks up the transaction, and compares the transaction's
    `tenant_id` against the bearer's. Mismatch → **404** (not
    403), same shape as a missing jti, so existence isn't leaked.
  - Receipts whose backing transaction has been GC'd, never
    existed, or pre-dates `tenant_id` stamping (`tenant_id == ""`)
    are unreadable to any real tenant. Conservative default —
    rather than expose every legacy receipt to whoever asks first,
    they're hidden entirely. Operators with direct DB access still
    have them.
  - 4-test integration suite (`tests/receipt_isolation.rs`):
    same-tenant read works and exposes the join-key
    `transaction_id`; cross-tenant read 404s; unknown jti 404s;
    legacy receipt with empty-string tenant_id on its transaction
    is invisible to any real tenant; unauthenticated read 401s.

- **Tenant scoping for resource read endpoints** — closes the same
  multi-tenancy gap on the per-resource read paths that the prior
  iteration closed for `webhook_deliveries`. Previously any
  authenticated caller could `GET /icp/v1/transactions/{id}`,
  `/icp/v1/subscriptions/{id}`, or `/icp/v1/peer_quotes/{id}` and
  read another tenant's PII / payment data, peer-quote pricing, and
  subscription billing details just by guessing or enumerating ids.
  - `Transaction` and `PeerQuote` now carry the originating
    `tenant_id` (Subscription already did from the multi-tenant
    webhook iteration). The field is stamped at creation time from
    the bearer key — no caller-supplied path — and `#[serde(default)]`
    keeps deserialization of pre-multi-tenant rows working.
  - All 5 transaction-construction sites in `service.rs`
    (`fresh_transaction`, the two A2A pay branches, the
    refund-flow constructor, and `subscription_pseudo_txn`) plus
    the peer-quote constructor in `do_a2a_quote` propagate
    `input.tenant.tenant_id`.
  - The three `GET /:id` handlers compare `tenant_id` after fetch
    and return **404** on mismatch (not 403) — same shape as a
    missing row, so a tenant cannot enumerate other tenants' id
    spaces or confirm an id-guess.
  - 4-test integration suite (`tests/resource_isolation.rs`):
    each resource type's same-tenant read works and exposes the
    stamped `tenant_id`; cross-tenant reads return 404; missing
    ids also return 404; unauthenticated reads return 401.

- **Tenant scoping + status filtering for `/icp/v1/webhook_deliveries`** —
  closes the multi-tenancy gap introduced when per-tenant subscribers
  shipped: previously every authenticated caller could `GET
  /icp/v1/webhook_deliveries` and see *every other tenant's* webhook
  payloads (and could probe / retry their delivery ids by guessing).
  - `WebhookDelivery` rows now carry the originating `tenant_id`,
    stamped at enqueue time from the bearer key (or from
    `Subscription.tenant_id` for scheduler-driven renewal events).
  - `GET /icp/v1/webhook_deliveries` returns only the caller's
    tenant's rows. New `?status=pending|in_flight|delivered|failed|dead_lettered`
    filter narrows by FSM state — useful for ops dashboards triaging
    only the dead-letter queue. An unknown `status` value returns
    400 with the bad value echoed (silent-empty would mask typos).
    `?limit=` is supported with a hard upper bound of 500.
  - `GET /icp/v1/webhook_deliveries/:id` and
    `POST /icp/v1/webhook_deliveries/:id/retry` are tenant-scoped:
    a cross-tenant id surfaces as **404** (identical to a missing
    row), not 403/412 — existence is not leaked across tenant
    boundaries, so tenant A can't enumerate B's id space or learn
    that a given id is `failed`.
  - 7-test integration suite (`tests/webhook_delivery_isolation.rs`):
    list-only-own-rows, cross-tenant get → 404, cross-tenant retry →
    404, status filter narrows, unknown status → 400, limit clamping,
    unauthenticated rejection.
- **Additive schema migrations** (`apply_column_additions`) — first
  step toward versioned migrations. Splits the schema into a static
  `CREATE TABLE IF NOT EXISTS` block (run on every open) and an
  ordered list of `ALTER TABLE ... ADD COLUMN` statements that
  tolerate the SQLite "duplicate column name" / "no such table"
  errors a re-run produces. Indexes that depend on additively-added
  columns also live here so they're created *after* the column
  exists (a legacy DB would otherwise see `CREATE INDEX` fail with
  "no such column"). Bootstraps the upgrade path: a handler binary
  built from this commit applied against a DB created by an older
  binary lifts the `tenant_id` column onto `webhook_deliveries`
  with `''` defaults, leaves existing rows queryable, and adds the
  `idx_webhook_deliveries_tenant_created` index needed by the new
  per-tenant list endpoint. Verified by
  `additive_migration_backfills_tenant_id_on_legacy_db` —
  drops the column to simulate a pre-upgrade DB, reopens the pool,
  and asserts the legacy row is still queryable with the defaulted
  tenant_id.

- **Per-tenant webhook subscribers** — multi-tenant deployments can
  now register their own webhook destinations rather than sharing the
  handler-wide `ICP_WEBHOOK_URL`. New CRUD surface lives under
  `/icp/v1/webhook_subscribers`:
  - `POST /icp/v1/webhook_subscribers` — create a subscriber for the
    bearer's tenant. Body: `{ "url": "https://...", "secret": "..." }`.
    URL must be `http(s)://`; secret must be non-empty (per-subscriber
    HMAC key — receivers verify deliveries with this exact secret).
    Returns the secret **once** so the caller can persist it
    out-of-band; subsequent reads redact it.
  - `GET /icp/v1/webhook_subscribers` — list this tenant's subscribers
    (active + disabled), secret redacted.
  - `GET /icp/v1/webhook_subscribers/:id` — read one. Returns 404
    across tenants (existence is not leaked across tenant boundaries —
    intentionally not 403).
  - `POST /icp/v1/webhook_subscribers/:id/disable` — soft-disable.
    Future events skip the subscriber; the row stays so the operator
    can audit / re-enable.
  - `DELETE /icp/v1/webhook_subscribers/:id` — hard-remove.
  - Persistent `webhook_subscribers` table joins the existing SQLite
    schema and survives restart; an in-memory variant backs the test
    suite.
  - **Fan-out semantics:** every state-changing intent enqueues one
    `WebhookDelivery` per **active** subscriber whose `tenant_id`
    matches the originating bearer's tenant. If the tenant has zero
    subscribers, the legacy `ICP_WEBHOOK_URL` fallback fires (existing
    single-tenant deployments keep working with no config change).
    When subscribers are present, the global fallback is **suppressed**
    for that tenant — explicit always wins over default.
  - The originating `tenant_id` propagates from the bearer key
    (`IntentInput.tenant.tenant_id`) and from the subscription
    scheduler (`Subscription.tenant_id`, new field, defaulted for serde
    backwards-compat). Cross-tenant event leakage is impossible by
    construction — there is no code path where tenant A's event sees
    tenant B's destinations.
  - 10-test integration suite (`tests/webhook_subscribers.rs`) covering
    CRUD, fan-out, tenant isolation (404 on cross-tenant read/write/
    disable), disable semantics, validation (empty URL / non-http(s)
    URL / empty secret), unauthenticated rejection, secret redaction
    on read paths, global fallback behavior, and the "no subscribers +
    no global = no outbox writes" no-op case.

- **`examples/anthropic_agent.py`** (Python) — Claude running an ICP
  merchant via Anthropic tool-use. 5 tools (search/quote/authorize/buy/
  track), standard tool-use loop, mandate attached only to writes.
  Substrate test for "agent framework as first-class consumer" — the
  glue between Claude and the merchant is exactly the Python client
  library, nothing custom. Optional `anthropic` dep; `ANTHROPIC_API_KEY`
  required at runtime. See `clients/python/CHANGELOG.md` for the
  package-scoped entry.
- **`examples/listen.py`** — live SSE listener demo that pairs with
  `buy_flow.py`. Prints each event to stdout with a fixed-width type
  column and a one-line summary (`transaction_id`, `subscription_id`,
  `peer_quote_id`, `order_id`, `agent_id`, totals when present), so the
  output tails cleanly in a terminal. Ctrl-C exits without leaking the
  underlying HTTP stream. Together with `buy_flow.py` the two examples
  cover "agent-as-merchant" + "agent-as-observer" — the producer/
  consumer halves of an ICP-Full deployment.
- **Python SSE event-stream wrapper** — `icp.events()` returns a
  context-managed iterator over `GET /icp/v1/events:stream`. Inline
  text/event-stream parser (no new dep) handles blank-line dispatch,
  comment keep-alives, multi-line `data:`, single-leading-space
  stripping per the HTML Living Standard. `SseEvent` dataclass carries
  `id`, `type`, auto-parsed `data` (when JSON), and `raw` for non-JSON
  payloads. Clean break-out: exiting the `with` block closes the
  underlying HTTP stream even on early `break`. 10-test
  `tests/test_sse.py` suite using `httpx.MockTransport` covers the
  parser end-to-end + the Client wrapper + 4xx → `IcpError` +
  unmanaged-iteration guard.

### Added
- **Python client reaches ICP-Full parity** —
  `clients/python/stateset_icp/client.py` now ships ergonomic wrappers
  for all 17 intents in the ICP-Full catalog: `search`, `describe`,
  `quote`, `authorize`, `buy`, `pay`, `track`, `return_` (trailing `_`
  dodges the Python keyword), `refund_request`, `subscribe`, `renew`,
  `pause`, `cancel_subscription`, `a2a_quote`, `a2a_pay` (two shapes —
  pay-against-quote vs direct-pay, with a client-side validator),
  `negotiate` (proposed-total vs discount-pct, mutually exclusive),
  and `confirm_receipt`. The shared `_call(intent, params, ...)` helper
  centralizes envelope construction so headers + auth propagate
  uniformly through every wrapper.
- **22-test envelope-contract suite** (`clients/python/tests/test_client_envelopes.py`)
  using `httpx.MockTransport` — covers each wrapper's HTTP method +
  URL, the exact `IntentEnvelope` body it produces, and header
  propagation (`ICP-Mandate`, `ICP-Idempotency-Key`,
  auto-generated `ICP-Request-Id`). Runs offline — no handler needed.
  Catches any shape drift between the client and server's parsing
  before a live request would 400.
- Python client README updated with a complete intent-to-method table
  so `pip install stateset-icp` → instant reference for every ICP-Full
  flow.

### Added
- **Subscription dunning** — production-grade backoff between failed
  renewal attempts. Previously after `MAX_RENEWAL_FAILURES = 3`
  consecutive scheduler failures (which all happened in immediate
  succession, often within a single tick window), a subscription
  would past_due — meaning a transient card decline could past_due
  the customer in seconds. The new
  `Config.subscription_dunning_schedule_hours` (env var
  `ICP_SUBSCRIPTION_DUNNING_SCHEDULE_HOURS`) takes a comma-separated
  list of hour-counts; each entry is the wait between the Nth failure
  and the next attempt. After the schedule is exhausted, the next
  failure transitions to past_due. Default `1,6,24` (1h → 6h → 24h
  → past_due on the 4th failure) gives transient issues the time
  they need to self-resolve. An empty schedule preserves the legacy
  immediate-retry semantics; `Config::for_test()` defaults to empty
  so every existing scheduler test still passes byte-for-byte.
  - New `IcpService::dunning_max_attempts()` derives the past-due
    threshold from the schedule length so the two knobs can never
    drift apart.
  - New `dunning_next_attempt(failure_count, now)` consults the
    schedule and returns the timestamp the next attempt should be
    scheduled for; `None` means the schedule is exhausted.
  - The scheduler's failure branch now pushes `next_charge_at`
    forward by the configured backoff on each failure instead of
    leaving it where it was — so the worker won't re-pick the sub
    until the wall clock has advanced past the schedule entry.
  - `parse_dunning_schedule` validates the env input: drops empty,
    non-numeric, zero, and >8760 (>1 year) entries silently so a
    typo can never crash boot or accept absurd waits.
- 7 dunning integration tests (`tests/dunning.rs`):
  - `first_failure_pushes_next_charge_by_first_backoff` — `[1, 6, 24]`
    schedule + 1 failure → `next_charge_at` is exactly 1h forward.
  - `second_tick_before_backoff_elapses_does_not_re_attempt` — the
    property test: 30 min after a failure (still inside the 1h
    backoff), a `tick_subscriptions` call sees 0 due subs and the
    failure counter doesn't advance.
  - `backoff_grows_with_each_failure_per_schedule` — three
    consecutive failures with `[1, 6, 24]` set the backoff to 1h,
    6h, 24h respectively.
  - `schedule_exhaustion_transitions_to_past_due` — 4th failure
    against a 3-entry schedule pasts_due.
  - `successful_renewal_clears_dunning_state` — a manual
    `intent.renew` between failures resets the counter AND moves
    `next_charge_at` past the dunning backoff target.
  - `empty_schedule_preserves_legacy_immediate_retry` — pins the
    backwards-compat property: with no schedule, `next_charge_at`
    is unchanged on failure (so the existing tests' `force_due`
    pattern keeps working).
  - `long_dunning_window_works_correctly` — Stripe-style
    `[24, 72, 168]` (1d / 3d / 1w) schedule end-to-end through
    past_due.

## [0.2.0] — 2026-04-21

### Added
- **Operator webhook retry** (`POST /icp/v1/webhook_deliveries/:id/retry`)
  — closes the dead-letter recovery gap. Previously a webhook delivery
  that exhausted its 5 retries was stuck in `dead_lettered` forever
  with no recovery path, even after the operator fixed the receiver.
  The new endpoint flips a `failed` or `dead_lettered` row back to
  `pending` with attempts reset to 0, immediate `next_attempt_at`,
  and cleared `last_error`/`last_status_code`. The worker picks it up
  on its next tick.
  - New `WebhookOutbox::reset_for_retry` returns a typed `RetryError`
    enum that maps 1:1 to HTTP statuses: `NotFound` → 404,
    `AlreadyPending`/`InFlight`/`AlreadyDelivered` → 412 with
    actionable messages. The `InFlight` refusal is the important
    one — without it, retry would race the worker mid-attempt.
  - Retry restores the row to a *fresh* state, not just unstuck:
    after retry, the row gets the full `max_attempts` budget on its
    next failure cycle. Verified by
    `retry_resets_attempt_counter_so_next_failure_starts_fresh`
    which forces 5 failures → dead_letter → retry → 4 more failures
    → still `failed`, not yet `dead_lettered`.
- 7-test integration suite (`tests/webhook_retry.rs`) using a local
  in-process axum mock receiver whose response sequence can be
  rewritten mid-test (simulating an operator fixing the bug):
  end-to-end retry + delivery, retry-from-failed-state collapses
  the backoff to immediate, 404 on unknown id, 412 on
  retry-pending / retry-delivered / retry-in-flight, and the
  attempt-counter reset property.

### Added
- **`@stateset/create-icp-commerce` npm scaffolder**
  (`clients/npm/create-icp-commerce/`) — Phase 3 distribution wedge.
  `npx @stateset/create-icp-commerce my-store` generates a minimal
  Rust project in `./my-store/`: `Cargo.toml` (depends on the handler
  via git), `src/main.rs` (20-line wrapper — `Config::load` →
  `build_app_state` → `serve`), `.env` preconfigured with demo keys +
  SQLite paths + dev-friendly mandate-verification defaults,
  `.gitignore`, and a `README.md` that walks through `cargo run`, a
  `curl` smoke-test, a Python buy-flow example, and the
  `npx @stateset/icp-conformance` validation step. Zero runtime deps,
  ESM, Node 18+. 9 hermetic Node tests cover name validation, template
  rendering (no orphan `{{` tokens, every file present),
  overwrite-refusal, and custom target dirs. Pairs with
  `@stateset/icp-conformance` to complete the "60 seconds from
  `npx create-icp-commerce` to a validated handler" story.

### Added
- **Pre-auth (per-IP) rate limiting** — closes the fake-bearer flood
  vector. `Config.pre_auth_rate_limit_per_minute` (sitting unwired
  since v0.1) is now enforced *before* tenant resolution, so a stream
  of requests with random invalid bearers gets capped at the IP layer
  instead of generating unbounded keystore lookups + 401 log entries.
  - Reuses the existing `RateLimiter` (no new state machine; same
    fixed-window semantics, same `Retry-After` math).
  - Client IP extracted from `X-Forwarded-For` (first segment per
    RFC 7239), falling back to `X-Real-IP` (nginx convention), and
    finally a sentinel `direct` bucket so all unidentified callers
    share one allowance — production deployments behind a proxy get
    the right behavior automatically.
  - Pre-auth denials are stamped with `X-RateLimit-Scope: pre-auth`
    so callers + observability can distinguish them from per-tenant
    429s. The post-auth response continues to omit this header.
  - Applied to write paths only (`POST /icp/v1/intents`); GET
    endpoints (`/health`, `/.well-known/icp`, `/openapi.json`, etc.)
    stay hammerable for monitoring + dashboards.
  - Capacity 0 disables the limit (single config knob covers
    "production: enable", "dev: optional").
- 8-test integration suite (`tests/pre_auth_rate_limit.rs`):
  burst-from-one-IP capped, distinct IPs don't share buckets,
  fake-bearer floods hit pre-auth ceiling (the property test that
  proves the limiter fires *before* auth resolution), `X-Real-IP`
  fallback honored, XFF first-segment used (not the full chain),
  GET endpoints unaffected, capacity-0 disables, and missing
  forwarding headers lump into a single `direct` bucket.

### Added
- **Per-tenant rate limiting** (`src/rate_limit.rs`) — `Config.rate_limit_per_minute`
  was sitting unwired since v0.1; now enforced. Fixed-window counter per
  `tenant_id`, default 60s window, capacity from `ApiKeyInfo.rate_limit_per_minute`
  (per-tenant override) or the handler-wide config default. Capacity of 0
  disables the limit (trusted internal clients). On allow, stamps
  `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` headers
  (industry-standard names). On deny, returns HTTP 429 with `Retry-After`
  + `X-RateLimit-*`. Idempotency replays count against the rate limit
  too — they're real HTTP calls the handler does work for.
  - In-memory + per-instance for v1.0; multi-instance deployments will
    see N× the per-tenant budget. Distributed counters (Redis-backed)
    are a v1.1 concern.
  - 7-test integration suite (`tests/rate_limit.rs`): within-capacity
    succeeds with X-RateLimit headers, N+1 returns 429+Retry-After,
    per-tenant override beats default, capacity=0 means unlimited,
    distinct tenants don't share buckets, next window clears the
    counter, rate-limit headers also appear on idempotency replays.
  - 6 unit tests in `rate_limit::tests` cover the limiter logic with
    deterministic `Instant` injection (no `tokio::sleep` in tests).

### Added
- **`@stateset/icp-conformance` npm wrapper** (`clients/npm/icp-conformance/`)
  — closes the Phase 2 distribution loop. Non-Rust developers can now
  run `npx @stateset/icp-conformance --url … --api-key … --agent-id …`
  to validate any handler URL against the spec, without installing
  cargo. The wrapper is a ~150-LOC pure-Node launcher (zero runtime
  deps) that resolves the Rust reference binary in priority order:
  (1) `ICP_CONFORMANCE_BIN` env override, (2) platform cache at
  `~/.cache/stateset/icp-conformance/<version>/` (reserved for a
  future release-download post-install hook), (3) `icp-conformance` on
  `PATH` (from `cargo install`), (4) `cargo run` fallback inside a
  monorepo checkout. When nothing resolves, prints a single actionable
  message naming all install paths and exits with POSIX 127. Node
  18+, ESM, `node --test` coverage of the resolver logic.
- **Golden test vectors** (`docs/specification/vectors/`) — byte-exact
  fixtures that pin the wire-level operations every ICP implementation
  must agree on: Ed25519 public key → `did:key` multibase encoding,
  and compact-JWS mandate signing (JCS-canonicalized header + payload,
  Ed25519 signature). Rust regression tests (`tests/vectors.rs`) and
  Python interop tests (`clients/python/tests/test_vectors.py`) both
  load the same JSON fixtures and assert their implementations produce
  byte-identical output. **Proven end-to-end:** a mandate signed by the
  Python client decodes under the Rust handler's `decode_unverified`,
  and both languages produce the identical compact JWS from the same
  fixed inputs (RFC 8032 test-vector-1 seed, pinned iat/nbf/exp). This
  is the substrate proof that the contract isn't Rust-shaped — a
  stranger implementing from the spec can achieve full signature-level
  interop. Regeneration is gated behind
  `ICP_REGENERATE_VECTORS=1 cargo test --test vectors`; any diff there
  is a breaking wire-format change.
- **`intent.negotiate` + `intent.confirm_receipt` — catalog reaches
  17/17, handler advertises `tier: "icp-full"`.** Previously the
  discovery doc declared `tier: "icp-core"` because these two intents
  parsed but rejected with `intent_not_supported`; now they're
  end-to-end and the tier auto-promotes (the tier is derived from the
  concrete intent set so the discovery doc can't lie about it).
  - `intent.negotiate` counter-offers the totals on a `quoted`
    transaction. Accepts either `proposed_total: Money` (whole-basket
    override) or `discount_pct: f64` in `[0.0, 90.0]`. Rejects
    re-negotiation once the transaction has left `quoted`. Every round
    is stamped onto `transaction.external_refs["negotiation_NNN"]` as
    a JSON blob recording `from_minor`, `to_minor`, `discount_pct`,
    `message`, agent, and timestamp — so audit reconstruction needs
    only the transaction record. Mandate scope: `quote`.
  - `intent.confirm_receipt` is the buyer's acknowledgement of
    physical receipt. Only allowed on `captured` / `fulfilled` /
    `completed` transactions. Pre-payment confirms return 412; double
    confirms (idempotency at the domain level) also return 412.
    Stamps `receipt_confirmed_at`, `receipt_confirmed_by`, and an
    optional `receipt_note` into `external_refs`. In production this
    is the trigger for escrow release on A2A and stablecoin flows.
    Mandate scope: `fulfill`.
  - Both intents emit bespoke event types
    (`transaction.renegotiated`, `transaction.receipt_confirmed`)
    rather than re-emitting the existing state event. Subscribers
    won't see duplicate `transaction.completed` events on
    `confirm_receipt`.
  - 15-test integration suite (`tests/negotiate_confirm.rs`) covers
    proposed-total override, discount-pct math, multi-round audit
    history, missing-proposal rejection, out-of-range discount
    rejection, non-quoted rejection, unknown-transaction 404, scope
    enforcement, post-payment confirm path, pre-payment confirm
    rejection, double-confirm rejection, scope enforcement on confirm,
    discovery `tier: "icp-full"` with empty `missing_intents`, and
    MCP catalog count of 17.
- **Python client — `clients/python/stateset-icp`** (Phase 2) —
  handwritten from `/openapi.json` and `ICP_SPEC.md` alone, no code
  generation and no imports from the Rust source. `Client` covers
  discovery, JWKS, the full `submit_intent` pipeline with mandate +
  idempotency headers, and ergonomic wrappers for the buy lifecycle
  (`quote` / `authorize` / `buy`). `Ed25519KeyPair` mints `did:key`
  identifiers inline (multicodec `0xed 0x01` + base58btc, zero deps
  beyond `cryptography`) and `sign_mandate` produces a compact JWS that
  a signature-verifying handler (now the default) accepts without any
  out-of-band key registry. `examples/buy_flow.py` runs the complete
  quote→authorize→buy→fetch-receipt→check-mandate-usage flow against a
  local handler. This is the substrate test for "a stranger can
  implement ICP from the spec" — if this client keeps working as the
  spec evolves, the contract is real.
- **ICP-Core / ICP-Full conformance tiers** (ICP_SPEC §15.1) — the
  spec now declares two tiers so a handler can ship an honest 1.0
  without waiting for every last intent. ICP-Core = the 15 implemented
  intents (search/describe/quote/authorize/buy/pay/track/return/
  refund_request/subscribe/renew/pause/cancel_subscription/a2a_quote/
  a2a_pay). ICP-Full adds `negotiate` + `confirm_receipt`. The
  discovery document at `/.well-known/icp` now advertises
  `conformance.tier` — derived from the concrete intent set so a
  handler can never lie about its capabilities. Calling an intent
  outside the declared tier returns `intent_not_supported` with HTTP
  501.

### Added
- **Outbound webhook delivery** (`src/webhook.rs`) — every state-changing
  intent now durably enqueues an event for delivery to a configured
  subscriber URL. Architecture:
  - **Outbox pattern**: a new `webhook_deliveries` table in the state
    DB acts as the durable queue. Writes happen synchronously inside
    the intent pipeline so an event is queued before the response is
    sent. If the handler crashes between intent success and worker
    delivery, the next process to come up resumes from the same row —
    delivery is at-least-once.
  - **HMAC-SHA256 signatures** in Stripe convention:
    `ICP-Signature: t=<unix>,v1=<hex>` over `<t>.<body>`. Receivers
    independently verify with the shared secret; constant-time
    comparison + body bytes are signed exactly as transmitted (no
    re-serialization drift). Companion `webhook::verify` helper
    provided for receiver code.
  - **Per-delivery headers**: `ICP-Event-Type`, `ICP-Event-Id`,
    `ICP-Delivery-Id`, `ICP-Delivery-Attempt` so subscribers can
    correlate, dedupe, and surface attempt counts in their UIs.
  - **Retry policy**: exponential backoff (`attempts²` seconds, capped
    at 1 hour). Failed deliveries flip to `failed` and are retried on
    the schedule; after `max_attempts` (default 5) the row transitions
    to `dead_lettered` and is no longer retried.
  - **Background worker**: `webhook::run_loop` ticks every
    `DEFAULT_TICK_SECS` (5s), drains up to 50 due deliveries per
    pass, processes each through HMAC sign + reqwest POST. Aborted
    cleanly when HTTP/gRPC exit.
  - **Refusal-to-send-unsigned**: if `ICP_WEBHOOK_URL` is set but
    `ICP_WEBHOOK_SECRET` is missing, the worker logs a warn and
    refuses to spawn — events accumulate in the outbox rather than
    being sent unsigned and unauthenticatable by the receiver.
  - **Read endpoints**: `GET /icp/v1/webhook_deliveries` (most-recent
    100) and `GET /icp/v1/webhook_deliveries/:id` for operator
    inspection / dashboards.
  - Read-only intents (search/describe/track) do NOT enqueue —
    avoids drowning subscribers in noise.
- 11-test integration suite (`tests/webhooks.rs`) with an in-process
  axum mock receiver: enqueue-on-quote, full delivery + signature
  verifies, signature rejected with wrong secret, 5xx → failed +
  backoff, 5 attempts → dead_lettered + no further retries,
  cross-restart persistence (deliver after the original handler
  "crashed"), `GET` list/by-id endpoints, opt-out when no URL
  configured, read-intents-don't-enqueue, in-memory backend basics,
  background `run_loop` actually drains.

### Added
- **OpenAPI 3.1 schema** — `GET /openapi.json` now serves a
  machine-readable contract covering every first-class HTTP endpoint and
  every public model type. The schema is derived at compile time from
  `#[utoipa::path]` annotations on the handlers and `#[derive(ToSchema)]`
  on the 34 model structs/enums, so the contract can't drift from the
  running code. A companion `GET /docs` serves a Swagger UI page that
  loads the schema via CDN (no multi-MB UI assets embedded in the
  binary).
  - Unblocks Phase 2's polyglot bindings — a Python or Go developer
    can fetch `/openapi.json` from a running handler, feed it to
    `openapi-generator`, and get a working client without reading
    `src/models.rs`.
  - 14 core paths documented (ICP intents, transactions, subscriptions,
    peer quotes, receipts, mandate usage, SSE, webhook deliveries,
    `/.well-known/icp`, JWKS, plus health/ready/metrics).
  - 34 schema components registered (envelopes, money/address/buyer,
    transaction + subscription + peer-quote aggregates, all intent param
    types, payment instrument variants, A2A service kinds).
  - `submit_intent` path declares typed responses for every meaningful
    status — 200/400/401/402/403/409 — so SDK generators can emit
    proper error unions.
  - 5-test `tests/openapi.rs` suite asserts the document is OpenAPI 3.x,
    every expected path is present, every core schema is present,
    `POST /icp/v1/intents` declares its full error envelope, and
    `/docs` returns the Swagger UI shell. Regressions in either the
    handler annotations or the `ToSchema` derives fail these tests
    rather than silently shrinking the spec.

### Changed
- **`ICP_VERIFY_MANDATE_SIGNATURES` defaults to `true`** — previously
  `false` to keep the `alg:none` dev flow frictionless. That default
  let a handler advertise ICP compliance while silently accepting
  unsigned mandates, a spec violation of §6.1 and a security hole.
  The production config should be secure-by-default; dev workflows
  that rely on `alg:none` fixtures must now set
  `ICP_VERIFY_MANDATE_SIGNATURES=false` explicitly.
  `Config::for_test()` still defaults to `false` so the existing test
  suite is unaffected — tests that need verification enabled already
  opt in with `cfg.verify_mandate_signatures = true`.

### Added
- **Idempotency (ICP spec §13)** — `ICP-Idempotency-Key` header is now
  honored end-to-end. Same key + same body → cached response replayed
  verbatim with `Idempotent-Replayed: true` and `Idempotent-Key: <key>`
  response headers. Same key + semantically different body → HTTP 409
  with `{type: "conflict", code: "idempotency_conflict"}`. Builds on
  the shared SQLite state pool so the cache survives restarts (without
  this, a process restart between the original `intent.buy` and the
  retry would silently double-charge — verified by
  `cache_survives_simulated_restart_via_shared_db_path`).
  - New `src/idempotency.rs` exposes `IdempotencyStore` with both
    in-memory and pool-backed constructors plus a configurable TTL
    (default 24h, lazy eviction on read).
  - Request equivalence is computed via JCS canonicalization +
    SHA-256, so a retry that reorders JSON object keys still matches
    the original request and replays correctly (verified by
    `jcs_canonicalization_means_byte_reordered_body_replays`).
  - Only successful (2xx) responses are cached — a transient pipeline
    error never poisons the cache and gets re-tried fresh.
  - New `idempotency` table in the state-DB schema with composite
    `(tenant_id, idempotency_key)` primary key (so tenant A's keys
    never collide with tenant B's) plus an age index for future GC.
  - `ICP_REQUIRE_IDEMPOTENCY_KEY=true` rejects writes without the
    header upfront — for production deployments where the property is
    a hard contract.
  - New `ApiError::IdempotencyConflict` variant returns the
    spec-aligned `{type: "conflict", code: "idempotency_conflict"}`
    shape.
  - 9-test integration suite (`tests/idempotency.rs`) covers replay,
    conflict, distinct-keys, JCS-equivalence, missing-key behavior in
    both required and optional modes, the strongest property test
    (`replay_after_state_changing_buy_returns_same_transaction_completed_once`
    — proving that without idempotency the same retry would 412), and
    cross-restart persistence.

### Added
- **Persistent handler state (Phase 0 toward v1.0)** — mandate spend,
  signed receipts, transactions, subscriptions, and peer quotes are now
  backed by a shared SQLite pool (`src/state_db.rs`) instead of in-memory
  `HashMap`s. Closes the single most disqualifying v1.0 gap: previously,
  a 24-hour budget mandate half-spent before a handler restart would
  allow unbounded further spend in the remaining window because the
  ledger forgot the prior spend on restart.
  - New `ICP_STATE_DB_PATH` env var, defaults to `./icp-state.db`;
    tests use `:memory:`. Independent of `COMMERCE_DB_PATH` — the
    handler's protocol-level state evolves separately from the
    commerce engine's schema.
  - Each store exposes `in_memory()` (legacy default) and
    `with_pool(pool)` (persistent) constructors. `MandateLedger::new()`
    remains in-memory so existing unit tests that instantiate a bare
    ledger are unaffected.
  - `state_store.rs` introduces a generic `JsonStore<T>` shared by
    `TransactionStore`, `SubscriptionStore`, and `PeerQuoteStore` —
    values are stored as JSON blobs keyed by id, so additive model
    changes don't require table migrations.
  - `build_app_state` opens (and migrates) the state pool once at
    startup and hands the same pool to every store. WAL mode +
    `synchronous=NORMAL` for file-backed deployments; `MEMORY`
    journal + `OFF` for `:memory:`.
  - 9-test `tests/state_persistence.rs` suite proves each store's
    state survives a simulated restart by writing, dropping the pool,
    reopening against the same path, and reading back.
- **A2A (peer commerce)** — implements both A2A intents end-to-end,
  taking the implemented-intent count from 13 → 15 of 17:
  - **`intent.a2a_quote`** asks a peer agent for a quote on a service.
    Creates a `PeerQuote` aggregate (`pending` if no `price_hint`,
    `quoted` if hinted), with a `service` typed by `A2aServiceKind`
    (`compute` / `data_feed` / `image_generation` / `ad_hoc`) plus
    free-form params, configurable expiry (default 5 min, max 24 h),
    and an optional `reference_id` for cross-system correlation.
  - **`intent.a2a_pay`** has two flow shapes:
    1. **Pay-against-quote** (`peer_quote_id` supplied): looks up the
       quote, validates the requester matches and the quote is in
       `quoted` status (rejects `pending`/`accepted`/`expired`), checks
       `expires_at`, marks the quote `accepted` with `accepted_at` +
       `charge_transaction_id`, and creates a real completed
       transaction with the quote's price.
    2. **Direct payment** (no quote): requires `peer_agent_id` +
       `amount`. Creates the same shape of completed transaction
       without a quote linkage.
  - Both refuse self-payment (`peer_agent_id` == requester) and require
    a non-empty `from` wallet identifier.
  - Charge transactions carry structured `external_refs`:
    `peer_agent_id`, `peer_quote_id` (if applicable), `a2a_from`, and
    `memo`. Receipts sign over the full response body, so an audit
    trail names the peer, the source wallet, and the quote.
  - Event bus emits `peer_quote.<status>` for quote lifecycle events
    (distinct from `subscription.<status>` and `transaction.<state>`).
  - Mandate scope `pay_peer` (already in the catalog) gates both
    intents — a mandate authorizing only `quote` cannot run
    `intent.a2a_pay`.
  - New `GET /icp/v1/peer_quotes/:id` route + `PeerQuoteStore` mirroring
    the transaction/subscription store pattern.
  - Discovery + MCP `tools/list` (HTTP and stdio) automatically pick up
    the two new tools (`icp_a2a_quote`, `icp_a2a_pay`).
- 15 A2A integration tests (`tests/a2a.rs`) covering: pending vs quoted
  status on creation, full quote→pay→accepted lifecycle with
  external_refs assertions, double-pay rejected, pay-pending rejected,
  expired-quote-with-1-second-TTL marks quote `expired`, direct-pay
  without quote, self-payment and self-quote rejected,
  pay-without-from rejected, GET retrieval, 404 on unknown quote,
  mandate scope enforcement (`quote` rejected, `pay_peer` accepted),
  and discovery catalog count.

### Added
- **Automatic subscription billing scheduler** (`src/scheduler.rs` +
  `IcpService::tick_subscriptions`) — completes the subscription story
  by running renewal charges on a wall-clock cadence with no manual
  `intent.renew` calls. Architecture:
  - Background `tokio::spawn`'d task at handler boot ticks at
    `ICP_SUBSCRIPTION_SCHEDULER_INTERVAL_SECS` (default 60s).
  - Each tick scans for active subs with `next_charge_at <= now` and a
    saved payment instrument, then runs a charge using the same
    `run_subscription_charge` path as manual renewal.
  - Successful renewals advance the period anchored on
    `current_period_end` (no drift), bump `charges_completed`, reset
    `failed_renewal_attempts`, and emit a `subscription.renewed`
    event with `automatic: true` in the payload.
  - Failed charges bump `failed_renewal_attempts`; after 3 consecutive
    failures the subscription transitions to `past_due` and is no
    longer retried — the agent must call `intent.renew` manually
    (which clears the counter and re-activates).
  - Toggle off with `ICP_SUBSCRIPTION_SCHEDULER_ENABLED=false`.
  - `SchedulerTickReport` exposed for observability — counts scanned /
    due / renewed / failed / past_due each tick.
- `Subscription.payment_instrument` — the saved instrument the
  scheduler charges against. Set by `intent.subscribe`, rotated by
  `intent.renew`. A2A instruments are rejected by the scheduler since
  peer commerce requires interactive consent.
- 10 scheduler tests (`tests/scheduler.rs`) — deterministic via direct
  `tick_subscriptions(now)` calls plus one test exercising the
  background loop end-to-end at 50ms interval. Covers: noop tick,
  before-due skip, after-due renewal, paused/canceled/no-payment
  skip, three-failures-to-past-due, manual-renew-resets-counter,
  mixed-workload report counts.

### Fixed
- `PaymentInstrument::A2A` now serializes/deserializes as `"a2a"` per
  spec. Was bug: serde's default `snake_case` rule converted `A2A` to
  `"a2_a"` because of the digit boundary, so any caller sending the
  spec-correct `{"method":"a2a", …}` got a deserialization error. Each
  variant now has an explicit `#[serde(rename = "...")]` to lock down
  wire names against rule-algorithm changes.

### Added
- **Subscriptions** — implements all four subscription intents
  (`intent.subscribe`, `intent.renew`, `intent.pause`,
  `intent.cancel_subscription`) end-to-end:
  - New `Subscription` aggregate (`src/models.rs`) with status lifecycle
    (`active → paused → active`, terminal `canceled`/`past_due`),
    cadence (`weekly` / `monthly` / `annual`), period anchors
    (`current_period_start`/`end`, `next_charge_at`), and
    `last_transaction_id` linking back into the transaction store.
  - `SubscriptionStore` mirrors `TransactionStore` semantics
    (`src/state_store.rs`).
  - `intent.subscribe` creates the aggregate AND runs the initial
    charge in one shot — response carries both the new
    `Subscription` and the completed `Transaction` plus a signed
    receipt.
  - `intent.renew` advances the period anchored on the previous
    `current_period_end` (no drift over consecutive renewals) and
    creates a new charge; rejects renewal of paused/canceled subs
    with `precondition_failed`.
  - `intent.pause` and `intent.cancel_subscription` flip status,
    timestamp the transition, and emit a signed receipt over a
    synthesized pseudo-transaction whose
    `external_refs["subscription_id"]` links back to the sub.
  - New `GET /icp/v1/subscriptions/:id` route for read-back.
  - Event bus emits `subscription.<status>` events on every
    subscription intent, distinct from `transaction.<state>` events.
  - Discovery + MCP `tools/list` now advertise **13 implemented intents**
    (was 9) — the 4 new subscription tools (`icp_subscribe`,
    `icp_renew`, `icp_pause`, `icp_cancel_subscription`) appear in
    every catalog, including the stdio binary's tool list.
- 10 subscription integration tests (`tests/subscriptions.rs`)
  covering: subscribe creates sub + charge, renew advances period and
  links new charge, pause+renew rejected, cancel is terminal +
  idempotency check, GET retrieval, 404 on unknown sub id, mandate
  scope `subscribe` enforced (request with only `quote` scope rejected),
  MCP catalog, discovery catalog.

### Out of scope (v0.2)
- Automatic time-driven billing / scheduler — `intent.renew` is
  manually triggered by the agent or an external cron job.
- Dunning, prorated mid-cycle changes, trial periods.
- Persistent storage for the subscription's payment instrument; the
  caller passes a fresh payment on each `renew`.

### Added
- **MCP stdio transport** (`src/bin/icp_mcp_stdio.rs`) — second binary
  that speaks JSON-RPC 2.0 over stdin/stdout for desktop MCP clients
  (Claude Desktop, Cursor, custom agents that spawn subprocesses). All
  commerce execution flows through the same `IcpService` as the HTTP
  binary, so a stdio session and an HTTP session produce identical
  signed receipts and share the same in-memory transaction store.
  Fixes a pre-existing JSON-RPC 2.0 §5 bug in MCP `initialize`: response
  `id` now mirrors the request `id` (was hardcoded to `null`).
- `mcp::dispatch` — transport-agnostic public dispatcher extracted from
  the HTTP wrapper so both transports share one routing table.
- `docs/claude-desktop.md` — drop-in `claude_desktop_config.json`
  snippet + production hardening guide (`--require-mandate`,
  `--verify-signatures`).
- 8 stdio integration tests (`tests/mcp_stdio.rs`) that spawn the
  actual binary as a subprocess, pipe JSON-RPC frames through, and
  assert id-echoing, tool catalog, full quote→receipt roundtrip, parse
  errors emit `-32700` without killing the process, **stderr stays
  empty on a clean session** (so MCP framing on stdout isn't polluted),
  and graceful shutdown on stdin EOF.
- **`did:web` resolver** (`src/resolver.rs`) — extends mandate
  signature verification beyond self-contained `did:key` to principals
  that live at HTTPS URLs. Dereferences `did:web:host[:path:segments]`
  to `https://host/.well-known/did.json` (or
  `https://host/path/segments/did.json`), fetches the W3C DID document,
  and extracts Ed25519 verifying keys from `verificationMethod` entries
  in either `publicKeyMultibase` (z6Mk… form) or `publicKeyJwk`
  (`{kty:OKP, crv:Ed25519, x:…}`) encoding. Results are TTL-cached
  (default 10 min) to bound upstream fetches; ZERO TTL disables caching
  for tests. Now in `CompositeResolver::default_set()` after
  `DidKeyResolver`.
- `PrincipalResolver` trait converted to **async** (via `async_trait`)
  to support I/O-bound DID methods. `mandate::evaluate` and
  `mandate::verify_signature` likewise become async.
- 9 new integration tests (`tests/did_web.rs`) covering both key
  encodings, nested URL paths, TTL cache and zero-TTL bypass, network
  failure handling, rejection of non-Ed25519 documents, composite
  fall-through to `did:key`, and an **end-to-end** test that boots an
  in-process axum mock `did:web` host, signs a mandate with the
  principal's key, submits it to a real handler, and asserts the
  handler resolves the DID over HTTP and verifies the signature.
- **`icp-conformance` harness** (`src/bin/icp_conformance.rs`) —
  implementation-independent conformance tester. Points at any ICP
  handler URL and validates 13 spec dimensions: discovery shape, JWKS,
  health, `ICP-Version` header presence, discovery-vs-implementation
  consistency, `intent.quote` happy path, receipt `body_digest`
  reproducible via independent JCS+SHA-256, receipt Ed25519 signature
  verifies against the published JWKS, transaction + receipt
  retrievability, error envelope shape on unknown intent, error envelope
  shape on missing auth, and full quote→authorize→buy lifecycle.
  Deliberately imports nothing from the handler library; passing the
  suite is evidence that *any* handler conforms. Exit 0 on all-pass,
  non-zero otherwise. Includes a `demo_conformance.sh` that boots a
  fresh handler and runs the suite against it for one-command CI.
- `Intent::is_implemented()` — gates discovery + MCP catalog so we only
  advertise intents that actually have a service-layer handler. Caught
  by the conformance test `discovery_intents_all_live`, which would
  otherwise flag the v0.1 handler as non-conformant for advertising
  `intent.subscribe`, `intent.a2a_pay`, etc.

### Fixed
- **Engine `orders.create` now actually persists** (`src/commerce.rs`).
  v0.1 wrote `..Default::default()` for `CreateOrderItem`, leaving
  `product_id` as `nil()` UUID, which the engine validator rejected
  silently — the buy still completed but `order` came back `null`. Fix:
  `ensure_product_for_sku` looks up the product by SKU via
  `products().get_variant_by_sku`, auto-creating the product + a
  default variant from the line-item data when the catalog hasn't seen
  the SKU yet. Also: customer `first_name` falls back to the local part
  of the email (and then to `"ICP Buyer"`) when the buyer didn't supply
  one, so the engine's non-empty validator doesn't reject otherwise
  well-formed orders. Net result: a fresh handler against an empty
  SQLite database self-seeds its catalog and persists real orders with
  engine-generated `ORD-…` order numbers — no manual product upload
  required for demo / conformance / first-touch flows.
- New integration test `buy_with_engine_persists_real_order` runs the
  full quote→authorize→buy lifecycle against the real embedded engine
  and asserts `order.order_number` starts with `ORD-` and `order.id` is
  the engine's UUID rather than the ICP `txn_…` id. This locks in the
  persistence contract end-to-end.

### Fixed
- Discovery document and MCP `tools/list` no longer advertise the 8
  catalog-only intents (`intent.subscribe`, `intent.a2a_pay`, etc.) that
  are scheduled for v0.2. Calling them still returns
  `intent_not_supported` with the same body shape; they're just absent
  from the advertised catalog so clients (especially LLM-driven MCP
  clients) don't plan around tools that always fail.

### Added
- **Mandate signature verification** (`src/resolver.rs` +
  `mandate::verify_signature`) — mandates are now cryptographically
  verified against the principal's advertised Ed25519 keyset, not just
  scope/budget/window-checked. Gated by the new
  `ICP_VERIFY_MANDATE_SIGNATURES` config flag (default: false, to keep
  the existing dev flow working with `alg:none` mandates;
  production **MUST** enable). Rejects `alg:none` and non-EdDSA algs
  outright; tries the JWS `kid`-matching key first and falls back to
  other keys the principal advertises.
- `PrincipalResolver` trait + `DidKeyResolver` + `CompositeResolver`.
  Supports `did:key:z…` (Ed25519 embedded, self-contained — no network
  resolution) out of the box. `did:web`, `did:stateset:buyer:…`, and
  HTTPS-profile resolution plug in as new `Box<dyn PrincipalResolver>`
  instances via the composite; both are documented v0.3 follow-ups.
- `resolver::encode_did_key` — test/example helper to round-trip an
  Ed25519 verifying key to a `did:key` URI.
- `MandateEvaluation.signature_verified` — boolean flag on every
  accepted evaluation indicating whether the crypto path actually ran
  (vs. dev-mode bypass).
- 8 mandate-signature integration tests (`tests/mandate_signatures.rs`)
  exercising: valid signature accepted, tampered payload rejected,
  wrong-key signature rejected, `alg:none` rejected when verification is
  on, `alg:none` accepted when off, malformed `did:key` rejected,
  unsupported DID method rejected, and full quote→authorize→buy
  lifecycle under a real signed mandate.
- **MCP surface** (`src/mcp.rs`) — JSON-RPC 2.0 endpoint at `POST /mcp`
  exposing every ICP intent as a discoverable tool. Supports
  `initialize`, `tools/list`, `tools/call`, `ping`, `resources/list`,
  `prompts/list`. Tool catalog has 17 entries (`icp_search`,
  `icp_quote`, `icp_buy`, `icp_return`, `icp_a2a_pay`, …) with per-intent
  JSON input schemas derived from the ICP spec. `tools/call` responses
  wrap the full ICP response body — including the signed Ed25519 receipt
  — under `structuredContent`, and include a human-readable summary
  under `content[].text`. Auth: same tenant bearer key as ICP; we
  synthesize `did:stateset:agent:mcp-<tenant>` and route through
  `IcpService::handle_intent` under the compat self-mandate. Protocol
  version `2024-11-05`. Toggle via `ICP_MCP_ENABLED`.
- MCP integration tests (`tests/mcp.rs`) — 12 tests covering
  initialize/tools/list/tools/call happy path + error path, full
  quote→authorize→buy flow via tool calls, auth enforcement, unknown
  method/tool rejection (-32601 / -32602), invalid JSON-RPC version
  (-32600), route toggling, and empty resources/prompts catalogs.
- **UCP compatibility surface** (`src/compat/ucp.rs`) — wires
  `/checkout-sessions`, `/checkout-sessions/:id` (GET + PUT), and the
  `/complete` + `/cancel` sub-paths to the internal intent pipeline. A
  UCP-native agent can now complete a full session on an ICP handler
  unchanged. Also serves `GET /.well-known/ucp` with a minimal capability
  advertisement (profile, services, shopping endpoint). Native headers
  (`UCP-Version: 2026-01-11`, `Request-Id`) are stamped on every
  response. Toggle via `ICP_UCP_COMPAT_ENABLED`.
- UCP compat tests (`tests/ucp_compat.rs`) — 10 tests covering lifecycle,
  discovery, the PUT-only update semantics (POST-to-update returns 405),
  route toggling, version header rejection, and **ACP/UCP coexistence**
  (both compat surfaces enabled on the same handler without
  interference).
- **ACP compatibility surface** (`src/compat/acp.rs`) — wires
  `/checkout_sessions`, `/checkout_sessions/:id` (GET + POST update),
  `/checkout_sessions/:id/complete`, and `/checkout_sessions/:id/cancel`
  to the internal intent pipeline. An ACP-native agent (ChatGPT Instant
  Checkout or similar) can now complete a full session against this
  handler unchanged. Enabled by default; toggle via
  `ICP_ACP_COMPAT_ENABLED`. Responses carry `API-Version: 2025-09-29`
  and `Request-Id` headers as ACP requires.
- ACP compat tests (`tests/acp_compat.rs`) — 8 tests covering session
  lifecycle, headers, receipt creation on compat path, route toggling,
  and auth/version rejection.
- `IntentInput::for_icp` / `IntentInput::for_compat` constructors — the
  compat variant bypasses mandate enforcement (tenant API key *is* the
  self-mandate per `docs/interop.md`).
- Integration test suite (`tests/integration.rs`) — 22 end-to-end tests
  covering discovery/JWKS/health/metrics, the full quote→authorize→buy
  lifecycle, auth and mandate enforcement, receipt body-digest
  reconstruction via JCS, and Ed25519 signature verification against the
  published JWKS.
- `Config::for_test()` — env-independent configuration for in-process
  tests.
- `.github/workflows/ci.yml` — runs `cargo fmt --check`, `cargo clippy
  -D warnings`, `cargo test`, and a Docker build on every push/PR.

### Changed
- All source now passes `cargo clippy --all-targets -- -D warnings`
  (cleaned up a handful of lints: identity-op, unnecessary lazy eval,
  len-without-is-empty, etc.).
- `IcpService::handle_intent` now respects an `IntentInput.skip_mandate_check`
  flag so compat callers (which are already tenant-authed) can run the
  pipeline without a caller-supplied mandate JWS.

## [0.1.0] — 2026-04-21 — Cornerstone

### Added
- **Intelligent Commerce Protocol specification**, version `2026-04-21`,
  published at [`docs/specification/ICP_SPEC.md`](./docs/specification/ICP_SPEC.md).
- Reference Rust handler:
  - `IcpService` intent router covering `intent.search`, `intent.describe`,
    `intent.quote`, `intent.authorize`, `intent.buy` / `intent.pay`,
    `intent.track`, `intent.return`, `intent.refund_request`.
  - `MandateLedger` for in-memory mandate decoding, scope/budget/window
    enforcement, and spend accrual.
  - Ed25519 `ReceiptSigner` producing compact-JWS receipts over
    JCS-canonicalized response bodies.
  - Embedded `stateset-icommerce` engine wrapper — persists buy-flow
    orders to SQLite (or PostgreSQL behind the `postgres` feature).
  - HTTP router on `:8082` (axum) with `/icp/v1/intents`, transactions,
    receipts, mandate usage, SSE event stream, discovery, JWKS, health,
    readiness, and Prometheus metrics.
  - gRPC server on `:50052` (tonic) with reflection and health.
  - Event bus (tokio broadcast) backing both SSE and gRPC streaming.
- Proto definition: `proto/icp_handler/v1/icp_handler.proto`.
- Docs: `architecture.md`, `getting-started.md`, `interop.md`,
  `agent-guide.md`, and the error table under
  `specification/errors.md`.
- Dockerfile (multi-stage, path-dep-aware) and `demo_test.sh` for an
  end-to-end walk through discovery → quote → authorize → buy.

### Known gaps (planned for 0.2)
- Mandate signature verification against resolvable principal DIDs.
- Full engine routing for tax, promotions, and shipping.
- `intent.subscribe`, `intent.renew`, `intent.pause`,
  `intent.cancel_subscription`.
- `intent.a2a_pay`, `intent.a2a_quote`.
- Wired ACP and UCP compatibility paths (currently advertised in
  discovery; compat handlers land in 0.2).
- MCP stdio tool surface.
- Language bindings mirroring the ACP/UCP handlers.
- Conformance test suite (`npx icp-conformance`).
