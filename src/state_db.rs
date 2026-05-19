//! Shared persistent state pool for handler-owned data: mandate spend,
//! signed receipts, in-flight transactions, subscriptions, peer quotes.
//!
//! This is deliberately separate from the iCommerce engine's own database.
//! The handler's state is *protocol-level* — it describes what agents have
//! spent against which mandates, which receipts we have signed, and which
//! transactions are currently quoted/authorized/captured. It must survive
//! handler restarts independently of commerce-engine schema evolution.
//!
//! # Migration model
//!
//! Migrations are an *ordered list* of [`Migration`] entries in
//! [`MIGRATIONS`]. On every pool open we read the `schema_migrations`
//! table to see which versions have already been applied, then run any
//! that haven't. Each step is wrapped in a transaction so a partial
//! apply rolls back cleanly. The applied set is queryable at runtime
//! via [`applied_versions`] — useful for operators verifying a deploy.
//!
//! Steps stay strictly additive (new tables, new columns with
//! `DEFAULT`, new indexes). Breaking changes (`DROP COLUMN`, type
//! changes, complex backfills) get a fresh migration version and a
//! transactional body that can fail closed without leaving the DB
//! half-migrated.
//!
//! Legacy DBs created before this versioning landed are detected by
//! probing for the columns the early migrations added: the runner
//! tolerates "duplicate column name" so a re-run on a fully-migrated
//! DB is a no-op, and stamps the version table on first success so
//! subsequent opens skip straight to the unapplied versions.

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
    let mut conn = pool.get().context("acquire state conn for migration")?;
    run_migrations(&mut conn).context("apply state migrations")?;
    Ok(())
}

/// One migration step. `body` runs inside a transaction; `version` is
/// the monotonic id stamped into `schema_migrations` on success.
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    body: fn(&rusqlite::Transaction<'_>) -> anyhow::Result<()>,
}

/// Ordered migration ladder. Versions are monotonic; never reorder or
/// renumber, even if a step turns out to be wrong — supersede it with
/// a higher version.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        body: migration_v1_initial_schema,
    },
    Migration {
        version: 2,
        name: "tenant_id_columns",
        body: migration_v2_tenant_id_columns,
    },
    Migration {
        version: 3,
        name: "denormalized_query_columns_and_indexes",
        body: migration_v3_query_columns_and_indexes,
    },
];

fn run_migrations(conn: &mut rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\
             version    INTEGER PRIMARY KEY,\
             name       TEXT NOT NULL,\
             applied_at TEXT NOT NULL\
         );",
    )
    .context("create schema_migrations")?;

    let applied = applied_versions_inner(conn)?;
    for m in MIGRATIONS {
        if applied.contains(&m.version) {
            continue;
        }
        let tx = conn
            .transaction()
            .with_context(|| format!("begin tx for migration v{}", m.version))?;
        (m.body)(&tx).with_context(|| format!("run migration v{}: {}", m.version, m.name))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![m.version as i64, m.name, chrono::Utc::now().to_rfc3339()],
        )
        .with_context(|| format!("stamp schema_migrations for v{}", m.version))?;
        tx.commit()
            .with_context(|| format!("commit migration v{}", m.version))?;
        tracing::info!(
            version = m.version,
            name = m.name,
            "state migration applied"
        );
    }
    Ok(())
}

/// Returns the set of migration versions recorded in `schema_migrations`.
/// Empty when the table is absent, which happens on a brand-new DB
/// before the first migration runs.
pub fn applied_versions(pool: &StatePool) -> anyhow::Result<Vec<u32>> {
    let conn = pool.get().context("acquire conn")?;
    applied_versions_inner(&conn)
}

fn applied_versions_inner(conn: &rusqlite::Connection) -> anyhow::Result<Vec<u32>> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .context("probe schema_migrations existence")?;
    if exists == 0 {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version ASC")
        .context("prepare applied_versions")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .context("query applied_versions")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("read applied version row")? as u32);
    }
    Ok(out)
}

// -- Migration bodies ------------------------------------------------------

fn migration_v1_initial_schema(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    tx.execute_batch(SCHEMA_V1)
        .context("apply initial schema")?;
    Ok(())
}

fn migration_v2_tenant_id_columns(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    // tenant_id was added across several tables in lockstep so list/
    // get/retry endpoints can scope by tenant without leaking other
    // tenants' rows. Pre-multi-tenant rows backfill to '' — invisible
    // to any real tenant_id but still queryable by string comparison,
    // which is the desired conservative-default isolation property.
    add_columns_idempotent(
        tx,
        &[
            "ALTER TABLE transactions ADD COLUMN tenant_id TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE subscriptions ADD COLUMN tenant_id TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE peer_quotes ADD COLUMN tenant_id TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE webhook_deliveries ADD COLUMN tenant_id TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE mandate_usage ADD COLUMN tenant_id TEXT NOT NULL DEFAULT ''",
        ],
    )
}

