//! Per-tenant rate limiting.
//!
//! Enforces a **fixed-window** request budget per tenant: each tenant
//! has a counter that resets every `window`, capped at `capacity`
//! requests. The first request after a window expires opens a new
//! window. The choice of fixed-window over sliding-window is
//! deliberate for v1.0:
//!   * **simpler concurrent state** — one HashMap entry per tenant
//!   * **deterministic `Retry-After`** — the exact seconds until the
//!     current window expires, no estimation
//!   * **bursty-friendly** — a tenant batching at the start of a
//!     window gets the full capacity in a few seconds, the same as
//!     real-world API client behavior
//!
//! Per-tenant capacity comes from `ApiKeyInfo.rate_limit_per_minute`
//! when the bearer key declares one; otherwise the handler-wide
//! `Config.rate_limit_per_minute` default applies. A `capacity` of 0
//! disables the limit entirely (useful for trusted internal clients).
//!
//! State is in-memory by default for local/test runs. Production can
//! attach a Redis-backed limiter so the same fixed window is enforced
//! across every handler instance in the fleet.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitDecision {
    Allowed {
        /// Capacity for this window.
        limit: u32,
        /// Requests remaining in the current window AFTER this request
        /// has been counted.
        remaining: u32,
        /// Seconds until the window resets (always > 0).
        reset_in_secs: u64,
    },
    Denied {
        limit: u32,
        /// Seconds the caller should wait before retrying.
        retry_after_secs: u64,
    },
}

#[derive(Debug)]
struct Bucket {
    /// Number of requests counted in the current window.
    used: u32,
    /// When the current window started.
    window_start: Instant,
}

/// In-memory fixed-window rate limiter.
pub struct RateLimiter {
    /// Default per-tenant capacity when the tenant doesn't declare one.
    pub default_capacity: u32,
    pub window: Duration,
    buckets: Mutex<HashMap<String, Bucket>>,
}

/// Redis-backed fixed-window rate limiter.
///
/// Keys are minute-bucketed and TTL'd slightly past the window reset.
/// The caller's bucket id is SHA-256 hashed before it becomes part of
/// the Redis key so raw bearer-derived tenant ids, IPs, or proxy header
/// values are never written directly into shared infrastructure.
#[derive(Clone)]
pub struct RedisRateLimiter {
    pub default_capacity: u32,
    pub window: Duration,
    prefix: String,
    manager: ConnectionManager,
}

impl RedisRateLimiter {
    pub async fn new(
        redis_url: &str,
        prefix: impl Into<String>,
        default_capacity: u32,
        window: Duration,
    ) -> redis::RedisResult<Self> {
        let client = redis::Client::open(redis_url)?;
        let manager = client.get_connection_manager().await?;
        Ok(Self {
            default_capacity,
            window,
            prefix: prefix.into(),
            manager,
        })
    }

    pub async fn per_minute(
        redis_url: &str,
        prefix: impl Into<String>,
        default_capacity: u32,
    ) -> redis::RedisResult<Self> {
        Self::new(redis_url, prefix, default_capacity, Duration::from_secs(60)).await
    }

    pub async fn check(
        &self,
        bucket_id: &str,
        bucket_capacity: Option<u32>,
    ) -> redis::RedisResult<RateLimitDecision> {
        let cap = bucket_capacity.unwrap_or(self.default_capacity);
        if cap == 0 {
            return Ok(RateLimitDecision::Allowed {
                limit: 0,
                remaining: u32::MAX,
                reset_in_secs: self.window.as_secs(),
            });
        }

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.check_at_epoch(bucket_id, cap, now_secs).await
    }

    async fn check_at_epoch(
        &self,
        bucket_id: &str,
        capacity: u32,
        now_secs: u64,
    ) -> redis::RedisResult<RateLimitDecision> {
        let window_secs = self.window.as_secs().max(1);
        let window_index = now_secs / window_secs;
        let reset_in_secs = (window_secs - (now_secs % window_secs)).max(1);
        let key = self.key(bucket_id, window_index);

        let mut con = self.manager.clone();
        let count: i64 = redis::cmd("EVAL")
            .arg(
                "local current = redis.call('INCRBY', KEYS[1], 1)\n\
                 if current == 1 then\n\
                   redis.call('EXPIRE', KEYS[1], ARGV[1])\n\
                 end\n\
                 return current",
            )
            .arg(1)
            .arg(&key)
            .arg((window_secs + 1) as i64)
            .query_async(&mut con)
            .await?;

        if count > i64::from(capacity) {
            return Ok(RateLimitDecision::Denied {
                limit: capacity,
                retry_after_secs: reset_in_secs,
            });
        }

        let used = u32::try_from(count).unwrap_or(u32::MAX);
        Ok(RateLimitDecision::Allowed {
            limit: capacity,
            remaining: capacity.saturating_sub(used),
            reset_in_secs,
        })
    }

    fn key(&self, bucket_id: &str, window_index: u64) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bucket_id.as_bytes());
        let digest = hex::encode(hasher.finalize());
        format!("{}:{}:{}", self.prefix, window_index, digest)
    }
}

