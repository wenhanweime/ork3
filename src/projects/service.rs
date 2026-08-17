use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};

use super::catalog::{CatalogError, ScanCompletion};
use super::domain::{PendingSemanticSession, SemanticAssignment, SemanticTopicMerge};
use super::{
    ProjectCatalog, ProjectSessionsPage, ProjectsSnapshot, SessionCandidate, SessionCursor,
};

const PROJECT_PAGE_SIZE: usize = 50;
const SCAN_WRITE_BATCH_SIZE: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectServiceError {
    pub code: &'static str,
    pub message: String,
}

impl ProjectServiceError {
    fn catalog(error: CatalogError) -> Self {
        let code = match error {
            CatalogError::NotFound => "not_found",
            CatalogError::AliasConflict => "alias_conflict",
            CatalogError::CrossBackendAlias => "cross_backend_alias",
            CatalogError::Corrupt => "catalog_corrupt",
            CatalogError::UnsupportedSchema(_) => "unsupported_schema",
            CatalogError::Sqlite(_) | CatalogError::Io(_) => "catalog_error",
        };
        Self {
            code,
            message: error.to_string(),
        }
    }

    fn unavailable() -> Self {
        Self {
            code: "catalog_unavailable",
            message: "Project Catalog is unavailable".to_string(),
        }
    }
}

