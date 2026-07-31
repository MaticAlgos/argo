//! Schema and forward-only migrations.
//!
//! Migrations are applied in order inside one transaction each, and the applied
//! version is recorded in `meta`. An upgraded binary opening an older database
//! migrates it; an older binary opening a newer database refuses to run rather
//! than writing rows the newer schema would misread.

use argo_core::error::{ArgoError, Result};
use rusqlite::Connection;

/// Schema version this build understands.
pub const SCHEMA_VERSION: i64 = 2;

/// Ordered migration statements. Index + 1 is the resulting schema version.
const MIGRATIONS: &[&str] = &[
    // v1: canonical conversation store.
    r#"
    CREATE TABLE IF NOT EXISTS workspaces (
        id          TEXT PRIMARY KEY,
        root        TEXT NOT NULL UNIQUE,
        created_at  INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS conversations (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        title                   TEXT,
        -- Delegation lineage: a child conversation records the parent run that
        -- spawned it so the TUI can navigate the session graph.
        parent_conversation_id  TEXT REFERENCES conversations(id) ON DELETE SET NULL,
        parent_run_id           TEXT,
        -- Pending selection applied at the next turn boundary, never mid-run.
        selected_agent_id       TEXT,
        selected_model          TEXT,
        selected_reasoning      TEXT,
        created_at              INTEGER NOT NULL,
        updated_at              INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS messages (
        id               TEXT PRIMARY KEY,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        role             TEXT NOT NULL,
        blocks           TEXT NOT NULL,
        agent_id         TEXT,
        model            TEXT,
        run_id           TEXT,
        seq              INTEGER NOT NULL,
        created_at       INTEGER NOT NULL,
        UNIQUE (conversation_id, seq)
    );

    CREATE TABLE IF NOT EXISTS runs (
        id                    TEXT PRIMARY KEY,
        conversation_id       TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        workspace_id          TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        agent_id              TEXT NOT NULL,
        model                 TEXT,
        status                TEXT NOT NULL,
        assistant_message_id  TEXT,
        parent_run_id         TEXT REFERENCES runs(id) ON DELETE SET NULL,
        resumed               INTEGER NOT NULL DEFAULT 0,
        invalidation_reason   TEXT,
        error_code            TEXT,
        error_message         TEXT,
        created_at            INTEGER NOT NULL,
        finished_at           INTEGER
    );

    CREATE TABLE IF NOT EXISTS run_events (
        run_id   TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        seq      INTEGER NOT NULL,
        at       INTEGER NOT NULL,
        payload  TEXT NOT NULL,
        PRIMARY KEY (run_id, seq)
    );

    -- One upstream handle per (conversation, agent). This is what allows a
    -- conversation to hold live sessions on several CLIs at once.
    CREATE TABLE IF NOT EXISTS agent_sessions (
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        agent_id         TEXT NOT NULL,
        session_id       TEXT NOT NULL,
        model            TEXT,
        cwd              TEXT,
        stable_hash      TEXT,
        last_message_id  TEXT,
        updated_at       INTEGER NOT NULL,
        PRIMARY KEY (conversation_id, agent_id)
    );

    CREATE TABLE IF NOT EXISTS context_epochs (
        id               TEXT PRIMARY KEY,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        summary          TEXT,
        compacted_upto   INTEGER NOT NULL DEFAULT 0,
        reason           TEXT NOT NULL,
        created_at       INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_messages_conversation_seq
        ON messages (conversation_id, seq);
    CREATE INDEX IF NOT EXISTS idx_messages_run
        ON messages (run_id);
    CREATE INDEX IF NOT EXISTS idx_runs_conversation
        ON runs (conversation_id, created_at);
    CREATE INDEX IF NOT EXISTS idx_runs_status
        ON runs (status);
    CREATE INDEX IF NOT EXISTS idx_runs_parent
        ON runs (parent_run_id);
    CREATE INDEX IF NOT EXISTS idx_conversations_workspace
        ON conversations (workspace_id, updated_at);
    CREATE INDEX IF NOT EXISTS idx_conversations_parent
        ON conversations (parent_conversation_id);
    CREATE INDEX IF NOT EXISTS idx_epochs_conversation
        ON context_epochs (conversation_id, created_at);
    "#,
    // v2: execution mode, selected per conversation and applied at the next turn.
    r#"
    ALTER TABLE conversations ADD COLUMN selected_mode TEXT;
    "#,
];

/// Applies pragmas required for correctness and concurrent reads.
pub(crate) fn apply_pragmas(conn: &Connection) -> Result<()> {
    // WAL lets the TUI read while a run writes. NORMAL synchronous is the
    // standard WAL pairing: durable across process crashes, which is the failure
    // mode that matters here.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| ArgoError::Store(format!("set journal_mode: {e}")))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| ArgoError::Store(format!("set synchronous: {e}")))?;
    // Cascades in this schema are load-bearing, so foreign keys must be on.
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|e| ArgoError::Store(format!("enable foreign_keys: {e}")))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| ArgoError::Store(format!("set busy_timeout: {e}")))?;
    Ok(())
}

/// Reads the recorded schema version, or 0 for a fresh database.
fn current_version(conn: &Connection) -> Result<i64> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )
    .map_err(|e| ArgoError::Store(format!("create meta: {e}")))?;

    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .ok();

    Ok(value.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0))
}

