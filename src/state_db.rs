//! Shared persistent state pool for handler-owned data: mandate spend,
//! signed receipts, in-flight transactions, subscriptions, peer quotes.
//!
//! This is deliberately separate from the iCommerce engine's own database.
//! The handler's state is *protocol-level* — it describes what agents have
//! spent against which mandates, which receipts we have signed, and which
//! transactions are currently quoted/authorized/captured. It must survive
//! handler restarts independently of commerce-engine schema evolution.
//!
//! Schema is applied idempotently on every pool open via `CREATE TABLE IF
//! NOT EXISTS`. When the first breaking schema change lands, introduce a
//! `schema_version` table and per-version migration steps.

use anyhow::Context;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub type StatePool = Pool<SqliteConnectionManager>;

/// Open (or create) a SQLite-backed state pool.
///
/// `path` is either a filesystem path (`./icp-state.db`) or `:memory:` for
/// an ephemeral per-handler store. In-memory pools are capped at a single
/// connection so the backing database isn't dropped when a worker releases
/// its connection — a hard requirement of SQLite's in-memory mode.
pub fn open(path: &str) -> anyhow::Result<StatePool> {
    let is_memory = path == ":memory:";
    let manager = if is_memory {
        SqliteConnectionManager::memory().with_init(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=MEMORY;\
                 PRAGMA synchronous=OFF;\
                 PRAGMA foreign_keys=ON;",
            )
        })
    } else {
        SqliteConnectionManager::file(path).with_init(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;\
                 PRAGMA synchronous=NORMAL;\
                 PRAGMA foreign_keys=ON;\
                 PRAGMA busy_timeout=5000;",
            )
        })
    };

    let pool = Pool::builder()
        .max_size(if is_memory { 1 } else { 16 })
        .build(manager)
        .context("build state pool")?;

    migrate(&pool)?;
    Ok(pool)
}

fn migrate(pool: &StatePool) -> anyhow::Result<()> {
    let conn = pool.get().context("acquire state conn for migration")?;
    conn.execute_batch(SCHEMA).context("apply state schema")?;
    apply_column_additions(&conn).context("apply additive column migrations")?;
    Ok(())
}

/// Idempotent additive column migrations + dependent index creation.
///
/// `CREATE TABLE IF NOT EXISTS` covers fresh DBs but doesn't touch a
/// table that already exists with a stale shape. For each column added
/// post-1.0 we issue an `ALTER TABLE ... ADD COLUMN` that tolerates the
/// "duplicate column name" error a second-run will produce. SQLite has
/// no native `ADD COLUMN IF NOT EXISTS`, so this is the supported
/// pattern. Every column added here must have a `DEFAULT` so existing
/// rows backfill cleanly without a separate UPDATE pass.
///
/// Indexes that reference newly-added columns live here too — they
/// can't go in the static `SCHEMA` block because on a legacy DB that
/// block runs *before* the column is added, and the bare
/// `CREATE INDEX` would fail with `no such column`.
///
/// Keep this list strictly additive — never reorder, never alter,
/// never drop. Breaking schema changes get a real `schema_version` /
/// per-version migration ladder.
fn apply_column_additions(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    // 1) Add columns. Tolerate "duplicate column name" so a second run
    //    is a no-op. Tolerate "no such table" so a partial / aborted
    //    earlier migration doesn't wedge subsequent runs.
    const COLUMN_ADDITIONS: &[&str] = &[
        // tenant_id added so list/get/retry endpoints can scope by
        // tenant and avoid leaking other tenants' webhook payloads.
        // Existing pre-multi-tenant rows backfill to '' — invisible to
        // any real tenant_id but still queryable for ops by string
        // comparison, which is the desired isolation property.
        "ALTER TABLE webhook_deliveries ADD COLUMN tenant_id TEXT NOT NULL DEFAULT ''",
    ];
    for stmt in COLUMN_ADDITIONS {
        match conn.execute(stmt, []) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") || msg.starts_with("no such table") => {}
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("additive column migration failed: {stmt}")))
            }
        }
    }

    // 2) Indexes that depend on the columns above. `IF NOT EXISTS`
    //    makes these safely idempotent.
    const INDEX_ADDITIONS: &[&str] = &[
        // Per-tenant list/get/retry queries scope by (tenant_id,
        // created_at DESC). A composite index keeps the scan from
        // touching cold rows as the table grows.
        "CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_tenant_created \
             ON webhook_deliveries(tenant_id, created_at)",
    ];
    for stmt in INDEX_ADDITIONS {
        conn.execute(stmt, [])
            .with_context(|| format!("additive index migration failed: {stmt}"))?;
    }
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS mandate_usage (
    jti          TEXT PRIMARY KEY,
    spent_minor  INTEGER NOT NULL DEFAULT 0,
    window_start TEXT
);