pub(crate) enum ProjectCommand {
    Upsert {
        candidate: Box<SessionCandidate>,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    UpsertScanBatch {
        candidates: Vec<SessionCandidate>,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    Assign {
        session_key: String,
        project_key: String,
        locked: bool,
        observed_at: i64,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    Unlock {
        session_key: String,
        observed_at: i64,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    SessionsPage {
        project_key: String,
        cursor: Option<SessionCursor>,
        limit: usize,
        reply: mpsc::Sender<Result<ProjectSessionsPage, ProjectServiceError>>,
    },
    ClearRuntime {
        session_key: String,
        generation: u64,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    CompleteScan {
        adapter: String,
        root_key: String,
        completion: ScanCompletion,
        seen_source_keys: HashSet<String>,
        excluded_source_keys: HashSet<String>,
        observed_at: i64,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    /// Sessions whose topic is missing or stale, for the classifier worker to pick up.
    PendingSemantic {
        limit: usize,
        reply: mpsc::Sender<Result<Vec<PendingSemanticSession>, ProjectServiceError>>,
    },
    /// Topic labels already in use, so later batches can reuse them.
    KnownTopics {
        limit: usize,
        reply: mpsc::Sender<Result<Vec<String>, ProjectServiceError>>,
    },
    BeginTopicMerge {
        now: i64,
        reply: mpsc::Sender<Result<Vec<String>, ProjectServiceError>>,
    },
    ApplyTopicMerges {
        merges: Vec<SemanticTopicMerge>,
        observed_at: i64,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    /// One classified batch, applied through the same serialized writer as every other mutation.
    ApplySemantic {
        batch: Vec<SemanticAssignment>,
        observed_at: i64,
        reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    },
    Shutdown,
}

pub(crate) struct ProjectService {
    sender: Option<mpsc::Sender<ProjectCommand>>,
    snapshot: Arc<RwLock<ProjectsSnapshot>>,
    worker: Option<std::thread::JoinHandle<()>>,
    scan_workers: Vec<std::thread::JoinHandle<()>>,
    scanner: Option<Arc<Mutex<super::adapters::AdapterScanner>>>,
    scans_in_progress: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
}

impl ProjectService {
    #[cfg(test)]
    pub(crate) fn open(path: &Path, event_hub: crate::api::EventHub) -> Self {
        Self::open_with_threshold(path, event_hub, 20)
    }

    pub(crate) fn open_with_threshold(
        path: &Path,
        event_hub: crate::api::EventHub,
        automation_title_threshold: usize,
    ) -> Self {
        match ProjectCatalog::open_with_threshold(path, automation_title_threshold) {
            Ok(catalog) => Self::from_catalog(catalog, event_hub),
            Err(error) => {
                tracing::warn!(
                    category = "catalog_open",
                    "Project Catalog unavailable: {error}"
                );
                Self::degraded("catalog_open")
            }
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            sender: None,
            snapshot: Arc::new(RwLock::new(ProjectsSnapshot::empty())),
            worker: None,
            scan_workers: Vec::new(),
            scanner: None,
            scans_in_progress: Arc::new(AtomicUsize::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub(crate) fn in_memory(event_hub: crate::api::EventHub) -> Self {
        match ProjectCatalog::open_in_memory() {
            Ok(catalog) => Self::from_catalog(catalog, event_hub),
            Err(error) => panic!("open in-memory Project Catalog: {error}"),
        }
    }

    fn degraded(category: &str) -> Self {
        Self {
            sender: None,
            snapshot: Arc::new(RwLock::new(ProjectsSnapshot::degraded(category))),
            worker: None,
            scan_workers: Vec::new(),
            scanner: None,
            scans_in_progress: Arc::new(AtomicUsize::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn from_catalog(mut catalog: ProjectCatalog, event_hub: crate::api::EventHub) -> Self {
        if let Err(error) = catalog.clear_all_runtime_mappings() {
            tracing::warn!(
                category = "catalog_runtime_reset",
                "Project Catalog runtime reset failed: {error}"
            );
            return Self::degraded("catalog_runtime_reset");
        }
        let initial = catalog
            .snapshot(PROJECT_PAGE_SIZE)
            .unwrap_or_else(|_| ProjectsSnapshot::degraded("catalog_snapshot"));
        let snapshot = Arc::new(RwLock::new(initial));
        let worker_snapshot = Arc::clone(&snapshot);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("ork3-project-catalog".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    if worker_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if !process_command(&mut catalog, &worker_snapshot, &event_hub, command) {
                        break;
                    }
                }
            })
            .ok();

        if worker.is_none() {
            return Self::degraded("catalog_worker_start");
        }
        Self {
            sender: Some(sender),
            snapshot,
            worker,
            scan_workers: Vec::new(),
            scanner: Some(Arc::new(Mutex::new(
                super::adapters::AdapterScanner::default(),
            ))),
            scans_in_progress: Arc::new(AtomicUsize::new(0)),
            shutdown,
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.sender.is_some()
    }

    pub(crate) fn start_background_scan(&mut self, roots: &[super::adapters::AdapterRoot]) {
        let (Some(sender), Some(scanner)) = (self.sender.clone(), self.scanner.clone()) else {
            return;
        };
        let roots = roots.to_vec();
        let scans_in_progress = Arc::clone(&self.scans_in_progress);
        let shutdown = Arc::clone(&self.shutdown);
        scans_in_progress.fetch_add(1, Ordering::Release);
        match std::thread::Builder::new()
            .name("ork3-project-scan".to_string())
            .spawn(move || {
                run_background_scan(sender, scanner, roots, &shutdown);
                scans_in_progress.fetch_sub(1, Ordering::Release);
            }) {
            Ok(worker) => self.scan_workers.push(worker),
            Err(error) => {
                self.scans_in_progress.fetch_sub(1, Ordering::Release);
                tracing::warn!(
                    category = "adapter_worker_start",
                    "Project adapter worker failed to start: {error}"
                );
            }
        }
    }

    pub(crate) fn snapshot(&self) -> ProjectsSnapshot {
        self.snapshot
            .read()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| ProjectsSnapshot::degraded("catalog_snapshot_lock"))
    }

    pub(crate) fn upsert_candidate(
        &self,
        candidate: SessionCandidate,
    ) -> Result<u64, ProjectServiceError> {
        self.request(|reply| ProjectCommand::Upsert {
            candidate: Box::new(candidate),
            reply,
        })
    }

    pub(crate) fn assign_session(
        &self,
        session_key: String,
        project_key: String,
        locked: bool,
        observed_at: i64,
    ) -> Result<u64, ProjectServiceError> {
        self.request(|reply| ProjectCommand::Assign {
            session_key,
            project_key,
            locked,
            observed_at,
            reply,
        })
    }

    pub(crate) fn unlock_session(
        &self,
        session_key: String,
        observed_at: i64,
    ) -> Result<u64, ProjectServiceError> {
        self.request(|reply| ProjectCommand::Unlock {
            session_key,
            observed_at,
            reply,
        })
    }

    /// Starts the topic classification worker.
    ///
    /// Runs off the input and render path, exactly like the file scan: classification calls out
    /// to another process and can take minutes, so it must never block a keystroke.
    pub(crate) fn start_semantic_classification(
        &mut self,
        config: super::semantic::SemanticConfig,
    ) {
        if !config.enabled {
            return;
        }
        let Some(sender) = self.sender.clone() else {
            return;
        };
        let scans_in_progress = Arc::clone(&self.scans_in_progress);
        let shutdown = Arc::clone(&self.shutdown);
        match std::thread::Builder::new()
            .name("ork3-project-semantic".to_string())
            .spawn(move || {
                while scans_in_progress.load(Ordering::Acquire) > 0
                    && !shutdown.load(Ordering::Acquire)
                {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if !shutdown.load(Ordering::Acquire) {
                    super::semantic::run_classification_worker(&sender, &config, &shutdown);
                }
            }) {
            Ok(worker) => self.scan_workers.push(worker),
            Err(error) => tracing::warn!(
                category = "semantic_worker_start",
                "Project semantic worker failed to start: {error}"
            ),
        }
    }

    pub(crate) fn sessions_page(
        &self,
        project_key: String,
        cursor: Option<SessionCursor>,
        limit: usize,
    ) -> Result<ProjectSessionsPage, ProjectServiceError> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(ProjectServiceError::unavailable)?;
        let (reply_tx, reply_rx) = mpsc::channel();
        sender
            .send(ProjectCommand::SessionsPage {
                project_key,
                cursor,
                limit,
                reply: reply_tx,
            })
            .map_err(|_| ProjectServiceError::unavailable())?;
        reply_rx
            .recv()
            .map_err(|_| ProjectServiceError::unavailable())?
    }

    pub(crate) fn clear_runtime_mapping(
        &self,
        session_key: String,
        generation: u64,
    ) -> Result<u64, ProjectServiceError> {
        self.request(|reply| ProjectCommand::ClearRuntime {
            session_key,
            generation,
            reply,
        })
    }

    fn request(
        &self,
        command: impl FnOnce(mpsc::Sender<Result<u64, ProjectServiceError>>) -> ProjectCommand,
    ) -> Result<u64, ProjectServiceError> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(ProjectServiceError::unavailable)?;
        let (reply_tx, reply_rx) = mpsc::channel();
        sender
            .send(command(reply_tx))
            .map_err(|_| ProjectServiceError::unavailable())?;
        reply_rx
            .recv()
            .map_err(|_| ProjectServiceError::unavailable())?
    }
}

impl Drop for ProjectService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(ProjectCommand::Shutdown);
        }
        // Adapter scans and semantic backends may be blocked in filesystem or
        // subprocess work. They observe `shutdown` between bounded operations;
        // detaching their handles prevents process shutdown from waiting on
        // unrelated background discovery work.
        self.scan_workers.clear();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_background_scan(
    sender: mpsc::Sender<ProjectCommand>,
    scanner: Arc<Mutex<super::adapters::AdapterScanner>>,
    roots: Vec<super::adapters::AdapterRoot>,
    shutdown: &AtomicBool,
) {
    for root in roots {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let scan = match scanner.lock() {
            Ok(mut scanner) => scanner.scan_root(&root, false),
            Err(_) => {
                tracing::warn!(
                    adapter = root.adapter,
                    category = "adapter_cache_lock",
                    "Project adapter cache is unavailable"
                );
                continue;
            }
        };
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let mut completion = scan.completion;
        if !scan.reused_cache {
            let mut candidates = scan.candidates.into_iter();
            loop {
                let batch = candidates
                    .by_ref()
                    .take(SCAN_WRITE_BATCH_SIZE)
                    .collect::<Vec<_>>();
                if batch.is_empty() {
                    break;
                }
                let result = request_on_sender(&sender, |reply| ProjectCommand::UpsertScanBatch {
                    candidates: batch,
                    reply,
                });
                if let Err(error) = result {
                    tracing::warn!(
                        adapter = scan.adapter,
                        category = error.code,
                        "Project adapter candidate was rejected"
                    );
                    if completion == ScanCompletion::Complete {
                        completion = ScanCompletion::Degraded;
                    }
                }
            }
        }
        let observed_at = super::runtime::unix_time_ms();
        if let Err(error) = request_on_sender(&sender, |reply| ProjectCommand::CompleteScan {
            adapter: scan.adapter.to_string(),
            root_key: scan.root_key,
            completion,
            seen_source_keys: scan.seen_source_keys,
            excluded_source_keys: scan.excluded_source_keys,
            observed_at,
            reply,
        }) {
            tracing::warn!(
                adapter = scan.adapter,
                category = error.code,
                "Project adapter completion was not committed"
            );
        }
    }
}

fn request_on_sender(
    sender: &mpsc::Sender<ProjectCommand>,
    command: impl FnOnce(mpsc::Sender<Result<u64, ProjectServiceError>>) -> ProjectCommand,
) -> Result<u64, ProjectServiceError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    sender
        .send(command(reply_tx))
        .map_err(|_| ProjectServiceError::unavailable())?;
    reply_rx
        .recv()
        .map_err(|_| ProjectServiceError::unavailable())?
}

/// Reads sessions awaiting classification from the classifier worker thread.
pub(crate) fn request_pending_semantic(
    sender: &mpsc::Sender<ProjectCommand>,
    limit: usize,
) -> Result<Vec<PendingSemanticSession>, ProjectServiceError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    sender
        .send(ProjectCommand::PendingSemantic {
            limit,
            reply: reply_tx,
        })
        .map_err(|_| ProjectServiceError::unavailable())?;
    reply_rx
        .recv()
        .map_err(|_| ProjectServiceError::unavailable())?
}

/// Reads topics already in use from the classifier worker thread.
pub(crate) fn request_known_topics(
    sender: &mpsc::Sender<ProjectCommand>,
    limit: usize,
) -> Result<Vec<String>, ProjectServiceError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    sender
        .send(ProjectCommand::KnownTopics {
            limit,
            reply: reply_tx,
        })
        .map_err(|_| ProjectServiceError::unavailable())?;
    reply_rx
        .recv()
        .map_err(|_| ProjectServiceError::unavailable())?
}

/// Applies a classified batch from the classifier worker thread.
pub(crate) fn request_apply_semantic(
    sender: &mpsc::Sender<ProjectCommand>,
    batch: Vec<SemanticAssignment>,
    observed_at: i64,
) -> Result<u64, ProjectServiceError> {
    request_on_sender(sender, |reply| ProjectCommand::ApplySemantic {
        batch,
        observed_at,
        reply,
    })
}

pub(crate) fn request_begin_topic_merge(
    sender: &mpsc::Sender<ProjectCommand>,
    now: i64,
) -> Result<Vec<String>, ProjectServiceError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    sender
        .send(ProjectCommand::BeginTopicMerge {
            now,
            reply: reply_tx,
        })
        .map_err(|_| ProjectServiceError::unavailable())?;
    reply_rx
        .recv()
        .map_err(|_| ProjectServiceError::unavailable())?
}

pub(crate) fn request_apply_topic_merges(
    sender: &mpsc::Sender<ProjectCommand>,
    merges: Vec<SemanticTopicMerge>,
    observed_at: i64,
) -> Result<u64, ProjectServiceError> {
    request_on_sender(sender, |reply| ProjectCommand::ApplyTopicMerges {
        merges,
        observed_at,
        reply,
    })
}

fn process_command(
    catalog: &mut ProjectCatalog,
    snapshot: &RwLock<ProjectsSnapshot>,
    event_hub: &crate::api::EventHub,
    command: ProjectCommand,
) -> bool {
    match command {
        ProjectCommand::Upsert { candidate, reply } => {
            finish_mutation(catalog, snapshot, event_hub, reply, |catalog| {
                catalog.upsert_candidate(&candidate)
            });
        }
        ProjectCommand::UpsertScanBatch { candidates, reply } => {
            let result = catalog
                .upsert_scanned_candidates(&candidates)
                .map_err(ProjectServiceError::catalog);
            let _ = reply.send(result);
        }
        ProjectCommand::Assign {
            session_key,
            project_key,
            locked,
            observed_at,
            reply,
        } => finish_mutation(catalog, snapshot, event_hub, reply, |catalog| {
            catalog.assign_session(&session_key, &project_key, locked, observed_at)
        }),
        ProjectCommand::Unlock {
            session_key,
            observed_at,
            reply,
        } => finish_mutation(catalog, snapshot, event_hub, reply, |catalog| {
            catalog.unlock_session(&session_key, observed_at)
        }),
        ProjectCommand::KnownTopics { limit, reply } => {
            let result = catalog
                .known_topics(limit)
                .map_err(ProjectServiceError::catalog);
            let _ = reply.send(result);
        }
        ProjectCommand::BeginTopicMerge { now, reply } => {
            let result = catalog
                .begin_topic_merge(now)
                .map_err(ProjectServiceError::catalog);
            let _ = reply.send(result);
        }
        ProjectCommand::ApplyTopicMerges {
            merges,
            observed_at,
            reply,
        } => finish_mutation(catalog, snapshot, event_hub, reply, |catalog| {
            catalog.apply_topic_merges(&merges, observed_at)
        }),
        ProjectCommand::PendingSemantic { limit, reply } => {
            let result = catalog
                .pending_semantic_sessions(limit)
                .map_err(ProjectServiceError::catalog);
            let _ = reply.send(result);
        }
        ProjectCommand::ApplySemantic {
            batch,
            observed_at,
            reply,
        } => finish_mutation(catalog, snapshot, event_hub, reply, |catalog| {
            catalog.apply_semantic_batch(&batch, observed_at)
        }),
        ProjectCommand::SessionsPage {
            project_key,
            cursor,
            limit,
            reply,
        } => {
            let result = catalog
                .sessions_page(&project_key, cursor.as_ref(), limit)
                .and_then(|(sessions, next_cursor)| {
                    Ok(ProjectSessionsPage {
                        projects_schema_version: crate::projects::domain::PROJECTS_SCHEMA_VERSION,
                        revision: catalog.revision()?,
                        project_key,
                        sessions,
                        next_cursor,
                    })
                })
                .map_err(ProjectServiceError::catalog);
            let _ = reply.send(result);
        }
        ProjectCommand::ClearRuntime {
            session_key,
            generation,
            reply,
        } => finish_mutation(catalog, snapshot, event_hub, reply, |catalog| {
            catalog.clear_runtime_mapping(&session_key, generation)
        }),
        ProjectCommand::CompleteScan {
            adapter,
            root_key,
            completion,
            seen_source_keys,
            excluded_source_keys,
            observed_at,
            reply,
        } => finish_mutation(catalog, snapshot, event_hub, reply, |catalog| {
            catalog.complete_root_scan(
                &adapter,
                &root_key,
                completion,
                &seen_source_keys,
                &excluded_source_keys,
                observed_at,
            )
        }),
        ProjectCommand::Shutdown => return false,
    }
    true
}

fn finish_mutation(
    catalog: &mut ProjectCatalog,
    snapshot: &RwLock<ProjectsSnapshot>,
    event_hub: &crate::api::EventHub,
    reply: mpsc::Sender<Result<u64, ProjectServiceError>>,
    mutation: impl FnOnce(&mut ProjectCatalog) -> Result<u64, CatalogError>,
) {
    let result = mutation(catalog).map_err(ProjectServiceError::catalog);
    let result = match result {
        Ok(revision) => match catalog.snapshot(PROJECT_PAGE_SIZE) {
            Ok(next_snapshot) => {
                if let Ok(mut cached) = snapshot.write() {
                    *cached = next_snapshot.clone();
                }
                event_hub.push(crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::ProjectSnapshotUpdated,
                    data: crate::api::schema::EventData::ProjectSnapshotUpdated {
                        revision,
                        scan_status: next_snapshot.scan_status,
                    },
                });
                Ok(revision)
            }
            Err(error) => Err(ProjectServiceError::catalog(error)),
        },
        Err(error) => Err(error),
    };
    let _ = reply.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::{RuntimeMapping, SessionCandidate, SessionIdentity};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-project-service-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("service fixture root");
        path
    }

    fn candidate(id: &str) -> SessionCandidate {
        SessionCandidate {
            identity: SessionIdentity::id("codex", id).unwrap(),
            title: None,
            cwd: None,
            transcript_ref: None,
            first_activity_at: 1,
            last_activity_at: 2,
            adapter: "codex".to_string(),
            root_key: "root".to_string(),
            source_key: id.to_string(),
            observed_at: 2,
            aliases: Vec::new(),
            runtime: None,
            weight: Default::default(),
            session_class: Some(crate::projects::SessionClass::Interactive),
        }
    }

    #[test]
    fn worker_serializes_mutations_and_publishes_monotonic_revisions() {
        let hub = crate::api::EventHub::default();
        let service = ProjectService::in_memory(hub.clone());
        let first = service.upsert_candidate(candidate("one")).unwrap();
        let second = service.upsert_candidate(candidate("two")).unwrap();
        assert!(second > first);
        assert_eq!(service.snapshot().revision, second);
        let events = hub.events_after(0);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.last().map(|(_, event)| &event.data),
            Some(crate::api::schema::EventData::ProjectSnapshotUpdated { revision, .. })
                if *revision == second
        ));
    }

    #[test]
    fn disabled_service_is_nonfatal_and_reports_unavailable_mutations() {
        let service = ProjectService::disabled();
        assert!(service.snapshot().projects.is_empty());
        let error = service.upsert_candidate(candidate("one")).unwrap_err();
        assert_eq!(error.code, "catalog_unavailable");
    }

    #[test]
    fn background_scan_returns_immediately_and_commits_through_writer_queue() {
        let root_path = temp_dir("background");
        std::fs::write(
            root_path.join("rollout-background.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"background\",\"cwd\":\"/tmp\"}}\n",
        )
        .expect("background fixture");
        let canonical = std::fs::canonicalize(&root_path).expect("canonical fixture root");
        let root = super::super::adapters::AdapterRoot {
            adapter: "codex",
            root_key: canonical.to_string_lossy().into_owned(),
            path: canonical,
            origin: super::super::adapters::AdapterRootOrigin::Explicit,
            preflight: None,
        };
        let hub = crate::api::EventHub::default();
        let mut service = ProjectService::in_memory(hub.clone());
        let started = std::time::Instant::now();
        service.start_background_scan(&[root]);
        assert!(started.elapsed() < std::time::Duration::from_millis(200));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let snapshot = service.snapshot();
            if snapshot
                .scan_status
                .iter()
                .any(|status| status.adapter == "codex" && status.state == "ready")
            {
                assert_eq!(snapshot.projects[0].sessions.len(), 1);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background scan timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(hub.events_after(0).len(), 1);
        let _ = std::fs::remove_dir_all(root_path);
    }

    #[test]
    fn opening_service_clears_runtime_mappings_left_by_previous_process() {
        let root = temp_dir("stale-runtime");
        let path = root.join("catalog.sqlite3");
        let mut catalog = ProjectCatalog::open(&path).expect("seed catalog");
        let mut item = candidate("stale-runtime");
        item.runtime = Some(RuntimeMapping {
            workspace_id: "old-workspace".to_string(),
            pane_id: "old-pane".to_string(),
            generation: 99,
        });
        catalog
            .upsert_candidate(&item)
            .expect("seed runtime mapping");
        assert!(catalog.snapshot(50).expect("seed snapshot").projects[0].sessions[0].live);
        drop(catalog);

        let service = ProjectService::open(&path, crate::api::EventHub::default());
        let snapshot = service.snapshot();
        assert!(!snapshot.projects[0].sessions[0].live);
        let _ = std::fs::remove_dir_all(root);
    }
}
