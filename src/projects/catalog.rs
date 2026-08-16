use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::classifier;
use super::domain::{
    AdapterScanStatus, CandidateField, IndexedSessionSummary, PendingSemanticSession,
    ProjectClassification, ProjectKind, ProjectSummary, ProjectsSnapshot, SemanticAssignment,
    SessionCandidate, SessionCursor, SessionIdentity, SessionRefKind, PROJECTS_SCHEMA_VERSION,
};

const CATALOG_SCHEMA_VERSION: u32 = 2;

/// Keyset page over one project's sessions.
///
/// `CROSS JOIN` pins `sessions` as the outer table so the ordering comes straight from
/// `sessions_total_order`. Letting SQLite drive from `assignments` instead makes every page
/// re-sort the whole project in a temp B-tree, which is what
/// `sessions_page_query_uses_the_total_order_index` guards against.
const SESSIONS_PAGE_SQL: &str = "SELECT s.stable_key, s.backend, s.ref_kind, s.title, s.cwd,
            s.first_activity_at, s.last_activity_at,
            r.workspace_id, r.pane_id, r.generation
     FROM sessions s
     CROSS JOIN assignments a ON a.session_key = s.stable_key
     LEFT JOIN runtime_mappings r ON r.session_key = s.stable_key
     WHERE a.project_id = ?1
       AND (?2 IS NULL OR s.last_activity_at < ?2
            OR (s.last_activity_at = ?2 AND s.stable_key > ?3))
     ORDER BY s.last_activity_at DESC, s.stable_key ASC
     LIMIT ?4";

/// Keyset page over one semantic topic's sessions.
///
/// Topic membership is deliberately read from `semantic_assignments`; directory ownership stays
/// in `assignments` and is never replaced by an inferred topic.
const TOPIC_SESSIONS_PAGE_SQL: &str = "SELECT s.stable_key, s.backend, s.ref_kind, s.title, s.cwd,
            s.first_activity_at, s.last_activity_at,
            r.workspace_id, r.pane_id, r.generation
     FROM sessions s
     CROSS JOIN semantic_assignments sa ON sa.session_key = s.stable_key
     LEFT JOIN runtime_mappings r ON r.session_key = s.stable_key
     WHERE sa.topic_key = ?1
       AND (?2 IS NULL OR s.last_activity_at < ?2
            OR (s.last_activity_at = ?2 AND s.stable_key > ?3))
     ORDER BY s.last_activity_at DESC, s.stable_key ASC
     LIMIT ?4";