/// Brings the database up to [`SCHEMA_VERSION`].
///
/// Refuses to open a database created by a newer Argo, because silently reading
/// an unknown schema risks writing rows the newer build cannot interpret.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    apply_pragmas(conn)?;
    let from = current_version(conn)?;

    if from > SCHEMA_VERSION {
        return Err(ArgoError::Store(format!(
            "database schema v{from} is newer than this build (v{SCHEMA_VERSION}); upgrade Argo"
        )));
    }

    for (idx, script) in MIGRATIONS.iter().enumerate() {
        let target = idx as i64 + 1;
        if target <= from {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|e| ArgoError::Store(format!("begin migration {target}: {e}")))?;
        tx.execute_batch(script)
            .map_err(|e| ArgoError::Store(format!("apply migration {target}: {e}")))?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [target.to_string()],
        )
        .map_err(|e| ArgoError::Store(format!("record migration {target}: {e}")))?;
        tx.commit()
            .map_err(|e| ArgoError::Store(format!("commit migration {target}: {e}")))?;
        tracing::info!(version = target, "applied schema migration");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_in_memory().expect("open memory db")
    }

    #[test]
    fn migrates_a_fresh_database_to_current_version() {
        let mut conn = mem();
        migrate(&mut conn).expect("migrate");
        assert_eq!(current_version(&conn).expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn migration_is_idempotent() {
        let mut conn = mem();
        migrate(&mut conn).expect("first");
        migrate(&mut conn).expect("second");
        assert_eq!(current_version(&conn).expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn refuses_a_future_schema_instead_of_corrupting_it() {
        let mut conn = mem();
        migrate(&mut conn).expect("migrate");
        conn.execute(
            "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
            [],
        )
        .expect("bump");
        let err = migrate(&mut conn).expect_err("must refuse");
        assert!(err.to_string().contains("newer than this build"));
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let mut conn = mem();
        migrate(&mut conn).expect("migrate");
        // Inserting a conversation for a non-existent workspace must fail rather
        // than creating an orphan the rest of the engine would trip over.
        let result = conn.execute(
            "INSERT INTO conversations (id, workspace_id, created_at, updated_at)
             VALUES ('c1', 'missing-workspace', 0, 0)",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn an_existing_v1_database_gains_the_mode_column() {
        // Upgrading must not require the user to discard their history.
        let mut conn = mem();
        conn.execute_batch(MIGRATIONS[0]).expect("v1 schema");
        conn.execute(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .expect("meta");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .expect("record v1");

        migrate(&mut conn).expect("migrate to v2");
        assert_eq!(current_version(&conn).expect("version"), 2);
        let columns: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('conversations') WHERE name = 'selected_mode'",
                [],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(columns, 1);
    }

    #[test]
    fn expected_tables_exist() {
        let mut conn = mem();
        migrate(&mut conn).expect("migrate");
        for table in [
            "workspaces",
            "conversations",
            "messages",
            "runs",
            "run_events",
            "agent_sessions",
            "context_epochs",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query");
            assert_eq!(count, 1, "missing table {table}");
        }
    }
}
