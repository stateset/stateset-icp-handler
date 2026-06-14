//! ICP intent pipeline.
//!
//! The service turns an [`IntentEnvelope`] into an [`IntentResponseBody`]
//! while:
//!
//! 1. Evaluating the mandate (if required for the intent's scope).
//! 2. Loading or creating the underlying [`Transaction`].
//! 3. Executing the intent against the embedded commerce engine.
//! 4. Recording mandate spend.
//! 5. Signing a receipt over the JCS-canonicalized response body.
//! 6. Emitting an event on the bus.
//!
//! The dispatcher [`IcpService::handle_intent`] lives here; the actual
//! per-intent implementations are split into intent-family submodules to
//! keep this file scoped to dispatch + cross-cutting concerns.

mod a2a;
mod catalog;
mod helpers;
mod lifecycle;
mod negotiate;
mod orders;
mod purchase;
mod subscriptions;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::agent::{AgentIdentifier, ApiKeyInfo};
use crate::commerce::CommerceEngine;
use crate::config::Config;
use crate::errors::ApiError;
use crate::events::{Event, EventBus};
use crate::intent::Intent;
use crate::mandate::{self, MandateLedger};
use crate::models::{
    IntentEnvelope, IntentResponseBody, OrderSummary, PeerQuote, ReceiptStub, ResponseEnvelope,
    Subscription, Transaction, TransactionState,
};
use crate::receipts::{ReceiptStore, StoredReceipt};
use crate::resolver::{CompositeResolver, PrincipalResolver};
use crate::signing::ReceiptSigner;
use crate::state_store::{PeerQuoteStore, SubscriptionStore, TransactionStore};

type KeyedLocks = Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>;

#[derive(Clone)]
pub struct IcpService {
    pub config: Arc<Config>,
    pub engine: Option<CommerceEngine>,
    pub transactions: TransactionStore,
    pub subscriptions: SubscriptionStore,
    pub peer_quotes: PeerQuoteStore,
    pub receipts: ReceiptStore,
    pub mandates: MandateLedger,
    pub idempotency: crate::idempotency::IdempotencyStore,
    pub rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    pub distributed_rate_limiter: Option<crate::rate_limit::RedisRateLimiter>,
    /// Per-IP pre-auth limiter. Fires *before* bearer resolution, so a
    /// flood of fake API keys can't burn unbounded CPU on keystore
    /// lookups. Disabled when the configured capacity is 0.
    pub pre_auth_limiter: Arc<crate::rate_limit::RateLimiter>,
    pub distributed_pre_auth_limiter: Option<crate::rate_limit::RedisRateLimiter>,
    pub webhook_outbox: crate::webhook::WebhookOutbox,
    /// Per-tenant webhook subscriber registry. Production deployments
    /// register one or more rows per tenant via the admin endpoints;
    /// the outbox fans out to all `active` rows on each event.
    pub webhook_subscribers: crate::webhook::SubscriberStore,
    /// Global fallback URL used when a tenant has no registered
    /// subscribers. `None` disables the fallback. The global secret
    /// (`Config.webhook_secret`) is what the worker signs with for
    /// the fallback path; per-tenant subscribers carry their own
    /// secret on the row.
    pub webhook_url: Option<String>,
    pub webhook_secret: Option<String>,
    pub signer: Arc<ReceiptSigner>,
    pub events: EventBus,
    pub resolver: Arc<dyn PrincipalResolver>,
    operation_locks: KeyedLocks,
}

pub struct IntentInput<'a> {
    pub envelope: IntentEnvelope,
    pub agent: AgentIdentifier,
    pub tenant: ApiKeyInfo,
    pub mandate_jws: Option<&'a str>,
    pub request_id: String,
    pub trace_id: Option<String>,
    /// When true, skip mandate enforcement. Set *only* by the ACP/UCP
    /// compatibility paths, which treat the tenant's bearer token as a
    /// self-mandate scoped to that merchant.
    pub skip_mandate_check: bool,
    /// When true, the caller has already applied the tenant rate limit
    /// (and emitted the `x-ratelimit-*` headers), so the service must not
    /// double-count. The HTTP and gRPC front doors set this; the compat /
    /// MCP surfaces leave it false so the service enforces the limit for
    /// them — making rate limiting safe-by-default for any new transport.
    pub skip_rate_limit: bool,
}