CREATE TABLE IF NOT EXISTS receipts (
    jti          TEXT PRIMARY KEY,
    kid          TEXT NOT NULL,
    jws          TEXT NOT NULL,
    body_digest  TEXT NOT NULL,
    claims_json  TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS transactions (
    id           TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS subscriptions (
    id           TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS peer_quotes (
    id           TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- Durable idempotency cache (ICP spec §13). Stores the response body and
-- status returned for a given (tenant_id, idempotency_key) pair, plus a
-- SHA-256 of the JCS-canonicalized request envelope so retries with a
-- different body can be flagged as `idempotency_conflict` rather than
-- silently replaying the wrong response.
--
-- Composite primary key keeps tenants isolated — agent A's idempotency
-- key never collides with agent B's even if they pick the same string.
CREATE TABLE IF NOT EXISTS idempotency (
    tenant_id        TEXT NOT NULL,
    idempotency_key  TEXT NOT NULL,
    request_digest   TEXT NOT NULL,
    response_status  INTEGER NOT NULL,
    response_body    TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    PRIMARY KEY (tenant_id, idempotency_key)
);

-- Cheap age sweep — TTL eviction happens lazily during reads, but a
-- standing index lets a future GC pass scan by age efficiently.
CREATE INDEX IF NOT EXISTS idx_idempotency_age ON idempotency(created_at);

-- Outbound webhook delivery outbox. Each row is one (event, target URL)
-- pair: writing the row is the durable hand-off from the intent
-- pipeline to the delivery worker, so an event survives a restart even
-- if it hasn't been transmitted yet. Status FSM:
--   pending → in_flight → delivered
--                       ↘ failed (retried) → in_flight ...
--                                          ↘ dead_lettered (max attempts)
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id               TEXT PRIMARY KEY,
    event_id         TEXT NOT NULL,
    event_type       TEXT NOT NULL,
    url              TEXT NOT NULL,
    payload_json     TEXT NOT NULL,
    status           TEXT NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    max_attempts     INTEGER NOT NULL,
    next_attempt_at  TEXT NOT NULL,
    last_status_code INTEGER,
    last_error       TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    delivered_at     TEXT,
    -- Originating tenant. The list/get/retry endpoints scope by this
    -- column so tenant A never sees tenant B's payloads. Defaults to
    -- '' for legacy single-tenant rows; see additive migration below.
    tenant_id        TEXT NOT NULL DEFAULT ''
);

-- Worker scans by (status, next_attempt_at) every tick — composite index
-- collapses the scan to an index range without touching cold rows.
CREATE INDEX IF NOT EXISTS idx_webhook_status_next
    ON webhook_deliveries(status, next_attempt_at);

-- Per-tenant webhook subscribers. Each row is one (tenant_id, url) pair
-- that will receive events from that tenant's intent activity. The
-- global ICP_WEBHOOK_URL stays as a fallback when a tenant has no
-- registered subscribers — useful for ops dashboards observing the
-- whole fleet — but production multi-tenant deployments register
-- per-tenant rows so each merchant's events go to their own system.
CREATE TABLE IF NOT EXISTS webhook_subscribers (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    url         TEXT NOT NULL,
    secret      TEXT NOT NULL,
    active      INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Lookups always filter by tenant + active; index supports the
-- per-tick fan-out scan with no full-table reads.
CREATE INDEX IF NOT EXISTS idx_webhook_subscribers_tenant_active
    ON webhook_subscribers(tenant_id, active);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_pool_opens_and_schema_applies() {
        let pool = open(":memory:").expect("open in-memory state pool");
        let conn = pool.get().expect("acquire conn");
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(tables.contains(&"mandate_usage".to_string()));
        assert!(tables.contains(&"receipts".to_string()));
        assert!(tables.contains(&"transactions".to_string()));
        assert!(tables.contains(&"subscriptions".to_string()));
        assert!(tables.contains(&"peer_quotes".to_string()));
        assert!(tables.contains(&"idempotency".to_string()));
        assert!(tables.contains(&"webhook_deliveries".to_string()));
        assert!(tables.contains(&"webhook_subscribers".to_string()));
    }

    #[test]
    fn file_pool_survives_reopen() {
        let dir = tempdir();
        let path = dir.join("state.db");
        let path_str = path.to_string_lossy().to_string();

        {
            let pool = open(&path_str).expect("open 1");
            let conn = pool.get().expect("acquire");
            conn.execute(
                "INSERT INTO mandate_usage (jti, spent_minor, window_start) VALUES ('m1', 500, '2026-04-21T00:00:00Z')",
                [],
            )
            .expect("insert");
        }
        {
            let pool = open(&path_str).expect("open 2");
            let conn = pool.get().expect("acquire");
            let (spent, window): (i64, Option<String>) = conn
                .query_row(
                    "SELECT spent_minor, window_start FROM mandate_usage WHERE jti = 'm1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("select");
            assert_eq!(spent, 500);
            assert_eq!(window.as_deref(), Some("2026-04-21T00:00:00Z"));
        }
    }

    /// Verifies the additive `tenant_id` migration backfills cleanly
    /// onto a DB created by an older handler that didn't have the
    /// column. Simulates the upgrade path: open a fresh pool, drop
    /// `tenant_id` (post-condition: schema looks pre-migration),
    /// insert a legacy row, then reopen — `apply_column_additions`
    /// must run idempotently and the legacy row must remain queryable
    /// with `tenant_id = ''`.
    #[test]
    fn additive_migration_backfills_tenant_id_on_legacy_db() {
        let dir = tempdir();
        let path = dir.join("legacy.db");
        let path_str = path.to_string_lossy().to_string();

        // First open: full schema (current version) is applied.
        {
            let pool = open(&path_str).expect("open 1");
            let conn = pool.get().expect("acquire");
            // Simulate "this DB was created before tenant_id existed"
            // by dropping the column (and the index that references
            // it). SQLite ≥ 3.35 supports DROP COLUMN.
            conn.execute("DROP INDEX idx_webhook_deliveries_tenant_created", [])
                .expect("drop tenant index");
            conn.execute("ALTER TABLE webhook_deliveries DROP COLUMN tenant_id", [])
                .expect("drop tenant_id to simulate legacy schema");
            // Insert a legacy row without a tenant_id.
            conn.execute(
                "INSERT INTO webhook_deliveries \
                     (id, event_id, event_type, url, payload_json, status, \
                      attempts, max_attempts, next_attempt_at, \
                      created_at, updated_at) \
                 VALUES ('legacy_1', 'evt_legacy', 'transaction.completed', \
                         'https://hooks.example/legacy', '{}', 'pending', \
                          0, 5, '2026-01-01T00:00:00Z', \
                          '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert legacy row");
        }

        // Second open: additive migration must run + the legacy row
        // must still be readable, with tenant_id defaulted to ''.
        {
            let pool = open(&path_str).expect("open 2 (after schema upgrade)");
            let conn = pool.get().expect("acquire");
            let tenant_id: String = conn
                .query_row(
                    "SELECT tenant_id FROM webhook_deliveries WHERE id = 'legacy_1'",
                    [],
                    |row| row.get(0),
                )
                .expect("legacy row queryable post-migration");
            assert_eq!(
                tenant_id, "",
                "legacy rows must backfill to empty string, not NULL"
            );
        }

        // Third open: rerunning the migration must be a no-op (no
        // duplicate-column error escapes).
        {
            let _pool = open(&path_str).expect("open 3 (idempotent re-migration)");
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "icp-state-db-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