#[derive(Debug)]
pub(crate) enum CatalogError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Corrupt,
    UnsupportedSchema(u32),
    AliasConflict,
    CrossBackendAlias,
    NotFound,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "catalog sqlite error: {error}"),
            Self::Io(error) => write!(f, "catalog io error: {error}"),
            Self::Corrupt => f.write_str("catalog integrity check failed"),
            Self::UnsupportedSchema(version) => {
                write!(f, "unsupported catalog schema version {version}")
            }
            Self::AliasConflict => f.write_str("session alias belongs to another session"),
            Self::CrossBackendAlias => {
                f.write_str("session alias backend differs from primary backend")
            }
            Self::NotFound => f.write_str("catalog record not found"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<rusqlite::Error> for CatalogError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for CatalogError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanCompletion {
    Complete,
    NotInstalled,
    RootUnavailable,
    ConfigurationError,
    UnsupportedFormat,
    // Reserved by the Catalog completion protocol; the current one-shot scanner cannot cancel yet.
    #[allow(dead_code)]
    Cancelled,
    Degraded,
    Failed,
}

impl ScanCompletion {
    fn is_complete(self) -> bool {
        self == Self::Complete
    }

    pub(crate) fn state(self) -> &'static str {
        match self {
            Self::Complete => "ready",
            Self::NotInstalled => "not_installed",
            Self::RootUnavailable => "root_unavailable",
            Self::ConfigurationError => "degraded",
            Self::UnsupportedFormat => "unsupported_format",
            Self::Cancelled => "cancelled",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn diagnostic_category(self) -> Option<&'static str> {
        match self {
            Self::Complete | Self::NotInstalled => None,
            Self::RootUnavailable => Some("root_unavailable"),
            Self::ConfigurationError => Some("configuration_error"),
            Self::UnsupportedFormat => Some("unsupported_format"),
            Self::Cancelled => Some("cancelled"),
            Self::Degraded => Some("malformed_input"),
            Self::Failed => Some("scan_failed"),
        }
    }
}

pub(crate) struct ProjectCatalog {
    connection: Connection,
}

impl ProjectCatalog {
    pub(crate) fn open(path: &Path) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Self::initialize(connection, true)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self, CatalogError> {
        Self::initialize(Connection::open_in_memory()?, false)
    }

    fn initialize(connection: Connection, use_wal: bool) -> Result<Self, CatalogError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        if use_wal {
            connection.pragma_update(None, "journal_mode", "WAL")?;
        }
        let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(CatalogError::Corrupt);
        }
        let mut catalog = Self { connection };
        catalog.migrate()?;
        catalog.restore_legacy_semantic_assignments()?;
        catalog.exclude_ephemeral_agent_assignments()?;
        Ok(catalog)
    }

    fn migrate(&mut self) -> Result<(), CatalogError> {
        let version: u32 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > CATALOG_SCHEMA_VERSION {
            return Err(CatalogError::UnsupportedSchema(version));
        }
        if version == CATALOG_SCHEMA_VERSION {
            return Ok(());
        }
        if version == 1 {
            return self.migrate_v1_to_v2();
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            r#"
            CREATE TABLE catalog_meta (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            INSERT INTO catalog_meta(key, value) VALUES ('revision', 0);

            CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                canonical_key TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL,
                canonical_path TEXT NOT NULL,
                display_name TEXT NOT NULL,
                manual INTEGER NOT NULL DEFAULT 0 CHECK (manual IN (0, 1)),
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE sessions (
                stable_key TEXT PRIMARY KEY,
                backend TEXT NOT NULL,
                ref_kind TEXT NOT NULL CHECK (ref_kind IN ('id', 'path')),
                ref_value TEXT NOT NULL,
                title TEXT NOT NULL,
                title_observed_at INTEGER NOT NULL,
                title_priority INTEGER NOT NULL,
                title_source_key TEXT NOT NULL,
                cwd TEXT,
                cwd_observed_at INTEGER,
                cwd_priority INTEGER,
                cwd_source_key TEXT,
                transcript_ref TEXT,
                transcript_observed_at INTEGER,
                transcript_priority INTEGER,
                transcript_source_key TEXT,
                first_activity_at INTEGER NOT NULL,
                last_activity_at INTEGER NOT NULL,
                -- How much the user actually said, used to skip thin sessions when classifying.
                user_turns INTEGER NOT NULL DEFAULT 0,
                user_chars INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(backend, ref_kind, ref_value)
            );

            CREATE TABLE assignments (
                session_key TEXT PRIMARY KEY REFERENCES sessions(stable_key) ON DELETE CASCADE,
                project_id INTEGER NOT NULL REFERENCES projects(id),
                source TEXT NOT NULL,
                evidence TEXT NOT NULL,
                locked INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0, 1)),
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE session_aliases (
                alias_backend TEXT NOT NULL,
                alias_kind TEXT NOT NULL CHECK (alias_kind IN ('id', 'path')),
                alias_value TEXT NOT NULL,
                primary_stable_key TEXT NOT NULL REFERENCES sessions(stable_key) ON DELETE CASCADE,
                evidence TEXT NOT NULL,
                PRIMARY KEY(alias_backend, alias_kind, alias_value)
            );

            CREATE TRIGGER session_alias_backend_insert
            BEFORE INSERT ON session_aliases
            WHEN (SELECT backend FROM sessions WHERE stable_key = NEW.primary_stable_key)
                 IS NOT NEW.alias_backend
            BEGIN
                SELECT RAISE(ABORT, 'cross-backend session alias');
            END;

            CREATE TRIGGER session_alias_backend_update
            BEFORE UPDATE ON session_aliases
            WHEN (SELECT backend FROM sessions WHERE stable_key = NEW.primary_stable_key)
                 IS NOT NEW.alias_backend
            BEGIN
                SELECT RAISE(ABORT, 'cross-backend session alias');
            END;

            CREATE TABLE session_sources (
                session_key TEXT NOT NULL REFERENCES sessions(stable_key) ON DELETE CASCADE,
                adapter TEXT NOT NULL,
                root_key TEXT NOT NULL,
                source_key TEXT NOT NULL,
                observed_at INTEGER NOT NULL,
                last_seen_generation INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL DEFAULT 'present'
                    CHECK (state IN ('present', 'suspected_missing', 'missing')),
                missing_streak INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(session_key, adapter, root_key, source_key)
            );

            CREATE TABLE scan_roots (
                adapter TEXT NOT NULL,
                root_key TEXT NOT NULL,
                successful_generation INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                last_success_at INTEGER,
                last_error_category TEXT,
                PRIMARY KEY(adapter, root_key)
            );

            CREATE TABLE runtime_mappings (
                session_key TEXT PRIMARY KEY REFERENCES sessions(stable_key) ON DELETE CASCADE,
                workspace_id TEXT NOT NULL,
                pane_id TEXT NOT NULL,
                generation INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- Topic clustering from a local Agent CLI. Kept separate from `assignments` because
            -- these rows are a discardable inference: clearing this table re-runs classification
            -- without touching a single manually locked assignment.
            CREATE TABLE semantic_assignments (
                session_key TEXT PRIMARY KEY REFERENCES sessions(stable_key) ON DELETE CASCADE,
                topic_key TEXT NOT NULL,
                topic_label TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                backend_used TEXT NOT NULL,
                model_used TEXT,
                classified_at INTEGER NOT NULL
            );

            CREATE INDEX sessions_total_order
                ON sessions(last_activity_at DESC, stable_key ASC);
            CREATE INDEX assignments_project
                ON assignments(project_id, session_key);
            CREATE INDEX session_sources_root
                ON session_sources(adapter, root_key, state, missing_streak);
            CREATE INDEX runtime_mappings_pane
                ON runtime_mappings(workspace_id, pane_id, generation);
            CREATE INDEX semantic_topic
                ON semantic_assignments(topic_key);
            CREATE INDEX semantic_fingerprint
                ON semantic_assignments(fingerprint);

            PRAGMA user_version = 2;
            "#,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Adds semantic clustering metadata to Catalogs created by the original v1 schema.
    ///
    /// Some development builds accidentally wrote the new v2 tables while still marking the
    /// database as v1. Check each object independently so those partially upgraded Catalogs also
    /// migrate safely instead of failing on duplicate columns.
    fn migrate_v1_to_v2(&mut self) -> Result<(), CatalogError> {
        let has_user_turns = table_has_column(&self.connection, "sessions", "user_turns")?;
        let has_user_chars = table_has_column(&self.connection, "sessions", "user_chars")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        if !has_user_turns {
            transaction.execute_batch(
                "ALTER TABLE sessions
                 ADD COLUMN user_turns INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if !has_user_chars {
            transaction.execute_batch(
                "ALTER TABLE sessions
                 ADD COLUMN user_chars INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        transaction.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS semantic_assignments (
                session_key TEXT PRIMARY KEY REFERENCES sessions(stable_key) ON DELETE CASCADE,
                topic_key TEXT NOT NULL,
                topic_label TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                backend_used TEXT NOT NULL,
                model_used TEXT,
                classified_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS semantic_topic
                ON semantic_assignments(topic_key);
            CREATE INDEX IF NOT EXISTS semantic_fingerprint
                ON semantic_assignments(fingerprint);
            PRAGMA user_version = 2;
            "#,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Repairs development Catalogs written while semantic topics incorrectly replaced the
    /// filesystem assignment.
    ///
    /// The semantic row is retained. Only the base `assignments` relation is rebuilt from cwd,
    /// so one session can appear in both its directory and its independent topic.
    fn restore_legacy_semantic_assignments(&mut self) -> Result<(), CatalogError> {
        if !table_has_column(&self.connection, "assignments", "source")?
            || !table_has_column(&self.connection, "sessions", "cwd")?
        {
            return Ok(());
        }
        let rows = {
            let mut statement = self.connection.prepare(
                "SELECT a.session_key, s.cwd, a.updated_at
                 FROM assignments a
                 JOIN sessions s ON s.stable_key = a.session_key
                 WHERE a.source = 'semantic'
                 ORDER BY a.session_key ASC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        if rows.is_empty() {
            return Ok(());
        }

        let mut classifications = HashMap::<Option<String>, ProjectClassification>::new();
        for (_, cwd, _) in &rows {
            classifications
                .entry(cwd.clone())
                .or_insert_with(|| classifier::classify(cwd.as_deref().map(Path::new)));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (session_key, cwd, observed_at) in rows {
            let Some(project) = classifications.get(&cwd) else {
                continue;
            };
            let project_id = upsert_project(&transaction, project, observed_at)?;
            transaction.execute(
                "UPDATE assignments
                 SET project_id = ?2, source = 'automatic', evidence = ?3, updated_at = ?4
                 WHERE session_key = ?1 AND source = 'semantic'",
                params![session_key, project_id, project.evidence, observed_at],
            )?;
        }
        bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// Rehomes disposable Paseo/Multica execution cwds into a hidden internal assignment.
    ///
    /// Older Catalogs classified each temporary OpenCode cwd as a standalone directory while it
    /// existed, then classified it as `Unclassified` after macOS removed it. The sessions remain
    /// indexed and available to topic clustering; only the false directory ownership is hidden.
    fn exclude_ephemeral_agent_assignments(&mut self) -> Result<(), CatalogError> {
        if !table_has_column(&self.connection, "assignments", "evidence")?
            || !table_has_column(&self.connection, "assignments", "locked")?
            || !table_has_column(&self.connection, "sessions", "cwd")?
        {
            return Ok(());
        }
        let rows = {
            let mut statement = self.connection.prepare(
                "SELECT a.session_key, s.cwd, a.updated_at
                 FROM assignments a
                 JOIN sessions s ON s.stable_key = a.session_key
                 WHERE a.locked = 0
                   AND a.evidence != 'ephemeral-agent-cwd'
                   AND s.cwd LIKE '%paseo-multica-agent-%'
                 ORDER BY a.session_key ASC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let rows = rows
            .into_iter()
            .filter(|(_, cwd, _)| classifier::is_ephemeral_agent_cwd(Path::new(cwd)))
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(());
        }

        let project = ProjectClassification::ephemeral_agent();
        let observed_at = rows
            .iter()
            .map(|(_, _, observed_at)| *observed_at)
            .max()
            .unwrap_or_default();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let project_id = upsert_project(&transaction, &project, observed_at)?;
        for (session_key, _, updated_at) in rows {
            transaction.execute(
                "UPDATE assignments
                 SET project_id = ?2, source = 'automatic', evidence = ?3, updated_at = ?4
                 WHERE session_key = ?1 AND locked = 0",
                params![session_key, project_id, project.evidence, updated_at],
            )?;
        }
        bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn revision(&self) -> Result<u64, CatalogError> {
        let value: i64 = self.connection.query_row(
            "SELECT value FROM catalog_meta WHERE key = 'revision'",
            [],
            |row| row.get(0),
        )?;
        Ok(value.max(0) as u64)
    }

    pub(crate) fn upsert_candidate(
        &mut self,
        candidate: &SessionCandidate,
    ) -> Result<u64, CatalogError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate = resolve_alias_primary(&transaction, candidate)?;
        let preserve_assignment = can_preserve_existing_assignment(&transaction, &candidate)?;
        let project = (!preserve_assignment).then(|| {
            classifier::classify(candidate.cwd.as_ref().map(|field| field.value.as_path()))
        });
        upsert_session(&transaction, &candidate)?;
        if let Some(project) = project {
            let project_id = upsert_project(&transaction, &project, candidate.observed_at)?;
            upsert_assignment(
                &transaction,
                &candidate.identity.stable_key,
                project_id,
                &project,
                candidate.observed_at,
            )?;
        }
        reconcile_semantic_assignment(&transaction, &candidate.identity.stable_key)?;
        upsert_source(&transaction, &candidate)?;
        upsert_aliases(&transaction, &candidate)?;
        upsert_runtime(&transaction, &candidate)?;
        let revision = bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Commits one scanner chunk in a single transaction.
    ///
    /// Historical scans contain thousands of sessions but usually only a small number of
    /// distinct working directories. Reusing the path classification within the chunk avoids
    /// repeatedly probing the same Git metadata, while one revision per chunk avoids an fsync for
    /// every transcript. Runtime reports still use `upsert_candidate` so interactive updates keep
    /// their existing one-mutation/one-snapshot behaviour.
    pub(crate) fn upsert_scanned_candidates(
        &mut self,
        candidates: &[SessionCandidate],
    ) -> Result<u64, CatalogError> {
        if candidates.is_empty() {
            return self.revision();
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut classifications = HashMap::new();
        for candidate in candidates {
            let candidate = resolve_alias_primary(&transaction, candidate)?;
            let preserve_assignment = can_preserve_existing_assignment(&transaction, &candidate)?;
            let project = if preserve_assignment {
                None
            } else {
                Some(match candidate.cwd.as_ref() {
                    Some(field) => classifications
                        .entry(field.value.clone())
                        .or_insert_with(|| classifier::classify(Some(field.value.as_path())))
                        .clone(),
                    None => ProjectClassification::unclassified(),
                })
            };
            upsert_session(&transaction, &candidate)?;
            if let Some(project) = project {
                let project_id = upsert_project(&transaction, &project, candidate.observed_at)?;
                upsert_assignment(
                    &transaction,
                    &candidate.identity.stable_key,
                    project_id,
                    &project,
                    candidate.observed_at,
                )?;
            }
            reconcile_semantic_assignment(&transaction, &candidate.identity.stable_key)?;
            upsert_source(&transaction, &candidate)?;
            upsert_aliases(&transaction, &candidate)?;
            upsert_runtime(&transaction, &candidate)?;
        }
        let revision = bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(revision)
    }

    pub(crate) fn clear_runtime_mapping(
        &mut self,
        stable_key: &str,
        expected_generation: u64,
    ) -> Result<u64, CatalogError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM runtime_mappings WHERE session_key = ?1 AND generation = ?2",
            params![stable_key, expected_generation as i64],
        )?;
        let revision = bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(revision)
    }

    pub(crate) fn clear_all_runtime_mappings(&mut self) -> Result<u64, CatalogError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute("DELETE FROM runtime_mappings", [])?;
        let revision = if changed == 0 {
            let value: i64 = transaction.query_row(
                "SELECT value FROM catalog_meta WHERE key = 'revision'",
                [],
                |row| row.get(0),
            )?;
            value.max(0) as u64
        } else {
            bump_revision(&transaction)?
        };
        transaction.commit()?;
        Ok(revision)
    }

    pub(crate) fn assign_session(
        &mut self,
        stable_key: &str,
        project_key: &str,
        lock: bool,
        observed_at: i64,
    ) -> Result<u64, CatalogError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let project_id: Option<i64> = transaction
            .query_row(
                "SELECT id FROM projects WHERE canonical_key = ?1",
                [project_key],
                |row| row.get(0),
            )
            .optional()?;
        let project_id = project_id.ok_or(CatalogError::NotFound)?;
        let changed = transaction.execute(
            "UPDATE assignments
             SET project_id = ?2, source = 'manual', evidence = 'manual', locked = ?3,
                 updated_at = ?4
             WHERE session_key = ?1",
            params![stable_key, project_id, i64::from(lock), observed_at],
        )?;
        if changed == 0 {
            return Err(CatalogError::NotFound);
        }
        let revision = bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Sessions whose topic is missing or stale, most recently active first.
    ///
    /// A session is pending when it has no semantic row, or its stored fingerprint no longer
    /// matches its current metadata. Manually locked sessions are excluded outright: the user
    /// already decided where they belong, so spending a classifier call on them is wasted.
    pub(crate) fn pending_semantic_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingSemanticSession>, CatalogError> {
        let mut statement = self.connection.prepare(
            "SELECT s.stable_key, s.title, s.cwd, s.backend, sa.fingerprint
             FROM sessions s
             JOIN assignments a ON a.session_key = s.stable_key
             LEFT JOIN semantic_assignments sa ON sa.session_key = s.stable_key
             WHERE a.locked = 0
               -- Skip throwaway sessions, which otherwise become junk Projects like
               -- \"no clear topic\". Either signal alone is enough to be worth classifying:
               -- a long back-and-forth, or one detailed request. Mirrors
               -- `SessionWeight::is_substantive`.
               AND s.user_chars >= ?3
               AND (s.user_turns >= ?1 OR s.user_chars >= ?2)
             ORDER BY s.last_activity_at DESC, s.stable_key ASC",
        )?;
        let rows = statement
            .query_map(
                params![
                    super::adapters::MIN_SUBSTANTIVE_TURNS as i64,
                    super::adapters::MIN_SUBSTANTIVE_CHARS as i64,
                    super::adapters::MIN_ANY_CHARS as i64
                ],
                |row| {
                    Ok(PendingSemanticSession {
                        stable_key: row.get(0)?,
                        title: row.get(1)?,
                        cwd: row.get(2)?,
                        backend: row.get(3)?,
                        stored_fingerprint: row.get(4)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                let current =
                    super::semantic_fingerprint(&row.title, row.cwd.as_deref(), &row.backend);
                row.stored_fingerprint.as_deref() != Some(current.as_str())
            })
            .take(limit)
            .collect())
    }

    /// Topic labels already in use, most recently assigned first.
    ///
    /// Offered back to the classifier so later batches join an existing topic rather than
    /// coining a near-duplicate name for the same effort.
    pub(crate) fn known_topics(&self, limit: usize) -> Result<Vec<String>, CatalogError> {
        let mut statement = self.connection.prepare(
            "SELECT topic_label, MAX(classified_at) recent
             FROM semantic_assignments
             GROUP BY topic_key
             ORDER BY recent DESC
             LIMIT ?1",
        )?;
        let rows = statement
            .query_map(params![limit as i64], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Applies one classified batch atomically.
    ///
    /// Either every session in the batch lands in its topic Project or none does, so a partial
    /// or malformed classifier response cannot leave sessions scattered across half-built topics.
    /// Directory ownership remains untouched in `assignments`.
    pub(crate) fn apply_semantic_batch(
        &mut self,
        batch: &[SemanticAssignment],
        observed_at: i64,
    ) -> Result<u64, CatalogError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for item in batch {
            // Re-check the lock inside the transaction: the user may have locked this session
            // while the classifier was running.
            let locked: Option<i64> = transaction
                .query_row(
                    "SELECT locked FROM assignments WHERE session_key = ?1",
                    [&item.session_key],
                    |row| row.get(0),
                )
                .optional()?;
            if locked != Some(0) {
                continue;
            }

            let classification = ProjectClassification::new(
                ProjectKind::Semantic,
                item.topic_key.clone(),
                item.topic_label.clone(),
                "semantic".to_string(),
            );
            upsert_project(&transaction, &classification, observed_at)?;
            transaction.execute(
                "INSERT INTO semantic_assignments(session_key, topic_key, topic_label,
                        fingerprint, backend_used, model_used, classified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(session_key) DO UPDATE SET
                   topic_key = excluded.topic_key,
                   topic_label = excluded.topic_label,
                   fingerprint = excluded.fingerprint,
                   backend_used = excluded.backend_used,
                   model_used = excluded.model_used,
                   classified_at = excluded.classified_at",
                params![
                    item.session_key,
                    item.topic_key,
                    item.topic_label,
                    item.fingerprint,
                    item.backend_used,
                    item.model_used,
                    observed_at
                ],
            )?;
        }
        let revision = bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(revision)
    }

    pub(crate) fn unlock_session(
        &mut self,
        stable_key: &str,
        observed_at: i64,
    ) -> Result<u64, CatalogError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE assignments SET locked = 0, updated_at = ?2 WHERE session_key = ?1",
            params![stable_key, observed_at],
        )?;
        if changed == 0 {
            return Err(CatalogError::NotFound);
        }
        let revision = bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(revision)
    }

    pub(crate) fn complete_root_scan(
        &mut self,
        adapter: &str,
        root_key: &str,
        completion: ScanCompletion,
        seen_source_keys: &HashSet<String>,
        observed_at: i64,
    ) -> Result<u64, CatalogError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_generation: i64 = transaction
            .query_row(
                "SELECT successful_generation FROM scan_roots
                 WHERE adapter = ?1 AND root_key = ?2",
                params![adapter, root_key],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);

        if completion.is_complete() {
            let next_generation = current_generation.saturating_add(1);
            transaction.execute(
                "INSERT INTO scan_roots(adapter, root_key, successful_generation, state,
                                        last_success_at, last_error_category)
                 VALUES (?1, ?2, ?3, 'ready', ?4, NULL)
                 ON CONFLICT(adapter, root_key) DO UPDATE SET
                   successful_generation = excluded.successful_generation,
                   state = 'ready', last_success_at = excluded.last_success_at,
                   last_error_category = NULL",
                params![adapter, root_key, next_generation, observed_at],
            )?;

            let mut sources = transaction.prepare(
                "SELECT session_key, source_key FROM session_sources
                 WHERE adapter = ?1 AND root_key = ?2",
            )?;
            let rows = sources
                .query_map(params![adapter, root_key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(sources);
            for (session_key, source_key) in rows {
                if seen_source_keys.contains(&source_key) {
                    transaction.execute(
                        "UPDATE session_sources SET last_seen_generation = ?5, state = 'present',
                         missing_streak = 0, observed_at = MAX(observed_at, ?4)
                         WHERE session_key = ?1 AND adapter = ?2 AND root_key = ?3
                           AND source_key = ?6",
                        params![
                            session_key,
                            adapter,
                            root_key,
                            observed_at,
                            next_generation,
                            source_key
                        ],
                    )?;
                } else {
                    transaction.execute(
                        "UPDATE session_sources SET
                           missing_streak = missing_streak + 1,
                           state = CASE WHEN missing_streak + 1 >= 2
                                        THEN 'missing' ELSE 'suspected_missing' END
                         WHERE session_key = ?1 AND adapter = ?2 AND root_key = ?3
                           AND source_key = ?4",
                        params![session_key, adapter, root_key, source_key],
                    )?;
                }
            }
        } else {
            let diagnostic_category = completion.diagnostic_category();
            transaction.execute(
                "INSERT INTO scan_roots(adapter, root_key, successful_generation, state,
                                        last_success_at, last_error_category)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)
                 ON CONFLICT(adapter, root_key) DO UPDATE SET
                   state = excluded.state, last_error_category = excluded.last_error_category",
                params![
                    adapter,
                    root_key,
                    current_generation,
                    completion.state(),
                    diagnostic_category
                ],
            )?;
        }
        let revision = bump_revision(&transaction)?;
        transaction.commit()?;
        Ok(revision)
    }

    pub(crate) fn snapshot(&self, page_size: usize) -> Result<ProjectsSnapshot, CatalogError> {
        let page_size = page_size.clamp(1, 500);
        let mut projects_statement = self.connection.prepare(
            "SELECT p.id, p.canonical_key, p.kind, p.display_name, p.canonical_path,
                    MAX(s.last_activity_at) AS latest
             FROM projects p
             JOIN assignments a ON a.project_id = p.id
             JOIN sessions s ON s.stable_key = a.session_key
             WHERE p.kind != 'semantic'
               AND a.evidence != 'ephemeral-agent-cwd'
             GROUP BY p.id
             ORDER BY latest DESC, p.canonical_key ASC",
        )?;
        let raw_projects = projects_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(projects_statement);

        let mut name_counts = HashMap::<String, usize>::new();
        for (_, _, _, display_name, _) in &raw_projects {
            *name_counts.entry(display_name.clone()).or_default() += 1;
        }

        let mut projects = Vec::with_capacity(raw_projects.len());
        for (project_id, canonical_key, kind, mut display_name, canonical_path) in raw_projects {
            if name_counts.get(&display_name).copied().unwrap_or_default() > 1 {
                display_name = format!("{display_name} — {canonical_path}");
            }
            let (sessions, next_cursor) = self.sessions_page_by_id(project_id, None, page_size)?;
            projects.push(ProjectSummary {
                canonical_key,
                kind: parse_project_kind(&kind),
                display_name,
                canonical_path,
                sessions,
                next_cursor,
            });
        }

        let mut topics_statement = self.connection.prepare(
            "SELECT sa.topic_key,
                    (SELECT latest.topic_label
                     FROM semantic_assignments latest
                     WHERE latest.topic_key = sa.topic_key
                     ORDER BY latest.classified_at DESC, latest.session_key ASC
                     LIMIT 1) AS topic_label,
                    MAX(s.last_activity_at) AS latest
             FROM semantic_assignments sa
             JOIN sessions s ON s.stable_key = sa.session_key
             GROUP BY sa.topic_key
             ORDER BY latest DESC, sa.topic_key ASC",
        )?;
        let raw_topics = topics_statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(topics_statement);

        let mut topics = Vec::with_capacity(raw_topics.len());
        for (topic_key, topic_label) in raw_topics {
            let classification = ProjectClassification::new(
                ProjectKind::Semantic,
                topic_key.clone(),
                topic_label.clone(),
                "semantic".to_string(),
            );
            let (sessions, next_cursor) = self.topic_sessions_page(&topic_key, None, page_size)?;
            topics.push(ProjectSummary {
                canonical_key: classification.canonical_key,
                kind: ProjectKind::Semantic,
                display_name: topic_label,
                canonical_path: topic_key,
                sessions,
                next_cursor,
            });
        }

        let mut status_statement = self.connection.prepare(
            "SELECT adapter, state, last_error_category FROM scan_roots
             ORDER BY adapter ASC, root_key ASC",
        )?;
        let raw_scan_status = status_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let scan_status = aggregate_scan_status(raw_scan_status);

        Ok(ProjectsSnapshot {
            projects_schema_version: PROJECTS_SCHEMA_VERSION,
            revision: self.revision()?,
            projects,
            topics,
            scan_status,
            diagnostic_category: None,
        })
    }

    pub(crate) fn sessions_page(
        &self,
        project_key: &str,
        cursor: Option<&SessionCursor>,
        limit: usize,
    ) -> Result<(Vec<IndexedSessionSummary>, Option<SessionCursor>), CatalogError> {
        let project: Option<(i64, String, String)> = self
            .connection
            .query_row(
                "SELECT id, kind, canonical_path FROM projects WHERE canonical_key = ?1",
                [project_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (project_id, kind, canonical_path) = project.ok_or(CatalogError::NotFound)?;
        if kind == ProjectKind::Semantic.as_str() {
            self.topic_sessions_page(&canonical_path, cursor, limit)
        } else {
            self.sessions_page_by_id(project_id, cursor, limit)
        }
    }

    fn sessions_page_by_id(
        &self,
        project_id: i64,
        cursor: Option<&SessionCursor>,
        limit: usize,
    ) -> Result<(Vec<IndexedSessionSummary>, Option<SessionCursor>), CatalogError> {
        let limit = limit.clamp(1, 500);
        let cursor_time = cursor.map(|value| value.last_activity_at);
        let cursor_key = cursor.map(|value| value.stable_key.as_str());
        let mut statement = self.connection.prepare(SESSIONS_PAGE_SQL)?;
        let mut sessions = statement
            .query_map(
                params![project_id, cursor_time, cursor_key, (limit + 1) as i64],
                |row| {
                    let ref_kind: String = row.get(2)?;
                    let workspace_id: Option<String> = row.get(7)?;
                    Ok(IndexedSessionSummary {
                        stable_key: row.get(0)?,
                        backend: row.get(1)?,
                        ref_kind: parse_ref_kind(&ref_kind),
                        title: row.get(3)?,
                        cwd: row.get(4)?,
                        first_activity_at: row.get(5)?,
                        last_activity_at: row.get(6)?,
                        live: workspace_id.is_some(),
                        workspace_id,
                        pane_id: row.get(8)?,
                        runtime_generation: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = sessions.len() > limit;
        if has_more {
            sessions.pop();
        }
        let next_cursor = has_more && !sessions.is_empty();
        let next_cursor = next_cursor.then(|| {
            let last = &sessions[sessions.len() - 1];
            SessionCursor {
                last_activity_at: last.last_activity_at,
                stable_key: last.stable_key.clone(),
            }
        });
        Ok((sessions, next_cursor))
    }

    fn topic_sessions_page(
        &self,
        topic_key: &str,
        cursor: Option<&SessionCursor>,
        limit: usize,
    ) -> Result<(Vec<IndexedSessionSummary>, Option<SessionCursor>), CatalogError> {
        let limit = limit.clamp(1, 500);
        let cursor_time = cursor.map(|value| value.last_activity_at);
        let cursor_key = cursor.map(|value| value.stable_key.as_str());
        let mut statement = self.connection.prepare(TOPIC_SESSIONS_PAGE_SQL)?;
        let mut sessions = statement
            .query_map(
                params![topic_key, cursor_time, cursor_key, (limit + 1) as i64],
                |row| {
                    let ref_kind: String = row.get(2)?;
                    let workspace_id: Option<String> = row.get(7)?;
                    Ok(IndexedSessionSummary {
                        stable_key: row.get(0)?,
                        backend: row.get(1)?,
                        ref_kind: parse_ref_kind(&ref_kind),
                        title: row.get(3)?,
                        cwd: row.get(4)?,
                        first_activity_at: row.get(5)?,
                        last_activity_at: row.get(6)?,
                        live: workspace_id.is_some(),
                        workspace_id,
                        pane_id: row.get(8)?,
                        runtime_generation: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = sessions.len() > limit;
        if has_more {
            sessions.pop();
        }
        let next_cursor = (has_more && !sessions.is_empty()).then(|| {
            let last = &sessions[sessions.len() - 1];
            SessionCursor {
                last_activity_at: last.last_activity_at,
                stable_key: last.stable_key.clone(),
            }
        });
        Ok((sessions, next_cursor))
    }

    #[cfg(test)]
    fn table_columns(&self, table: &str) -> Result<Vec<String>, CatalogError> {
        let mut statement = self
            .connection
            .prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |row| row.get(1))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(columns)
    }
}

fn table_has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, CatalogError> {
    let exists = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2
         )",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(exists)
}

fn resolve_alias_primary(
    transaction: &Transaction<'_>,
    candidate: &SessionCandidate,
) -> Result<SessionCandidate, CatalogError> {
    let primary: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT s.backend, s.ref_kind, s.ref_value
             FROM session_aliases a
             JOIN sessions s ON s.stable_key = a.primary_stable_key
             WHERE a.alias_backend = ?1 AND a.alias_kind = ?2 AND a.alias_value = ?3",
            params![
                candidate.identity.backend,
                candidate.identity.ref_kind.as_str(),
                candidate.identity.canonical_ref_value
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let mut resolved = candidate.clone();
    if let Some((backend, ref_kind, canonical_ref_value)) = primary {
        resolved.identity = SessionIdentity::from_canonical(
            backend,
            parse_ref_kind(&ref_kind),
            canonical_ref_value,
        );
    }
    Ok(resolved)
}

fn aggregate_scan_status(rows: Vec<(String, String, Option<String>)>) -> Vec<AdapterScanStatus> {
    let mut grouped = HashMap::<String, Vec<(String, Option<String>)>>::new();
    for (adapter, state, diagnostic) in rows {
        grouped
            .entry(adapter)
            .or_default()
            .push((state, diagnostic));
    }
    let mut adapters = grouped.into_iter().collect::<Vec<_>>();
    adapters.sort_by(|left, right| left.0.cmp(&right.0));
    adapters
        .into_iter()
        .map(|(adapter, states)| {
            let selected = [
                "unsupported_format",
                "failed",
                "degraded",
                "root_unavailable",
                "cancelled",
            ]
            .into_iter()
            .find_map(|wanted| {
                states
                    .iter()
                    .find(|(state, _)| state == wanted)
                    .map(|(state, diagnostic)| (state.clone(), diagnostic.clone()))
            })
            .or_else(|| {
                states
                    .iter()
                    .find(|(state, _)| state == "ready")
                    .map(|(state, diagnostic)| (state.clone(), diagnostic.clone()))
            })
            .unwrap_or_else(|| ("not_installed".to_string(), None));
            let (state, diagnostic_category) =
                if matches!(selected.0.as_str(), "root_unavailable" | "cancelled")
                    && states.iter().any(|(state, _)| state == "ready")
                {
                    ("degraded".to_string(), selected.1)
                } else {
                    selected
                };
            AdapterScanStatus {
                adapter,
                state,
                diagnostic_category,
            }
        })
        .collect()
}

fn upsert_session(
    transaction: &Transaction<'_>,
    candidate: &SessionCandidate,
) -> Result<(), CatalogError> {
    let title = candidate.fallback_title();
    let title_meta = candidate.title.as_ref();
    let title_observed_at = title_meta.map_or(candidate.observed_at, |field| field.observed_at);
    let title_priority = title_meta.map_or(0, |field| field.priority as i64);
    let title_source_key = title_meta
        .map(|field| field.source_key.as_str())
        .unwrap_or(&candidate.source_key);
    transaction.execute(
        "INSERT INTO sessions(
           stable_key, backend, ref_kind, ref_value, title, title_observed_at,
           title_priority, title_source_key, first_activity_at, last_activity_at,
           user_turns, user_chars, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?12, ?13, ?11, ?11)
         ON CONFLICT(stable_key) DO UPDATE SET
           first_activity_at = MIN(first_activity_at, excluded.first_activity_at),
           last_activity_at = MAX(last_activity_at, excluded.last_activity_at),
           -- A runtime report carries no transcript, so never let its zero weight
           -- overwrite what the file scan measured.
           user_turns = MAX(user_turns, excluded.user_turns),
           user_chars = MAX(user_chars, excluded.user_chars),
           updated_at = MAX(updated_at, excluded.updated_at)",
        params![
            candidate.identity.stable_key,
            candidate.identity.backend,
            candidate.identity.ref_kind.as_str(),
            candidate.identity.canonical_ref_value,
            title,
            title_observed_at,
            title_priority,
            title_source_key,
            candidate.first_activity_at,
            candidate.last_activity_at,
            candidate.observed_at,
            candidate.weight.turns as i64,
            candidate.weight.chars as i64
        ],
    )?;

    if let Some(field) = &candidate.title {
        update_string_field(transaction, &candidate.identity.stable_key, "title", field)?;
    }
    if let Some(field) = &candidate.cwd {
        let value = field.value.to_string_lossy().into_owned();
        let field = CandidateField {
            value,
            observed_at: field.observed_at,
            priority: field.priority,
            source_key: field.source_key.clone(),
        };
        update_string_field(transaction, &candidate.identity.stable_key, "cwd", &field)?;
    }
    if let Some(field) = &candidate.transcript_ref {
        update_string_field(
            transaction,
            &candidate.identity.stable_key,
            "transcript_ref",
            field,
        )?;
    }
    Ok(())
}

/// Existing history rows already have a valid path-based assignment. A scanner replay whose cwd
/// is absent or unchanged only needs to refresh metadata (notably semantic weight), so avoid
/// probing the filesystem and rewriting the same assignment thousands of times.
fn can_preserve_existing_assignment(
    transaction: &Transaction<'_>,
    candidate: &SessionCandidate,
) -> Result<bool, CatalogError> {
    let existing = transaction
        .query_row(
            "SELECT s.cwd, p.kind, a.source
             FROM sessions s
             JOIN assignments a ON a.session_key = s.stable_key
             JOIN projects p ON p.id = a.project_id
             WHERE s.stable_key = ?1",
            [&candidate.identity.stable_key],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((existing_cwd, project_kind, assignment_source)) = existing else {
        return Ok(false);
    };
    if project_kind == ProjectKind::Semantic.as_str() || assignment_source == "semantic" {
        return Ok(false);
    }
    let Some(incoming_cwd) = candidate.cwd.as_ref() else {
        return Ok(true);
    };
    Ok(existing_cwd.as_deref() == incoming_cwd.value.to_str())
}

fn reconcile_semantic_assignment(
    transaction: &Transaction<'_>,
    stable_key: &str,
) -> Result<(), CatalogError> {
    let metadata = transaction
        .query_row(
            "SELECT s.title, s.cwd, s.backend, sa.fingerprint
             FROM sessions s
             JOIN semantic_assignments sa ON sa.session_key = s.stable_key
             WHERE s.stable_key = ?1",
            [stable_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((title, cwd, backend, stored_fingerprint)) = metadata else {
        return Ok(());
    };
    let current_fingerprint = super::semantic_fingerprint(&title, cwd.as_deref(), &backend);
    if stored_fingerprint != current_fingerprint {
        transaction.execute(
            "DELETE FROM semantic_assignments WHERE session_key = ?1",
            [stable_key],
        )?;
    }
    Ok(())
}

fn update_string_field(
    transaction: &Transaction<'_>,
    stable_key: &str,
    field_name: &str,
    candidate: &CandidateField<String>,
) -> Result<(), CatalogError> {
    let (observed_column, priority_column, source_column) = match field_name {
        "title" => ("title_observed_at", "title_priority", "title_source_key"),
        "cwd" => ("cwd_observed_at", "cwd_priority", "cwd_source_key"),
        "transcript_ref" => (
            "transcript_observed_at",
            "transcript_priority",
            "transcript_source_key",
        ),
        _ => return Ok(()),
    };
    let query = format!(
        "SELECT COALESCE({observed_column}, -1), COALESCE({priority_column}, -1),
                COALESCE({source_column}, '')
         FROM sessions WHERE stable_key = ?1"
    );
    let existing: (i64, i64, String) = transaction.query_row(&query, [stable_key], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;
    let same_source_reparse = candidate.observed_at == existing.0
        && candidate.priority as i64 == existing.1
        && candidate.source_key == existing.2;
    if !candidate.outranks(existing.0, existing.1, &existing.2) && !same_source_reparse {
        return Ok(());
    }
    let update = format!(
        "UPDATE sessions SET {field_name} = ?2, {observed_column} = ?3,
                {priority_column} = ?4, {source_column} = ?5
         WHERE stable_key = ?1"
    );
    transaction.execute(
        &update,
        params![
            stable_key,
            candidate.value,
            candidate.observed_at,
            candidate.priority as i64,
            candidate.source_key
        ],
    )?;
    Ok(())
}

fn upsert_project(
    transaction: &Transaction<'_>,
    project: &ProjectClassification,
    observed_at: i64,
) -> Result<i64, CatalogError> {
    transaction.execute(
        "INSERT INTO projects(canonical_key, kind, canonical_path, display_name,
                              created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(canonical_key) DO UPDATE SET updated_at = MAX(updated_at, excluded.updated_at)",
        params![
            project.canonical_key,
            project.kind.as_str(),
            project.canonical_path,
            project.display_name,
            observed_at
        ],
    )?;
    Ok(transaction.query_row(
        "SELECT id FROM projects WHERE canonical_key = ?1",
        [&project.canonical_key],
        |row| row.get(0),
    )?)
}

fn upsert_assignment(
    transaction: &Transaction<'_>,
    session_key: &str,
    project_id: i64,
    project: &ProjectClassification,
    observed_at: i64,
) -> Result<(), CatalogError> {
    transaction.execute(
        "INSERT INTO assignments(session_key, project_id, source, evidence, locked, updated_at)
         VALUES (?1, ?2, 'automatic', ?3, 0, ?4)
         ON CONFLICT(session_key) DO UPDATE SET
           project_id = excluded.project_id,
           source = excluded.source,
           evidence = excluded.evidence,
           updated_at = excluded.updated_at
         WHERE assignments.locked = 0",
        params![session_key, project_id, project.evidence, observed_at],
    )?;
    Ok(())
}

fn upsert_source(
    transaction: &Transaction<'_>,
    candidate: &SessionCandidate,
) -> Result<(), CatalogError> {
    transaction.execute(
        "INSERT INTO session_sources(session_key, adapter, root_key, source_key, observed_at,
                                     state, missing_streak)
         VALUES (?1, ?2, ?3, ?4, ?5, 'present', 0)
         ON CONFLICT(session_key, adapter, root_key, source_key) DO UPDATE SET
           observed_at = MAX(observed_at, excluded.observed_at),
           state = 'present', missing_streak = 0",
        params![
            candidate.identity.stable_key,
            candidate.adapter,
            candidate.root_key,
            candidate.source_key,
            candidate.observed_at
        ],
    )?;
    Ok(())
}

fn upsert_aliases(
    transaction: &Transaction<'_>,
    candidate: &SessionCandidate,
) -> Result<(), CatalogError> {
    for alias in &candidate.aliases {
        if alias.identity.backend != candidate.identity.backend {
            return Err(CatalogError::CrossBackendAlias);
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT primary_stable_key FROM session_aliases
                 WHERE alias_backend = ?1 AND alias_kind = ?2 AND alias_value = ?3",
                params![
                    alias.identity.backend,
                    alias.identity.ref_kind.as_str(),
                    alias.identity.canonical_ref_value
                ],
                |row| row.get(0),
            )
            .optional()?;
        if existing
            .as_ref()
            .is_some_and(|stable_key| stable_key != &candidate.identity.stable_key)
        {
            return Err(CatalogError::AliasConflict);
        }
        transaction.execute(
            "INSERT INTO session_aliases(alias_backend, alias_kind, alias_value,
                                         primary_stable_key, evidence)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(alias_backend, alias_kind, alias_value) DO UPDATE SET
               evidence = excluded.evidence",
            params![
                alias.identity.backend,
                alias.identity.ref_kind.as_str(),
                alias.identity.canonical_ref_value,
                candidate.identity.stable_key,
                alias.evidence
            ],
        )?;
    }
    Ok(())
}

fn upsert_runtime(
    transaction: &Transaction<'_>,
    candidate: &SessionCandidate,
) -> Result<(), CatalogError> {
    let Some(runtime) = &candidate.runtime else {
        return Ok(());
    };
    transaction.execute(
        "INSERT INTO runtime_mappings(session_key, workspace_id, pane_id, generation, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session_key) DO UPDATE SET
           workspace_id = excluded.workspace_id, pane_id = excluded.pane_id,
           generation = excluded.generation, updated_at = excluded.updated_at
         WHERE excluded.generation >= runtime_mappings.generation",
        params![
            candidate.identity.stable_key,
            runtime.workspace_id,
            runtime.pane_id,
            runtime.generation as i64,
            candidate.observed_at
        ],
    )?;
    Ok(())
}

fn bump_revision(transaction: &Transaction<'_>) -> Result<u64, CatalogError> {
    transaction.execute(
        "UPDATE catalog_meta SET value = value + 1 WHERE key = 'revision'",
        [],
    )?;
    let value: i64 = transaction.query_row(
        "SELECT value FROM catalog_meta WHERE key = 'revision'",
        [],
        |row| row.get(0),
    )?;
    Ok(value.max(0) as u64)
}

fn parse_ref_kind(value: &str) -> SessionRefKind {
    if value == "path" {
        SessionRefKind::Path
    } else {
        SessionRefKind::Id
    }
}

fn parse_project_kind(value: &str) -> ProjectKind {
    match value {
        "git-common-dir" => ProjectKind::GitCommonDir,
        "cwd" => ProjectKind::Cwd,
        "semantic" => ProjectKind::Semantic,
        _ => ProjectKind::Unclassified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::{
        CandidateField, RuntimeMapping, SessionAliasCandidate, SessionIdentity, SourcePriority,
    };

    fn candidate(backend: &str, id: &str, last: i64) -> SessionCandidate {
        let identity = SessionIdentity::id(backend, id).expect("identity");
        SessionCandidate {
            identity,
            title: Some(CandidateField {
                value: format!("title-{id}"),
                observed_at: last,
                priority: SourcePriority::PrimaryIndex,
                source_key: format!("index-{id}"),
            }),
            cwd: None,
            transcript_ref: None,
            first_activity_at: last.saturating_sub(1),
            last_activity_at: last,
            adapter: backend.to_string(),
            root_key: format!("{backend}-root"),
            source_key: format!("source-{id}"),
            observed_at: last,
            aliases: Vec::new(),
            runtime: None,
            weight: Default::default(),
        }
    }

    #[test]
    fn fresh_schema_has_no_fold_columns() {
        let catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let version: u32 = catalog
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version");
        assert_eq!(version, CATALOG_SCHEMA_VERSION);
        for table in [
            "catalog_meta",
            "projects",
            "sessions",
            "assignments",
            "session_aliases",
            "session_sources",
            "scan_roots",
            "runtime_mappings",
            "semantic_assignments",
        ] {
            let columns = catalog.table_columns(table).expect("columns");
            assert!(columns.iter().all(|column| !column.contains("fold")));
            assert!(columns.iter().all(|column| !column.contains("collapse")));
        }
    }

    #[test]
    fn legacy_v1_catalog_is_upgraded_without_losing_sessions() {
        let connection = Connection::open_in_memory().expect("legacy database");
        connection
            .execute_batch(
                r#"
                CREATE TABLE sessions (
                    stable_key TEXT PRIMARY KEY
                );
                INSERT INTO sessions(stable_key) VALUES ('legacy-session');
                PRAGMA user_version = 1;
                "#,
            )
            .expect("legacy schema");

        let catalog = ProjectCatalog::initialize(connection, false).expect("migrate v1");
        let columns = catalog.table_columns("sessions").expect("session columns");
        assert!(columns.iter().any(|column| column == "user_turns"));
        assert!(columns.iter().any(|column| column == "user_chars"));
        assert!(catalog
            .table_columns("semantic_assignments")
            .expect("semantic table")
            .iter()
            .any(|column| column == "topic_key"));
        let preserved: i64 = catalog
            .connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("preserved sessions");
        assert_eq!(preserved, 1);
        let version: u32 = catalog
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("migrated version");
        assert_eq!(version, CATALOG_SCHEMA_VERSION);
    }

    #[test]
    fn partially_upgraded_v1_catalog_migrates_idempotently() {
        let connection = Connection::open_in_memory().expect("partial database");
        connection
            .execute_batch(
                r#"
                CREATE TABLE sessions (
                    stable_key TEXT PRIMARY KEY,
                    user_turns INTEGER NOT NULL DEFAULT 0,
                    user_chars INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE semantic_assignments (
                    session_key TEXT PRIMARY KEY REFERENCES sessions(stable_key) ON DELETE CASCADE,
                    topic_key TEXT NOT NULL,
                    topic_label TEXT NOT NULL,
                    fingerprint TEXT NOT NULL,
                    backend_used TEXT NOT NULL,
                    model_used TEXT,
                    classified_at INTEGER NOT NULL
                );
                PRAGMA user_version = 1;
                "#,
            )
            .expect("partial v1 schema");

        let catalog = ProjectCatalog::initialize(connection, false).expect("migrate partial v1");
        let version: u32 = catalog
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("migrated version");
        assert_eq!(version, CATALOG_SCHEMA_VERSION);
        let index_count: i64 = catalog
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name IN ('semantic_topic', 'semantic_fingerprint')",
                [],
                |row| row.get(0),
            )
            .expect("semantic indexes");
        assert_eq!(index_count, 2);
    }

    #[test]
    fn known_topics_are_distinct_recent_and_bounded() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let seed = |catalog: &mut ProjectCatalog, id: &str, topic: &str, observed_at: i64| {
            let item = candidate("codex", id, observed_at);
            let session_key = item.identity.stable_key.clone();
            catalog.upsert_candidate(&item).expect("upsert session");
            catalog
                .apply_semantic_batch(
                    &[SemanticAssignment {
                        session_key,
                        topic_key: super::super::semantic_topic_key(topic),
                        topic_label: topic.to_string(),
                        fingerprint: format!("fingerprint-{id}"),
                        backend_used: "test".to_string(),
                        model_used: None,
                    }],
                    observed_at,
                )
                .expect("apply semantic topic");
        };

        seed(&mut catalog, "old-a", "主题 A", 10);
        seed(&mut catalog, "topic-b", "主题 B", 20);
        seed(&mut catalog, "new-a", "主题 A", 30);

        assert_eq!(
            catalog.known_topics(2).expect("known topics"),
            vec!["主题 A".to_string(), "主题 B".to_string()]
        );
        assert_eq!(
            catalog.known_topics(1).expect("bounded topics"),
            vec!["主题 A".to_string()]
        );
    }

    #[test]
    fn directory_and_topic_snapshots_are_independent_and_descending() {
        let root = std::env::temp_dir().join(format!(
            "herdr-project-parallel-groups-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let older_dir = root.join("older");
        let newer_dir = root.join("newer");
        std::fs::create_dir_all(&older_dir).expect("older dir");
        std::fs::create_dir_all(&newer_dir).expect("newer dir");

        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let mut older = candidate("codex", "older", 10);
        older.cwd = Some(CandidateField {
            value: older_dir.clone(),
            observed_at: 10,
            priority: SourcePriority::PrimaryIndex,
            source_key: "older-cwd".into(),
        });
        let mut newer = candidate("pi", "newer", 30);
        newer.cwd = Some(CandidateField {
            value: newer_dir.clone(),
            observed_at: 30,
            priority: SourcePriority::PrimaryIndex,
            source_key: "newer-cwd".into(),
        });
        catalog.upsert_candidate(&older).expect("older upsert");
        catalog.upsert_candidate(&newer).expect("newer upsert");

        let topic_key = super::super::semantic_topic_key("ORK3 repair");
        catalog
            .apply_semantic_batch(
                &[
                    SemanticAssignment {
                        session_key: older.identity.stable_key.clone(),
                        topic_key: topic_key.clone(),
                        topic_label: "ORK3 repair".into(),
                        fingerprint: super::super::semantic_fingerprint(
                            older.title.as_ref().unwrap().value.as_str(),
                            older_dir.to_str(),
                            "codex",
                        ),
                        backend_used: "test".into(),
                        model_used: None,
                    },
                    SemanticAssignment {
                        session_key: newer.identity.stable_key.clone(),
                        topic_key,
                        topic_label: "ORK3 repair".into(),
                        fingerprint: super::super::semantic_fingerprint(
                            newer.title.as_ref().unwrap().value.as_str(),
                            newer_dir.to_str(),
                            "pi",
                        ),
                        backend_used: "test".into(),
                        model_used: None,
                    },
                ],
                30,
            )
            .expect("semantic batch");

        let snapshot = catalog.snapshot(50).expect("snapshot");
        assert_eq!(snapshot.projects.len(), 2);
        assert_eq!(
            snapshot.projects[0].sessions[0].stable_key,
            newer.identity.stable_key
        );
        assert_eq!(
            snapshot.projects[1].sessions[0].stable_key,
            older.identity.stable_key
        );
        assert_eq!(snapshot.topics.len(), 1);
        assert_eq!(snapshot.topics[0].display_name, "ORK3 repair");
        assert_eq!(snapshot.topics[0].sessions.len(), 2);
        assert_eq!(
            snapshot.topics[0].sessions[0].stable_key,
            newer.identity.stable_key
        );
        assert_eq!(
            snapshot.topics[0].sessions[1].stable_key,
            older.identity.stable_key
        );
        assert!(snapshot
            .projects
            .iter()
            .all(|project| project.kind != ProjectKind::Semantic));

        let (first_topic_page, cursor) = catalog
            .sessions_page(&snapshot.topics[0].canonical_key, None, 1)
            .expect("first topic page");
        assert_eq!(first_topic_page[0].stable_key, newer.identity.stable_key);
        let (second_topic_page, next) = catalog
            .sessions_page(&snapshot.topics[0].canonical_key, cursor.as_ref(), 1)
            .expect("second topic page");
        assert_eq!(second_topic_page[0].stable_key, older.identity.stable_key);
        assert!(next.is_none());

        let sources = catalog
            .connection
            .prepare("SELECT DISTINCT source FROM assignments ORDER BY source")
            .expect("source query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("source rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("sources");
        assert_eq!(sources, vec!["automatic".to_string()]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn opening_catalog_restores_legacy_semantic_overwrite_without_losing_topic() {
        let root = std::env::temp_dir().join(format!(
            "herdr-project-legacy-semantic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("legacy cwd");
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let mut item = candidate("codex", "legacy-semantic", 20);
        item.cwd = Some(CandidateField {
            value: root.clone(),
            observed_at: 20,
            priority: SourcePriority::PrimaryIndex,
            source_key: "legacy-cwd".into(),
        });
        let session_key = item.identity.stable_key.clone();
        catalog.upsert_candidate(&item).expect("session");
        catalog
            .apply_semantic_batch(
                &[SemanticAssignment {
                    session_key: session_key.clone(),
                    topic_key: super::super::semantic_topic_key("legacy topic"),
                    topic_label: "legacy topic".into(),
                    fingerprint: super::super::semantic_fingerprint(
                        item.title.as_ref().unwrap().value.as_str(),
                        root.to_str(),
                        "codex",
                    ),
                    backend_used: "test".into(),
                    model_used: None,
                }],
                20,
            )
            .expect("topic");
        let semantic_project_id: i64 = catalog
            .connection
            .query_row(
                "SELECT id FROM projects WHERE kind = 'semantic' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("semantic project");
        catalog
            .connection
            .execute(
                "UPDATE assignments
                 SET project_id = ?2, source = 'semantic', evidence = 'semantic'
                 WHERE session_key = ?1",
                params![session_key, semantic_project_id],
            )
            .expect("seed legacy overwrite");

        let ProjectCatalog { connection } = catalog;
        let restored = ProjectCatalog::initialize(connection, false).expect("restore legacy");
        let snapshot = restored.snapshot(50).expect("snapshot");
        assert_eq!(snapshot.projects.len(), 1);
        assert_ne!(snapshot.projects[0].kind, ProjectKind::Semantic);
        assert_eq!(snapshot.topics.len(), 1);
        assert_eq!(snapshot.topics[0].display_name, "legacy topic");
        let source: String = restored
            .connection
            .query_row(
                "SELECT source FROM assignments WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .expect("restored source");
        assert_eq!(source, "automatic");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ephemeral_agent_cwd_is_kept_out_of_directory_snapshot() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let mut item = candidate("opencode", "ephemeral-current", 20);
        item.cwd = Some(CandidateField {
            value: std::env::temp_dir().join("paseo-multica-agent-current-test"),
            observed_at: 20,
            priority: SourcePriority::PrimaryIndex,
            source_key: "opencode-index".into(),
        });
        let session_key = item.identity.stable_key.clone();

        catalog.upsert_candidate(&item).expect("session");

        let snapshot = catalog.snapshot(50).expect("snapshot");
        assert!(snapshot.projects.is_empty());
        let evidence: String = catalog
            .connection
            .query_row(
                "SELECT evidence FROM assignments WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .expect("assignment evidence");
        assert_eq!(evidence, "ephemeral-agent-cwd");
    }

    #[test]
    fn catalog_open_repairs_existing_paseo_temp_directory_assignment() {
        let root = std::env::temp_dir().join(format!(
            "herdr-project-legacy-paseo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("legacy cwd");
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let mut item = candidate("opencode", "ephemeral-legacy", 20);
        item.cwd = Some(CandidateField {
            value: root.clone(),
            observed_at: 20,
            priority: SourcePriority::PrimaryIndex,
            source_key: "opencode-index".into(),
        });
        let session_key = item.identity.stable_key.clone();
        catalog.upsert_candidate(&item).expect("session");
        assert_eq!(
            catalog.snapshot(50).expect("before repair").projects.len(),
            1
        );

        let disposable = std::env::temp_dir().join("paseo-multica-agent-legacy-test");
        catalog
            .connection
            .execute(
                "UPDATE sessions SET cwd = ?2 WHERE stable_key = ?1",
                params![session_key, disposable.to_string_lossy()],
            )
            .expect("seed disposable cwd");

        let ProjectCatalog { connection } = catalog;
        let repaired = ProjectCatalog::initialize(connection, false).expect("repair catalog");
        assert!(repaired
            .snapshot(50)
            .expect("after repair")
            .projects
            .is_empty());
        let evidence: String = repaired
            .connection
            .query_row(
                "SELECT evidence FROM assignments WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .expect("assignment evidence");
        assert_eq!(evidence, "ephemeral-agent-cwd");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_report_and_file_scan_dedupe_and_merge() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let mut scanned = candidate("codex", "same", 20);
        scanned.title.as_mut().unwrap().priority = SourcePriority::PrimaryIndex;
        catalog.upsert_candidate(&scanned).expect("scan upsert");

        let mut runtime = candidate("codex", "same", 10);
        runtime.title = Some(CandidateField {
            value: "runtime-title".to_string(),
            observed_at: 20,
            priority: SourcePriority::RuntimeReport,
            source_key: "runtime".to_string(),
        });
        runtime.adapter = "runtime".to_string();
        runtime.runtime = Some(RuntimeMapping {
            workspace_id: "w1".to_string(),
            pane_id: "p1".to_string(),
            generation: 7,
        });
        catalog.upsert_candidate(&runtime).expect("runtime upsert");

        let snapshot = catalog.snapshot(50).expect("snapshot");
        assert_eq!(snapshot.projects.len(), 1);
        assert_eq!(snapshot.projects[0].sessions.len(), 1);
        let session = &snapshot.projects[0].sessions[0];
        assert_eq!(session.title, "runtime-title");
        assert_eq!(session.first_activity_at, 9);
        assert_eq!(session.last_activity_at, 20);
        assert!(session.live);
        assert_eq!(session.runtime_generation, Some(7));
    }

    #[test]
    fn same_source_reparse_refreshes_title_and_invalidates_semantic_fingerprint() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let mut dirty = candidate("codex", "reparsed", 20);
        dirty.title.as_mut().unwrap().value =
            "# AGENTS.md instructions for /tmp/project <INSTRUCTIONS>".to_string();
        dirty.weight = crate::projects::adapters::SessionWeight {
            turns: 4,
            chars: 120,
        };
        let session_key = dirty.identity.stable_key.clone();
        catalog.upsert_candidate(&dirty).expect("dirty upsert");
        catalog
            .apply_semantic_batch(
                &[SemanticAssignment {
                    session_key: session_key.clone(),
                    topic_key: super::super::semantic_topic_key("old topic"),
                    topic_label: "old topic".to_string(),
                    fingerprint: super::super::semantic_fingerprint(
                        &dirty.title.as_ref().unwrap().value,
                        None,
                        "codex",
                    ),
                    backend_used: "test".to_string(),
                    model_used: None,
                }],
                20,
            )
            .expect("dirty semantic assignment");

        catalog
            .upsert_candidate(&dirty)
            .expect("reconcile current semantic assignment");
        let reconciled_source: String = catalog
            .connection
            .query_row(
                "SELECT source FROM assignments WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .expect("reconciled assignment source");
        assert_eq!(reconciled_source, "automatic");
        let snapshot = catalog.snapshot(50).expect("parallel snapshot");
        assert_eq!(snapshot.projects.len(), 1);
        assert_eq!(snapshot.topics.len(), 1);
        assert_eq!(snapshot.topics[0].display_name, "old topic");

        let mut clean = dirty.clone();
        clean.title.as_mut().unwrap().value = "修复 ORK3 Projects 自动分类".to_string();
        catalog.upsert_candidate(&clean).expect("clean reparse");

        let snapshot = catalog.snapshot(50).expect("snapshot");
        assert_eq!(
            snapshot.projects[0].sessions[0].title,
            "修复 ORK3 Projects 自动分类"
        );
        assert!(snapshot.topics.is_empty());
        let pending = catalog
            .pending_semantic_sessions(10)
            .expect("stale semantic query");
        assert!(pending
            .iter()
            .any(|session| session.stable_key == session_key));
        let semantic_count: i64 = catalog
            .connection
            .query_row(
                "SELECT COUNT(*) FROM semantic_assignments WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .expect("semantic row count");
        assert_eq!(semantic_count, 0);
        let assignment_source: String = catalog
            .connection
            .query_row(
                "SELECT source FROM assignments WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .expect("assignment source");
        assert_eq!(assignment_source, "automatic");

        catalog
            .connection
            .execute(
                "UPDATE assignments SET source = 'semantic', evidence = 'semantic'
                 WHERE session_key = ?1",
                [&session_key],
            )
            .expect("seed orphan semantic assignment");
        catalog
            .upsert_candidate(&clean)
            .expect("repair orphan semantic assignment");
        let repaired_source: String = catalog
            .connection
            .query_row(
                "SELECT source FROM assignments WHERE session_key = ?1",
                [&session_key],
                |row| row.get(0),
            )
            .expect("repaired assignment source");
        assert_eq!(repaired_source, "automatic");
    }

    #[test]
    fn runtime_report_resolves_an_official_alias_to_the_path_primary() {
        let root = std::env::temp_dir().join(format!(
            "herdr-project-alias-runtime-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("alias root");
        let transcript = root.join("session.jsonl");
        std::fs::write(&transcript, "fixture").expect("alias transcript");
        let path_identity =
            SessionIdentity::path("pi", &transcript, &root, std::slice::from_ref(&root), false)
                .expect("path primary");
        let alias = SessionIdentity::id("pi", "official-id").expect("id alias");
        let mut scanned = candidate("pi", "placeholder", 10);
        scanned.identity = path_identity.clone();
        scanned.aliases.push(SessionAliasCandidate {
            identity: alias.clone(),
            evidence: "same pi header".to_string(),
        });

        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        catalog.upsert_candidate(&scanned).expect("path scan");
        let mut runtime = candidate("pi", "official-id", 20);
        runtime.identity = alias;
        runtime.runtime = Some(RuntimeMapping {
            workspace_id: "workspace".to_string(),
            pane_id: "pane".to_string(),
            generation: 3,
        });
        catalog
            .upsert_candidate(&runtime)
            .expect("runtime alias upsert");
        let snapshot = catalog.snapshot(50).expect("snapshot");
        let sessions = snapshot
            .projects
            .iter()
            .flat_map(|project| &project.sessions)
            .collect::<Vec<_>>();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].stable_key, path_identity.stable_key);
        assert!(sessions[0].live);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn identity_matrix_keeps_distinct_native_sessions() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        catalog
            .upsert_candidate(&candidate("codex", "same", 1))
            .unwrap();
        catalog
            .upsert_candidate(&candidate("codex", "different", 2))
            .unwrap();
        catalog
            .upsert_candidate(&candidate("claude", "same", 3))
            .unwrap();
        let snapshot = catalog.snapshot(50).unwrap();
        assert_eq!(snapshot.projects[0].sessions.len(), 3);
    }

    #[test]
    fn total_order_keyset_handles_equal_timestamps_without_gaps() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        for index in 0..100 {
            catalog
                .upsert_candidate(&candidate("codex", &format!("s-{index:03}"), 10))
                .expect("upsert");
        }
        let project_key = catalog.snapshot(1).unwrap().projects[0]
            .canonical_key
            .clone();
        let mut cursor = None;
        let mut keys = Vec::new();
        loop {
            let (page, next) = catalog
                .sessions_page(&project_key, cursor.as_ref(), 37)
                .expect("page");
            keys.extend(page.into_iter().map(|session| session.stable_key));
            let Some(next) = next else {
                break;
            };
            cursor = Some(next);
        }
        assert_eq!(keys.len(), 100);
        let unique = keys.iter().collect::<HashSet<_>>();
        assert_eq!(unique.len(), 100);
    }

    #[test]
    fn ten_thousand_session_paging_is_ordered_unique_and_under_one_second() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        // Deliberately repeat timestamps so paging must lean on the tiebreaker, not just time.
        for index in 0..10_000 {
            catalog
                .upsert_candidate(&candidate(
                    "codex",
                    &format!("s-{index:05}"),
                    1_000 + (index as i64 / 4),
                ))
                .expect("upsert");
        }
        let project_key = catalog.snapshot(1).unwrap().projects[0]
            .canonical_key
            .clone();

        let started = std::time::Instant::now();
        let mut cursor = None;
        let mut rows = Vec::new();
        loop {
            let (page, next) = catalog
                .sessions_page(&project_key, cursor.as_ref(), 250)
                .expect("page");
            rows.extend(
                page.into_iter()
                    .map(|session| (session.last_activity_at, session.stable_key)),
            );
            let Some(next) = next else {
                break;
            };
            cursor = Some(next);
        }
        let elapsed = started.elapsed();

        assert_eq!(rows.len(), 10_000);
        let unique = rows
            .iter()
            .map(|(_, key)| key)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 10_000, "paging must not repeat a key");
        let mut expected = rows.clone();
        expected.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        assert_eq!(rows, expected, "paging must follow the total order");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "10k keyset paging took {elapsed:?}, expected under 1s"
        );
    }

    #[test]
    fn sessions_page_query_uses_the_total_order_index() {
        let catalog = ProjectCatalog::open_in_memory().expect("catalog");
        // Explains the exact statement production paging runs, so the two cannot drift apart.
        let plan = catalog
            .connection
            .prepare(&format!("EXPLAIN QUERY PLAN {SESSIONS_PAGE_SQL}"))
            .expect("prepare plan")
            .query_map(params![1_i64, None::<i64>, None::<String>, 10_i64], |row| {
                row.get::<_, String>(3)
            })
            .expect("query plan")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan rows")
            .join("\n");

        assert!(
            plan.contains("sessions_total_order"),
            "keyset paging must use the total-order index, plan was:\n{plan}"
        );
        assert!(
            !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
            "ordering must come from the index, not a sort, plan was:\n{plan}"
        );
    }

    #[test]
    fn alias_conflict_rolls_back_second_candidate() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let alias = SessionIdentity::id("pi", "official").unwrap();
        let mut first = candidate("pi", "first", 1);
        first.aliases.push(SessionAliasCandidate {
            identity: alias.clone(),
            evidence: "official record".to_string(),
        });
        catalog.upsert_candidate(&first).unwrap();

        let mut second = candidate("pi", "second", 2);
        second.aliases.push(SessionAliasCandidate {
            identity: alias,
            evidence: "conflict".to_string(),
        });
        assert!(matches!(
            catalog.upsert_candidate(&second),
            Err(CatalogError::AliasConflict)
        ));
        let snapshot = catalog.snapshot(50).unwrap();
        assert_eq!(snapshot.projects[0].sessions.len(), 1);
    }

    #[test]
    fn alias_trigger_cross_backend_rollback() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let primary = candidate("pi", "primary", 1);
        catalog.upsert_candidate(&primary).unwrap();
        let result = catalog.connection.execute(
            "INSERT INTO session_aliases(alias_backend, alias_kind, alias_value,
                                         primary_stable_key, evidence)
             VALUES ('codex', 'id', 'wrong', ?1, 'raw')",
            [&primary.identity.stable_key],
        );
        assert!(result.is_err());
        let count: i64 = catalog
            .connection
            .query_row("SELECT COUNT(*) FROM session_aliases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn manual_lock_survives_automatic_reclassification() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let first = candidate("codex", "one", 1);
        let second = candidate("codex", "two", 2);
        catalog.upsert_candidate(&first).unwrap();
        catalog.upsert_candidate(&second).unwrap();
        let projects = catalog.snapshot(50).unwrap().projects;
        let project_key = &projects[0].canonical_key;
        catalog
            .assign_session(&first.identity.stable_key, project_key, true, 3)
            .unwrap();
        let mut rescan = first.clone();
        let alternate = std::env::temp_dir();
        rescan.cwd = Some(CandidateField {
            value: alternate,
            observed_at: 4,
            priority: SourcePriority::PrimaryIndex,
            source_key: "new-cwd".to_string(),
        });
        catalog.upsert_candidate(&rescan).unwrap();
        let assigned_project: String = catalog
            .connection
            .query_row(
                "SELECT p.canonical_key FROM assignments a
                 JOIN projects p ON p.id = a.project_id WHERE a.session_key = ?1",
                [&first.identity.stable_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(&assigned_project, project_key);
    }

    #[test]
    fn failed_scans_do_not_advance_missing_streak() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let item = candidate("codex", "missing", 1);
        catalog.upsert_candidate(&item).unwrap();
        let empty = HashSet::new();
        for outcome in [
            ScanCompletion::RootUnavailable,
            ScanCompletion::Cancelled,
            ScanCompletion::Degraded,
            ScanCompletion::Failed,
        ] {
            catalog
                .complete_root_scan(&item.adapter, &item.root_key, outcome, &empty, 2)
                .unwrap();
        }
        assert_source_state(&catalog, &item, "present", 0);

        catalog
            .complete_root_scan(
                &item.adapter,
                &item.root_key,
                ScanCompletion::Complete,
                &empty,
                3,
            )
            .unwrap();
        assert_source_state(&catalog, &item, "suspected_missing", 1);
        catalog
            .complete_root_scan(
                &item.adapter,
                &item.root_key,
                ScanCompletion::Failed,
                &empty,
                4,
            )
            .unwrap();
        assert_source_state(&catalog, &item, "suspected_missing", 1);
        catalog
            .complete_root_scan(
                &item.adapter,
                &item.root_key,
                ScanCompletion::Complete,
                &empty,
                5,
            )
            .unwrap();
        assert_source_state(&catalog, &item, "missing", 2);

        let mut seen = HashSet::new();
        seen.insert(item.source_key.clone());
        catalog
            .complete_root_scan(
                &item.adapter,
                &item.root_key,
                ScanCompletion::Complete,
                &seen,
                6,
            )
            .unwrap();
        assert_source_state(&catalog, &item, "present", 0);
        assert_eq!(catalog.snapshot(50).unwrap().projects[0].sessions.len(), 1);
    }

    #[test]
    fn scan_status_aggregates_valid_and_invalid_roots_per_adapter() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let empty = HashSet::new();
        catalog
            .complete_root_scan("codex", "default", ScanCompletion::NotInstalled, &empty, 1)
            .expect("default missing");
        catalog
            .complete_root_scan(
                "codex",
                "configured-valid",
                ScanCompletion::Complete,
                &empty,
                2,
            )
            .expect("configured valid");
        let status = catalog.snapshot(50).expect("snapshot").scan_status;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].state, "ready");

        catalog
            .complete_root_scan(
                "codex",
                "configured-invalid",
                ScanCompletion::ConfigurationError,
                &empty,
                3,
            )
            .expect("configured invalid");
        let status = catalog.snapshot(50).expect("snapshot").scan_status;
        assert_eq!(status[0].state, "degraded");
        assert_eq!(
            status[0].diagnostic_category.as_deref(),
            Some("configuration_error")
        );
    }

    fn assert_source_state(
        catalog: &ProjectCatalog,
        candidate: &SessionCandidate,
        expected_state: &str,
        expected_streak: i64,
    ) {
        let actual: (String, i64) = catalog
            .connection
            .query_row(
                "SELECT state, missing_streak FROM session_sources
                 WHERE session_key = ?1 AND adapter = ?2 AND root_key = ?3 AND source_key = ?4",
                params![
                    candidate.identity.stable_key,
                    candidate.adapter,
                    candidate.root_key,
                    candidate.source_key
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(actual, (expected_state.to_string(), expected_streak));
    }

    #[test]
    fn corrupt_catalog_is_reported_without_overwrite() {
        let path = std::env::temp_dir().join(format!(
            "herdr-project-corrupt-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let original = b"not a sqlite database";
        std::fs::write(&path, original).unwrap();
        assert!(ProjectCatalog::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn display_name_collision_uses_canonical_path_disambiguator() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let base = std::env::temp_dir().join(format!("herdr-display-{}", std::process::id()));
        let first_dir = base.join("a/same");
        let second_dir = base.join("b/same");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let mut first = candidate("codex", "a", 1);
        first.cwd = Some(CandidateField {
            value: first_dir,
            observed_at: 1,
            priority: SourcePriority::PrimaryIndex,
            source_key: "a".to_string(),
        });
        let mut second = candidate("codex", "b", 2);
        second.cwd = Some(CandidateField {
            value: second_dir,
            observed_at: 2,
            priority: SourcePriority::PrimaryIndex,
            source_key: "b".to_string(),
        });
        catalog.upsert_candidate(&first).unwrap();
        catalog.upsert_candidate(&second).unwrap();
        let projects = catalog.snapshot(50).unwrap().projects;
        assert_eq!(projects.len(), 2);
        assert!(projects
            .iter()
            .all(|project| project.display_name.starts_with("same — ")));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn stale_runtime_clear_respects_generation() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let mut item = candidate("codex", "runtime", 1);
        item.runtime = Some(RuntimeMapping {
            workspace_id: "w1".to_string(),
            pane_id: "p1".to_string(),
            generation: 2,
        });
        catalog.upsert_candidate(&item).unwrap();
        catalog
            .clear_runtime_mapping(&item.identity.stable_key, 1)
            .unwrap();
        assert!(catalog.snapshot(50).unwrap().projects[0].sessions[0].live);
        catalog
            .clear_runtime_mapping(&item.identity.stable_key, 2)
            .unwrap();
        assert!(!catalog.snapshot(50).unwrap().projects[0].sessions[0].live);
    }

    #[test]
    fn missing_title_and_cwd_keep_valid_session() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let item = candidate("grok", "fallback", 5);
        let key_prefix = item.identity.stable_key[..8].to_string();
        let mut missing = item;
        missing.title = None;
        missing.cwd = None;
        catalog.upsert_candidate(&missing).unwrap();
        let snapshot = catalog.snapshot(50).unwrap();
        assert_eq!(snapshot.projects[0].kind, ProjectKind::Unclassified);
        assert_eq!(
            snapshot.projects[0].sessions[0].title,
            format!("Grok session · {key_prefix}")
        );
    }

    #[test]
    fn project_sort_and_page_are_independent_of_runtime_state() {
        let mut catalog = ProjectCatalog::open_in_memory().expect("catalog");
        let base = std::env::temp_dir();
        let mut old = candidate("codex", "old", 1);
        old.cwd = Some(CandidateField {
            value: base.clone(),
            observed_at: 1,
            priority: SourcePriority::PrimaryIndex,
            source_key: "old".to_string(),
        });
        old.runtime = Some(RuntimeMapping {
            workspace_id: "w".to_string(),
            pane_id: "p".to_string(),
            generation: 1,
        });
        let mut recent = candidate("codex", "recent", 2);
        recent.cwd = Some(CandidateField {
            value: base,
            observed_at: 2,
            priority: SourcePriority::PrimaryIndex,
            source_key: "recent".to_string(),
        });
        catalog.upsert_candidate(&old).unwrap();
        catalog.upsert_candidate(&recent).unwrap();
        let sessions = &catalog.snapshot(50).unwrap().projects[0].sessions;
        assert_eq!(sessions[0].title, "title-recent");
        assert!(!sessions[0].live);
        assert!(sessions[1].live);
    }
}