impl<'a> IntentInput<'a> {
    /// Construct an input for the first-class ICP `/icp/v1/intents` path
    /// — mandate enforcement is governed by `config.require_mandate`.
    pub fn for_icp(
        envelope: IntentEnvelope,
        agent: AgentIdentifier,
        tenant: ApiKeyInfo,
        mandate_jws: Option<&'a str>,
        request_id: String,
        trace_id: Option<String>,
    ) -> Self {
        Self {
            envelope,
            agent,
            tenant,
            mandate_jws,
            request_id,
            trace_id,
            skip_mandate_check: false,
            // HTTP / gRPC apply the tenant rate limit at the front door
            // (with response headers) before calling the service.
            skip_rate_limit: true,
        }
    }

    /// Construct an input for a compat path — mandate enforcement is
    /// bypassed in favor of tenant-level self-authorization.
    pub fn for_compat(
        envelope: IntentEnvelope,
        agent: AgentIdentifier,
        tenant: ApiKeyInfo,
        request_id: String,
        trace_id: Option<String>,
    ) -> Self {
        Self {
            envelope,
            agent,
            tenant,
            mandate_jws: None,
            request_id,
            trace_id,
            skip_mandate_check: true,
            // Compat / MCP surfaces don't pre-check; the service enforces
            // the tenant rate limit for them.
            skip_rate_limit: false,
        }
    }
}

/// Outcome of an idempotency-guarded intent execution.
pub struct IntentExecution {
    pub body: IntentResponseBody,
    /// True when the body was served from the idempotency cache rather
    /// than freshly executed — callers stamp `Idempotent-Replayed: true`.
    pub replayed: bool,
}

impl IcpService {
    pub fn new(config: Config, engine: Option<CommerceEngine>, signer: ReceiptSigner) -> Self {
        let resolver = Arc::new(CompositeResolver::default_set_with_options(
            config.allow_insecure_urls,
        ));
        Self::with_resolver(config, engine, signer, resolver)
    }