impl RateLimiter {
    pub fn new(default_capacity: u32, window: Duration) -> Self {
        Self {
            default_capacity,
            window,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Per-minute helper — the most common production setting.
    pub fn per_minute(default_capacity: u32) -> Self {
        Self::new(default_capacity, Duration::from_secs(60))
    }

    /// Check + increment in one atomic operation. Returns the decision
    /// the caller should act on (allow + headers, or deny + 429).
    /// Setting `tenant_capacity = 0` disables the limit for this
    /// caller — Allowed is returned without touching the bucket map.
    pub fn check(&self, tenant_id: &str, tenant_capacity: Option<u32>) -> RateLimitDecision {
        let cap = tenant_capacity.unwrap_or(self.default_capacity);
        if cap == 0 {
            return RateLimitDecision::Allowed {
                limit: 0,
                remaining: u32::MAX,
                reset_in_secs: self.window.as_secs(),
            };
        }
        self.check_at(tenant_id, cap, Instant::now())
    }

    /// Same as `check`, but with a caller-supplied `now` so tests can
    /// drive deterministic windows without sleeping.
    pub fn check_at(&self, tenant_id: &str, capacity: u32, now: Instant) -> RateLimitDecision {
        // A poisoned mutex means a previous thread panicked while
        // holding the lock and bucket state may be inconsistent.
        // Fail closed: deny with a short retry-after rather than
        // propagating the panic, so a poisoned bucket doesn't take
        // down the whole intent pipeline. The poisoned guard still
        // gives us mutable access, so we also reset the offending
        // entry under it before returning.
        let mut guard = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::error!(
                    tenant_id,
                    "rate limiter mutex poisoned; failing closed for this request and clearing bucket"
                );
                let mut guard = poisoned.into_inner();
                guard.remove(tenant_id);
                return RateLimitDecision::Denied {
                    limit: capacity,
                    retry_after_secs: 1,
                };
            }
        };
        let bucket = guard.entry(tenant_id.to_string()).or_insert(Bucket {
            used: 0,
            window_start: now,
        });

        // Window rollover.
        if now.saturating_duration_since(bucket.window_start) >= self.window {
            bucket.window_start = now;
            bucket.used = 0;
        }

        let elapsed = now.saturating_duration_since(bucket.window_start);
        let reset_in = self.window.saturating_sub(elapsed);
        let reset_in_secs = reset_in.as_secs().max(1);

        if bucket.used >= capacity {
            return RateLimitDecision::Denied {
                limit: capacity,
                retry_after_secs: reset_in_secs,
            };
        }

        bucket.used = bucket.used.saturating_add(1);
        let remaining = capacity.saturating_sub(bucket.used);
        RateLimitDecision::Allowed {
            limit: capacity,
            remaining,
            reset_in_secs,
        }
    }

    pub fn clear(&self) {
        match self.buckets.lock() {
            Ok(mut guard) => guard.clear(),
            Err(poisoned) => {
                tracing::error!("rate limiter mutex poisoned; clearing under poisoned guard");
                poisoned.into_inner().clear();
            }
        }
    }

    /// Test helper — number of distinct tenants with active buckets.
    pub fn tracked_tenants(&self) -> usize {
        match self.buckets.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => {
                tracing::error!("rate limiter mutex poisoned; reading length under poisoned guard");
                poisoned.into_inner().len()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_limit_allows() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        let now = Instant::now();
        for _ in 0..3 {
            assert!(matches!(
                rl.check_at("t", 3, now),
                RateLimitDecision::Allowed { .. }
            ));
        }
    }

    #[test]
    fn fourth_call_in_same_window_is_denied() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        let now = Instant::now();
        for _ in 0..3 {
            rl.check_at("t", 3, now);
        }
        match rl.check_at("t", 3, now) {
            RateLimitDecision::Denied {
                limit,
                retry_after_secs,
            } => {
                assert_eq!(limit, 3);
                assert!((1..=60).contains(&retry_after_secs));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn next_window_clears_counter() {
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        let now = Instant::now();
        rl.check_at("t", 2, now);
        rl.check_at("t", 2, now);
        assert!(matches!(
            rl.check_at("t", 2, now),
            RateLimitDecision::Denied { .. }
        ));
        // Jump past the window.
        let later = now + Duration::from_secs(61);
        assert!(matches!(
            rl.check_at("t", 2, later),
            RateLimitDecision::Allowed { .. }
        ));
    }

    #[test]
    fn distinct_tenants_dont_share_buckets() {
        let rl = RateLimiter::new(1, Duration::from_secs(60));
        let now = Instant::now();
        rl.check_at("a", 1, now);
        rl.check_at("b", 1, now);
        // Both are at limit independently.
        assert!(matches!(
            rl.check_at("a", 1, now),
            RateLimitDecision::Denied { .. }
        ));
        assert!(matches!(
            rl.check_at("b", 1, now),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[test]
    fn capacity_zero_means_unlimited() {
        let rl = RateLimiter::new(5, Duration::from_secs(60));
        // Per-tenant override of 0 → no limit applied; bucket isn't
        // even created.
        for _ in 0..1000 {
            assert!(matches!(
                rl.check("infinite-tenant", Some(0)),
                RateLimitDecision::Allowed { .. }
            ));
        }
        assert_eq!(rl.tracked_tenants(), 0);
    }

    #[test]
    fn remaining_reflects_count_post_increment() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        let now = Instant::now();
        let RateLimitDecision::Allowed { remaining, .. } = rl.check_at("t", 3, now) else {
            panic!()
        };
        assert_eq!(remaining, 2);
        let RateLimitDecision::Allowed { remaining, .. } = rl.check_at("t", 3, now) else {
            panic!()
        };
        assert_eq!(remaining, 1);
        let RateLimitDecision::Allowed { remaining, .. } = rl.check_at("t", 3, now) else {
            panic!()
        };
        assert_eq!(remaining, 0);
    }
}