fn migration_v3_query_columns_and_indexes(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    // Denormalized columns the list endpoints filter on, kept in sync
    // by the write path. The JSON payload remains the source of truth;
    // these columns let the index pruner skip cold rows on a tenanted
    // scan.
    add_columns_idempotent(
        tx,
        &[
            "ALTER TABLE transactions ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE transactions ADD COLUMN state TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE transactions ADD COLUMN quote_expires_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE subscriptions ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE subscriptions ADD COLUMN status TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE subscriptions ADD COLUMN next_charge_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE subscriptions ADD COLUMN payment_present INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE peer_quotes ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE peer_quotes ADD COLUMN status TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE peer_quotes ADD COLUMN expires_at TEXT NOT NULL DEFAULT ''",
        ],
    )?;

    const INDEX_ADDITIONS: &[&str] = &[
        "CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_tenant_created \
             ON webhook_deliveries(tenant_id, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_transactions_tenant_created \
             ON transactions(tenant_id, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_transactions_tenant_state_created \
             ON transactions(tenant_id, state, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_transactions_expiry_due \
             ON transactions(state, quote_expires_at)",
        "CREATE INDEX IF NOT EXISTS idx_subscriptions_tenant_created \
             ON subscriptions(tenant_id, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_subscriptions_tenant_status_created \
             ON subscriptions(tenant_id, status, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_subscriptions_renewal_due \
             ON subscriptions(status, payment_present, next_charge_at)",
        "CREATE INDEX IF NOT EXISTS idx_peer_quotes_tenant_created \
             ON peer_quotes(tenant_id, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_peer_quotes_tenant_status_created \
             ON peer_quotes(tenant_id, status, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_peer_quotes_expiry_due \
             ON peer_quotes(status, expires_at)",
    ];
    for stmt in INDEX_ADDITIONS {
        tx.execute(stmt, [])
            .with_context(|| format!("create index: {stmt}"))?;
    }
    Ok(())
}

/// Apply `ALTER TABLE ADD COLUMN` statements, tolerating
/// "duplicate column name" so a step is safely re-runnable on a DB
/// where an older handler already issued the same ALTERs.
fn add_columns_idempotent(
    tx: &rusqlite::Transaction<'_>,
    stmts: &[&'static str],
) -> anyhow::Result<()> {
    for stmt in stmts {
        match tx.execute(stmt, []) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") || msg.starts_with("no such table") => {}
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("additive column migration failed: {stmt}")))
            }
        }
    }
    Ok(())
}

const SCHEMA_V1: &str = r#"
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
    updated_at   TEXT NOT NULL,
    tenant_id    TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL DEFAULT '',
    state        TEXT NOT NULL DEFAULT '',
    quote_expires_at TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS subscriptions (
    id           TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    tenant_id    TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL DEFAULT '',
    status       TEXT NOT NULL DEFAULT '',
    next_charge_at TEXT NOT NULL DEFAULT '',
    payment_present INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS peer_quotes (
    id           TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    tenant_id    TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL DEFAULT '',
    status       TEXT NOT NULL DEFAULT '',
    expires_at   TEXT NOT NULL DEFAULT ''
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

    /// Verifies the migration ladder backfills cleanly onto a DB
    /// created by an older handler that didn't carry a
    /// `schema_migrations` table. Simulates the upgrade path: drop
    /// `tenant_id` (post-condition: schema looks pre-multi-tenant)
    /// AND drop the `schema_migrations` table itself (post-condition:
    /// the runner can't see which versions already ran), insert a
    /// legacy row, then reopen — v2 must re-run and backfill the
    /// column.
    #[test]
    fn legacy_db_without_schema_migrations_applies_all_steps() {
        let dir = tempdir();
        let path = dir.join("legacy.db");
        let path_str = path.to_string_lossy().to_string();

        // First open: full ladder runs and stamps schema_migrations.
        {
            let pool = open(&path_str).expect("open 1");
            let conn = pool.get().expect("acquire");
            conn.execute("DROP INDEX idx_webhook_deliveries_tenant_created", [])
                .expect("drop tenant index");
            conn.execute("ALTER TABLE webhook_deliveries DROP COLUMN tenant_id", [])
                .expect("drop tenant_id to simulate legacy schema");
            // Drop schema_migrations so the runner can't see that v2 +
            // v3 already applied — this is what a *truly* pre-versioning
            // DB looks like to a freshly-deployed handler.
            conn.execute("DROP TABLE schema_migrations", [])
                .expect("drop schema_migrations to simulate pre-versioning DB");
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

        // Second open: ladder re-runs from scratch. Idempotent inside
        // each step keeps it from blowing up on the v1 CREATE TABLE
        // IF NOT EXISTS, the v2 ADD COLUMN, etc. The legacy row must
        // remain readable with tenant_id defaulted to ''.
        {
            let pool = open(&path_str).expect("open 2 (legacy schema upgrade)");
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
            let versions = applied_versions(&pool).expect("applied_versions");
            assert_eq!(
                versions,
                MIGRATIONS.iter().map(|m| m.version).collect::<Vec<_>>(),
                "all migrations stamped after legacy upgrade"
            );
        }

        // Third open: re-running with schema_migrations fully populated
        // applies nothing new.
        {
            let pool = open(&path_str).expect("open 3 (idempotent re-migration)");
            let versions = applied_versions(&pool).expect("applied_versions");
            assert_eq!(versions.len(), MIGRATIONS.len());
        }
    }

    #[test]
    fn applied_versions_lists_every_migration_after_fresh_open() {
        let dir = tempdir();
        let path = dir.join("fresh.db");
        let pool = open(&path.to_string_lossy()).expect("open");
        let versions = applied_versions(&pool).expect("applied_versions");
        let expected: Vec<u32> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert_eq!(versions, expected, "fresh DB stamps every migration");
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