    pub fn with_resolver(
        config: Config,
        engine: Option<CommerceEngine>,
        signer: ReceiptSigner,
        resolver: Arc<dyn PrincipalResolver>,
    ) -> Self {
        let webhook_url = config.webhook_url.clone();
        let webhook_secret = config.webhook_secret.clone();
        let rate_limiter = Arc::new(crate::rate_limit::RateLimiter::per_minute(
            config.rate_limit_per_minute,
        ));
        let pre_auth_limiter = Arc::new(crate::rate_limit::RateLimiter::per_minute(
            config.pre_auth_rate_limit_per_minute,
        ));
        Self {
            config: Arc::new(config),
            engine,
            transactions: TransactionStore::new(),
            subscriptions: SubscriptionStore::new(),
            peer_quotes: PeerQuoteStore::new(),
            receipts: ReceiptStore::new(),
            mandates: MandateLedger::new(),
            idempotency: crate::idempotency::IdempotencyStore::default(),
            rate_limiter,
            distributed_rate_limiter: None,
            pre_auth_limiter,
            distributed_pre_auth_limiter: None,
            webhook_outbox: crate::webhook::WebhookOutbox::default(),
            webhook_subscribers: crate::webhook::SubscriberStore::default(),
            webhook_url,
            webhook_secret,
            signer: Arc::new(signer),
            events: EventBus::default(),
            resolver,
            operation_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn lock_idempotency_key(
        &self,
        tenant_id: &str,
        idempotency_key: &str,
    ) -> OwnedMutexGuard<()> {
        self.lock_operation(format!(
            "idem:{}:{}{}",
            tenant_id.len(),
            tenant_id,
            idempotency_key
        ))
        .await
    }

    pub(super) async fn lock_transaction(&self, transaction_id: &str) -> OwnedMutexGuard<()> {
        self.lock_operation(format!("txn:{transaction_id}")).await
    }

    pub(super) async fn lock_subscription(&self, subscription_id: &str) -> OwnedMutexGuard<()> {
        self.lock_operation(format!("sub:{subscription_id}")).await
    }

    pub(super) async fn lock_peer_quote(&self, peer_quote_id: &str) -> OwnedMutexGuard<()> {
        self.lock_operation(format!("peer_quote:{peer_quote_id}"))
            .await
    }

    async fn lock_operation(&self, key: String) -> OwnedMutexGuard<()> {
        let lock = {
            let mut guard = self
                .operation_locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Drop locks no caller is holding. While a guard is alive the
            // `Arc` strong count is >= 2 (one in the map, one in the live
            // `OwnedMutexGuard`); once released it falls to 1, so a count
            // of 1 means the entry is idle and safe to evict. Without this
            // the map grows by one `Arc<AsyncMutex>` per distinct
            // idempotency key forever — an unbounded per-request leak.
            guard.retain(|_, lock| Arc::strong_count(lock) > 1);
            guard
                .entry(key)
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    pub async fn check_pre_auth_rate_limit(
        &self,
        bucket_id: &str,
    ) -> Result<crate::rate_limit::RateLimitDecision, ApiError> {
        self.check_rate_limit(
            "pre-auth",
            bucket_id,
            None,
            &self.pre_auth_limiter,
            self.distributed_pre_auth_limiter.as_ref(),
        )
        .await
    }

    pub async fn check_tenant_rate_limit(
        &self,
        tenant: &ApiKeyInfo,
    ) -> Result<crate::rate_limit::RateLimitDecision, ApiError> {
        self.check_rate_limit(
            "tenant",
            &tenant.tenant_id,
            tenant.rate_limit_per_minute,
            &self.rate_limiter,
            self.distributed_rate_limiter.as_ref(),
        )
        .await
    }

    async fn check_rate_limit(
        &self,
        scope: &str,
        bucket_id: &str,
        bucket_capacity: Option<u32>,
        local: &crate::rate_limit::RateLimiter,
        distributed: Option<&crate::rate_limit::RedisRateLimiter>,
    ) -> Result<crate::rate_limit::RateLimitDecision, ApiError> {
        let Some(distributed) = distributed else {
            return Ok(local.check(bucket_id, bucket_capacity));
        };

        match distributed.check(bucket_id, bucket_capacity).await {
            Ok(decision) => Ok(decision),
            Err(err) if self.config.is_production() => Err(ApiError::EngineUnavailable(format!(
                "distributed {scope} rate limiter unavailable: {err}"
            ))),
            Err(err) => {
                tracing::warn!(
                    scope,
                    error = %err,
                    "distributed rate limiter unavailable; falling back to local limiter"
                );
                Ok(local.check(bucket_id, bucket_capacity))
            }
        }
    }

    pub async fn handle_intent(
        &self,
        input: IntentInput<'_>,
    ) -> Result<IntentResponseBody, ApiError> {
        if input.envelope.agent_id != input.agent.raw {
            return Err(ApiError::AuthenticationFailed(format!(
                "envelope.agent_id `{}` does not match ICP-Agent-Id `{}`",
                input.envelope.agent_id, input.agent.raw
            )));
        }

        let intent = Intent::parse(&input.envelope.intent)?;

        let resolver: Option<&dyn PrincipalResolver> = if self.config.verify_mandate_signatures {
            Some(self.resolver.as_ref())
        } else {
            None
        };
        let mandate = match (intent.scope(), input.skip_mandate_check) {
            (Some(scope), false) => match input.mandate_jws {
                Some(jws) => Some(
                    mandate::evaluate(
                        jws,
                        scope,
                        helpers::estimate_intent_amount_minor(&input.envelope),
                        Utc::now(),
                        &input.tenant.tenant_id,
                        input.envelope.context.jurisdiction.as_deref(),
                        &self.mandates,
                        resolver,
                    )
                    .await?,
                ),
                None if self.config.require_mandate => {
                    return Err(ApiError::MandateInvalid(
                        "ICP-Mandate header required for this intent scope".into(),
                    ));
                }
                None => None,
            },
            _ => None,
        };
        if let Some(ev) = mandate.as_ref() {
            if ev.payload.sub != input.agent.raw {
                return Err(ApiError::MandateOutOfScope(format!(
                    "mandate subject `{}` does not authorize agent `{}`",
                    ev.payload.sub, input.agent.raw
                )));
            }
        }

        // Route by intent.
        let outcome = match intent {
            Intent::Search => self.do_search(&input).await?,
            Intent::Describe => self.do_describe(&input).await?,
            Intent::Quote => self.do_quote(&input, mandate.as_ref()).await?,
            Intent::Authorize => self.do_authorize(&input, mandate.as_ref()).await?,
            Intent::Buy | Intent::Pay => self.do_buy(&input, mandate.as_ref()).await?,
            Intent::Track => self.do_track(&input).await?,
            Intent::Return => self.do_return(&input, mandate.as_ref()).await?,
            Intent::RefundRequest => self.do_refund_request(&input, mandate.as_ref()).await?,
            Intent::Subscribe => self.do_subscribe(&input, mandate.as_ref()).await?,
            Intent::Renew => self.do_renew(&input, mandate.as_ref()).await?,
            Intent::Pause => self.do_pause(&input).await?,
            Intent::CancelSubscription => self.do_cancel_subscription(&input).await?,
            Intent::A2aQuote => self.do_a2a_quote(&input, mandate.as_ref()).await?,
            Intent::A2aPay => self.do_a2a_pay(&input, mandate.as_ref()).await?,
            Intent::Negotiate => self.do_negotiate(&input, mandate.as_ref()).await?,
            Intent::ConfirmReceipt => self.do_confirm_receipt(&input).await?,
        };

        let envelope = ResponseEnvelope {
            icp_version: self.config.icp_version.clone(),
            request_id: input.request_id.clone(),
            trace_id: input.trace_id.clone(),
            issued_at: Utc::now(),
        };

        // Build response body (unsigned) so we can canonicalize, digest,
        // then sign.
        let intent_id = input
            .envelope
            .intent_id
            .clone()
            .unwrap_or_else(|| format!("int_{}", Uuid::new_v4().simple()));

        let mut body = IntentResponseBody {
            intent: intent.wire_name().to_string(),
            intent_id: intent_id.clone(),
            transaction: outcome.transaction.clone(),
            order: outcome.order.clone(),
            subscription: outcome.subscription.clone(),
            peer_quote: outcome.peer_quote.clone(),
            // Placeholder — overwritten after signing.
            receipt: ReceiptStub {
                jti: String::new(),
                kid: self.signer.kid.clone(),
                jws: String::new(),
                body_digest: String::new(),
            },
            envelope,
        };

        let receipt = if intent.is_state_change() {
            let bytes = serde_jcs::to_vec(&body)
                .map_err(|e| ApiError::ProcessingError(format!("jcs: {e}")))?;
            let signed = self
                .signer
                .sign_receipt(
                    &input.agent.raw,
                    &self.config.public_base_url,
                    intent.wire_name(),
                    &body.transaction.id,
                    body.order.as_ref().map(|o| o.id.as_str()),
                    mandate.as_ref().map(|m| m.payload.jti.as_str()),
                    &bytes,
                )
                .map_err(|e| ApiError::ProcessingError(format!("sign: {e}")))?;

            self.receipts.insert(StoredReceipt {
                jti: signed.jti.clone(),
                kid: signed.kid.clone(),
                jws: signed.jws.clone(),
                body_digest: signed.body_digest.clone(),
                claims: signed.claims.clone(),
            });

            ReceiptStub {
                jti: signed.jti,
                kid: signed.kid,
                jws: signed.jws,
                body_digest: signed.body_digest,
            }
        } else {
            ReceiptStub {
                jti: String::new(),
                kid: self.signer.kid.clone(),
                jws: String::new(),
                body_digest: String::new(),
            }
        };

        body.receipt = receipt;

        // Event type follows the dominant aggregate produced by the
        // intent: `peer_quote.<status>` for A2A, `subscription.<status>`
        // for subscription intents, otherwise `transaction.<state>`.
        // `negotiate` and `confirm_receipt` don't transition the
        // transaction state but ARE meaningful lifecycle events, so
        // they get bespoke event types instead of duplicating the
        // existing `transaction.quoted` / `transaction.completed`.
        let evt_type = if let Some(pq) = outcome.peer_quote.as_ref() {
            format!("peer_quote.{}", pq.status.wire_name())
        } else if let Some(sub) = outcome.subscription.as_ref() {
            format!("subscription.{}", sub.status.wire_name())
        } else {
            match intent {
                Intent::Negotiate => "transaction.renegotiated".to_string(),
                Intent::ConfirmReceipt => "transaction.receipt_confirmed".to_string(),
                _ => format!("transaction.{}", outcome.transaction.state.wire_name()),
            }
        };
        let event = Event {
            id: format!("evt_{}", Uuid::new_v4().simple()),
            r#type: evt_type,
            transaction_id: Some(outcome.transaction.id.clone()),
            order_id: outcome.order.as_ref().map(|o| o.id.clone()),
            agent_id: Some(input.agent.raw.clone()),
            occurred_at: Utc::now(),
            payload: serde_json::to_value(&body).unwrap_or(Value::Null),
        };
        // Outbound webhook (durable outbox) — only enqueue on
        // state-changing intents to avoid spamming subscribers with
        // search/describe noise. Read-only intents already skip the
        // receipt path; same gate applies here.
        if intent.is_state_change() {
            self.enqueue_webhook(&event, Some(&input.tenant.tenant_id));
        }
        self.events.emit(event);

        Ok(body)
    }

    /// Execute an intent under idempotency protection shared by every
    /// transport (the first-class HTTP path *and* the ACP/UCP compat
    /// surfaces). The effective dedup key is `explicit_key` when present,
    /// otherwise the client-supplied `intent_id` — spec §7.4/§15.2 make an
    /// intent idempotent under `(agent_id, intent_id)`, independent of the
    /// optional `ICP-Idempotency-Key` header. With neither key there is
    /// nothing stable to dedup on, so the intent runs unguarded.
    ///
    /// Centralizing this here is what closes the double-charge gap on the
    /// compat paths, which previously called `handle_intent` directly and
    /// re-executed on every network retry.
    pub async fn handle_intent_idempotent(
        &self,
        input: IntentInput<'_>,
        explicit_key: Option<&str>,
    ) -> Result<IntentExecution, ApiError> {
        // Enforce the tenant rate limit for surfaces that don't pre-check at
        // their front door (compat ACP/UCP, MCP). HTTP and gRPC set
        // `skip_rate_limit` because they already applied it (and emitted the
        // `x-ratelimit-*` headers). Gating here means a newly-added transport
        // is rate-limited by default rather than silently uncapped.
        if !input.skip_rate_limit {
            if let crate::rate_limit::RateLimitDecision::Denied { .. } =
                self.check_tenant_rate_limit(&input.tenant).await?
            {
                return Err(ApiError::RateLimited);
            }
        }

        let effective_key: Option<String> = match explicit_key {
            Some(k) if !k.is_empty() => Some(k.to_string()),
            _ => input
                .envelope
                .intent_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|id| format!("intent:{}:{id}", input.agent.raw)),
        };

        let Some(key) = effective_key else {
            let body = self.handle_intent(input).await?;
            return Ok(IntentExecution {
                body,
                replayed: false,
            });
        };

        let tenant_id = input.tenant.tenant_id.clone();
        // Match retries by semantic (JCS-canonicalized) request equality,
        // so a reused key with a different body is a 409 conflict.
        let canonical = serde_jcs::to_vec(&input.envelope)
            .map_err(|e| ApiError::ProcessingError(format!("jcs: {e}")))?;
        let digest = crate::idempotency::IdempotencyStore::digest_request(&canonical);

        // The in-process lock is the fast path: it serializes same-key
        // concurrency within this process so we rarely hit the database
        // reservation for local duplicates. The DB reservation below is what
        // extends that guarantee across *separate* processes sharing one
        // database — neither alone is sufficient for a multi-replica fleet.
        let guard = self.lock_idempotency_key(&tenant_id, &key).await;
        let lease = chrono::Duration::seconds(self.config.idempotency_lease_secs.max(1) as i64);

        let replay =
            |cached: crate::idempotency::CachedResponse| -> Result<IntentExecution, ApiError> {
                let body: IntentResponseBody = serde_json::from_slice(&cached.body_json)
                    .map_err(|e| ApiError::ProcessingError(format!("cached response JSON: {e}")))?;
                Ok(IntentExecution {
                    body,
                    replayed: true,
                })
            };
        let conflict = || {
            ApiError::IdempotencyConflict(format!(
                "idempotency key `{key}` was used previously with a different request body"
            ))
        };

        match self
            .idempotency
            .reserve(&tenant_id, &key, &digest, Utc::now(), lease)
        {
            crate::idempotency::ReserveOutcome::Replay(cached) => replay(cached),
            crate::idempotency::ReserveOutcome::Conflict => Err(conflict()),
            crate::idempotency::ReserveOutcome::Won(reservation_id) => {
                // We own the claim. Execute, then record the response — or
                // release the reservation so a failed attempt can be retried
                // immediately rather than waiting out the lease. Both calls
                // carry our reservation token so a stale-lease takeover can't
                // be clobbered by, nor clobber, this caller.
                let body = match self.handle_intent(input).await {
                    Ok(body) => body,
                    Err(e) => {
                        self.idempotency.release(&tenant_id, &key, &reservation_id);
                        return Err(e);
                    }
                };
                let body_bytes = serde_json::to_vec(&body)
                    .map_err(|e| ApiError::ProcessingError(format!("serialize for cache: {e}")))?;
                self.idempotency.store(
                    &tenant_id,
                    &key,
                    &digest,
                    crate::idempotency::CachedResponse {
                        status: 200,
                        body_json: body_bytes,
                    },
                    Utc::now(),
                    &reservation_id,
                );
                Ok(IntentExecution {
                    body,
                    replayed: false,
                })
            }
            crate::idempotency::ReserveOutcome::InProgress => {
                // Another *process* holds the reservation (a same-process
                // duplicate would have been serialized by `guard` before
                // reaching `reserve`). The DB reservation is the real guard
                // now, so drop the in-process lock before sleeping rather than
                // head-of-line-blocking same-key callers for the poll window.
                drop(guard);
                // Wait briefly for its result and replay it; if it doesn't land
                // in time, return a retryable error so the client retries (by
                // which point the original has almost certainly completed).
                const MAX_POLLS: u32 = 20;
                const POLL_INTERVAL_MS: u64 = 100;
                for _ in 0..MAX_POLLS {
                    tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
                    match self
                        .idempotency
                        .lookup(&tenant_id, &key, &digest, Utc::now())
                    {
                        crate::idempotency::LookupOutcome::Replay(cached) => return replay(cached),
                        crate::idempotency::LookupOutcome::Conflict => return Err(conflict()),
                        crate::idempotency::LookupOutcome::Miss => continue,
                    }
                }
                Err(ApiError::EngineUnavailable(format!(
                    "a request with idempotency key `{key}` is already in progress; retry shortly"
                )))
            }
        }
    }

    /// Return true when an in-process event belongs to `tenant_id`.
    ///
    /// The event bus is intentionally process-wide so background
    /// workers, gRPC streams, and SSE can share one fan-out channel.
    /// Public streaming endpoints must apply this predicate before
    /// serializing an event, otherwise one tenant can observe another
    /// tenant's lifecycle metadata.
    pub fn event_belongs_to_tenant(&self, event: &Event, tenant_id: &str) -> bool {
        if let Some(txn_id) = event.transaction_id.as_deref() {
            return self
                .transactions
                .get(txn_id)
                .is_some_and(|t| t.tenant_id == tenant_id);
        }
        if let Some(sub_id) = event
            .payload
            .get("subscription_id")
            .and_then(|v| v.as_str())
        {
            return self
                .subscriptions
                .get(sub_id)
                .is_some_and(|s| s.tenant_id == tenant_id);
        }
        if let Some(peer_quote_id) = event.payload.get("peer_quote_id").and_then(|v| v.as_str()) {
            return self
                .peer_quotes
                .get(peer_quote_id)
                .is_some_and(|q| q.tenant_id == tenant_id);
        }
        false
    }
}

/// Internal carrier for what each `do_*` intent method returns. The
/// dispatcher uses these fields to build the `IntentResponseBody` and
/// pick the event type. Visible across submodules of `service` only.
pub(super) struct Outcome {
    pub(super) transaction: Transaction,
    pub(super) order: Option<OrderSummary>,
    /// Set by subscription intents (`subscribe`, `renew`, `pause`,
    /// `cancel_subscription`). Surfaced into `IntentResponseBody.subscription`
    /// and drives `subscription.<status>` event emission.
    pub(super) subscription: Option<Subscription>,
    /// Set by `intent.a2a_quote` and `intent.a2a_pay` (when paying an
    /// existing quote). Drives `peer_quote.<status>` event emission.
    pub(super) peer_quote: Option<PeerQuote>,
}

/// Result of one [`IcpService::tick_expiries`] sweep. Surfaced to
/// tests for deterministic assertions and to the
/// `icp_expiries_total{kind}` counter via the run loop.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct ExpiryTickReport {
    /// Transactions that had `quote_expires_at <= now` and a
    /// non-terminal pre-auth state (Draft / Quoted) — transitioned
    /// to `Expired` this tick.
    pub transactions_expired: usize,
    /// Peer quotes that had `expires_at <= now` and a non-terminal
    /// status (Pending / Quoted) — transitioned to `Expired`.
    pub peer_quotes_expired: usize,
}

/// Result of one [`IcpService::tick_subscriptions`] sweep — useful for
/// deterministic testing and Prometheus exposition.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct SchedulerTickReport {
    /// Total subscriptions in the store (any status).
    pub scanned: usize,
    /// Subset due for auto-renewal (active + next_charge_at <= now +
    /// payment_instrument present).
    pub due: usize,
    /// Subscriptions that successfully renewed in this tick.
    pub renewed: usize,
    /// Subscriptions whose charge failed in this tick.
    pub failed: usize,
    /// Subscriptions that transitioned to `past_due` in this tick.
    pub past_due: usize,
}

impl TransactionState {
    pub(super) fn wire_name(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Quoted => "quoted",
            Self::Authorized => "authorized",
            Self::Captured => "captured",
            Self::Fulfilled => "fulfilled",
            Self::Completed => "completed",
            Self::Reversed => "reversed",
            Self::Canceled => "canceled",
            Self::Expired => "expired",
        }
    }
}
