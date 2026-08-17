use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use rusqlite::OpenFlags;
use serde_json::Value;

use super::catalog::ScanCompletion;
use super::{
    CandidateField, SessionAliasCandidate, SessionCandidate, SessionClass, SessionIdentity,
    SourcePriority,
};

const MAX_HISTORY_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JSON_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_JSONL_SCAN_BYTES: usize = 8 * 1024 * 1024;
const MAX_SCAN_DEPTH: usize = 8;
pub(crate) const ADAPTER_NAMES: [&str; 5] = ["codex", "claude", "pi", "opencode", "grok"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdapterRootOrigin {
    Default,
    Explicit,
}

#[derive(Debug, Clone)]
pub(crate) struct AdapterRoot {
    pub adapter: &'static str,
    pub path: PathBuf,
    pub root_key: String,
    pub origin: AdapterRootOrigin,
    pub(crate) preflight: Option<ScanCompletion>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AdapterRootSet {
    roots: Vec<AdapterRoot>,
    allowed_roots: HashMap<&'static str, Vec<PathBuf>>,
    diagnostics: Vec<String>,
}

impl AdapterRootSet {
    pub(crate) fn roots(&self) -> &[AdapterRoot] {
        &self.roots
    }

    pub(crate) fn allowed_roots(&self, adapter: &str) -> &[PathBuf] {
        self.allowed_roots
            .get(adapter)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AdapterScan {
    pub adapter: &'static str,
    pub root_key: String,
    pub candidates: Vec<SessionCandidate>,
    pub seen_source_keys: HashSet<String>,
    /// Known non-session records that should be removed from the derived Catalog.
    pub excluded_source_keys: HashSet<String>,
    pub completion: ScanCompletion,
    pub reused_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileWatermark {
    source_key: String,
    size: u64,
    modified_ns: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootFingerprint {
    files: Vec<FileWatermark>,
    traversal_errors: usize,
}

#[derive(Debug, Clone)]
struct CachedScan {
    fingerprint: RootFingerprint,
    scan: AdapterScan,
}

#[derive(Debug, Default)]
pub(crate) struct AdapterScanner {
    cache: HashMap<(String, String), CachedScan>,
    parse_attempts: usize,
}

impl AdapterScanner {
    pub(crate) fn scan_root(&mut self, root: &AdapterRoot, force_full: bool) -> AdapterScan {
        if let Some(completion) = root.preflight {
            return empty_scan(root, completion);
        }

        let canonical_root = match canonical_directory(&root.path) {
            Ok(path) => path,
            Err(()) => {
                let completion = match root.origin {
                    AdapterRootOrigin::Default if !root.path.exists() => {
                        ScanCompletion::NotInstalled
                    }
                    AdapterRootOrigin::Default => ScanCompletion::RootUnavailable,
                    AdapterRootOrigin::Explicit => ScanCompletion::ConfigurationError,
                };
                return empty_scan(root, completion);
            }
        };
        let root_key = canonical_root.to_string_lossy().into_owned();
        let fingerprint = match root_fingerprint(&canonical_root) {
            Ok(fingerprint) => fingerprint,
            Err(()) => return empty_scan_with_key(root, root_key, ScanCompletion::Failed),
        };
        let cache_key = (root.adapter.to_string(), root_key.clone());
        if !force_full {
            if let Some(cached) = self.cache.get(&cache_key) {
                if cached.fingerprint == fingerprint {
                    let mut scan = cached.scan.clone();
                    scan.reused_cache = true;
                    return scan;
                }
            }
        }

        let (mut scan, attempts) = perform_scan(root.adapter, &canonical_root, &root_key);
        self.parse_attempts = self.parse_attempts.saturating_add(attempts);
        scan.reused_cache = false;
        if !matches!(
            scan.completion,
            ScanCompletion::Failed
                | ScanCompletion::RootUnavailable
                | ScanCompletion::ConfigurationError
                | ScanCompletion::Cancelled
        ) {
            self.cache.insert(
                cache_key,
                CachedScan {
                    fingerprint,
                    scan: scan.clone(),
                },
            );
        }
        scan
    }

    #[cfg(test)]
    fn parse_attempts(&self) -> usize {
        self.parse_attempts
    }
}

pub(crate) fn default_roots() -> Vec<(&'static str, PathBuf)> {
    let (home, data) = crate::platform::project_history_base_dirs();
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.extend([
            ("codex", home.join(".codex/sessions")),
            ("claude", home.join(".claude/projects")),
            ("pi", home.join(".pi/agent/sessions")),
            ("grok", home.join(".grok/sessions")),
        ]);
    }
    if let Some(data) = data {
        roots.push(("opencode", data.join("opencode")));
    }
    roots
}

pub(crate) fn resolve_roots(config: &crate::config::ProjectsConfig) -> AdapterRootSet {
    resolve_roots_with_defaults(config, default_roots())
}

pub(crate) fn configuration_diagnostics(config: &crate::config::ProjectsConfig) -> Vec<String> {
    resolve_roots(config).diagnostics().to_vec()
}

#[cfg(test)]
pub(crate) fn strict_probe(
    config: &crate::config::ProjectsConfig,
) -> Result<Vec<(String, String)>, Vec<String>> {
    let roots = resolve_roots(config);
    let mut scanner = AdapterScanner::default();
    let mut statuses = Vec::new();
    let mut failures = Vec::new();
    for root in roots.roots() {
        let scan = scanner.scan_root(root, true);
        statuses.push((
            scan.adapter.to_string(),
            scan.completion.state().to_string(),
        ));
        if !matches!(
            scan.completion,
            ScanCompletion::Complete | ScanCompletion::NotInstalled
        ) {
            failures.push(format!(
                "{}: {}",
                scan.adapter,
                scan.completion
                    .diagnostic_category()
                    .unwrap_or_else(|| scan.completion.state())
            ));
        }
    }
    if failures.is_empty() {
        Ok(statuses)
    } else {
        Err(failures)
    }
}

fn resolve_roots_with_defaults(
    config: &crate::config::ProjectsConfig,
    defaults: Vec<(&'static str, PathBuf)>,
) -> AdapterRootSet {
    let mut result = AdapterRootSet::default();
    let mut canonical_seen = HashSet::<(String, PathBuf)>::new();
    let mut defaults_by_adapter = defaults.into_iter().collect::<HashMap<_, _>>();

    for adapter in ADAPTER_NAMES {
        if let Some(path) = defaults_by_adapter.remove(adapter) {
            push_resolved_root(
                &mut result,
                &mut canonical_seen,
                adapter,
                path,
                AdapterRootOrigin::Default,
            );
        } else {
            result.roots.push(AdapterRoot {
                adapter,
                path: PathBuf::new(),
                root_key: format!("default:{adapter}:unavailable"),
                origin: AdapterRootOrigin::Default,
                preflight: Some(ScanCompletion::NotInstalled),
            });
        }

        for configured in configured_roots(config, adapter) {
            let path = expand_configured_root(configured);
            push_resolved_root(
                &mut result,
                &mut canonical_seen,
                adapter,
                path,
                AdapterRootOrigin::Explicit,
            );
        }
    }
    result
}

fn configured_roots<'a>(config: &'a crate::config::ProjectsConfig, adapter: &str) -> &'a [PathBuf] {
    match adapter {
        "codex" => &config.adapters.codex.roots,
        "claude" => &config.adapters.claude.roots,
        "pi" => &config.adapters.pi.roots,
        "opencode" => &config.adapters.opencode.roots,
        "grok" => &config.adapters.grok.roots,
        _ => &[],
    }
}

fn push_resolved_root(
    result: &mut AdapterRootSet,
    canonical_seen: &mut HashSet<(String, PathBuf)>,
    adapter: &'static str,
    path: PathBuf,
    origin: AdapterRootOrigin,
) {
    match canonical_directory(&path) {
        Ok(canonical) => {
            if !canonical_seen.insert((adapter.to_string(), canonical.clone())) {
                return;
            }
            result
                .allowed_roots
                .entry(adapter)
                .or_default()
                .push(canonical.clone());
            result.roots.push(AdapterRoot {
                adapter,
                root_key: canonical.to_string_lossy().into_owned(),
                path: canonical,
                origin,
                preflight: None,
            });
        }
        Err(()) => {
            let completion = match origin {
                AdapterRootOrigin::Default => {
                    if path.exists() {
                        ScanCompletion::RootUnavailable
                    } else {
                        ScanCompletion::NotInstalled
                    }
                }
                AdapterRootOrigin::Explicit => {
                    result.diagnostics.push(format!(
                        "projects.adapters.{adapter}.roots contains a missing, unreadable, or non-directory root; ignoring that root"
                    ));
                    ScanCompletion::ConfigurationError
                }
            };
            result.roots.push(AdapterRoot {
                adapter,
                root_key: path.to_string_lossy().into_owned(),
                path,
                origin,
                preflight: Some(completion),
            });
        }
    }
}

fn expand_configured_root(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    let expanded = if value == "~" || value.starts_with("~/") {
        let (home, _) = crate::platform::project_history_base_dirs();
        home.map(|home| home.join(value.strip_prefix("~/").unwrap_or_default()))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        crate::config::resolve_config_relative_path(path)
    };
    lexical_normalize(&expanded).unwrap_or(expanded)
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized.is_absolute().then_some(normalized)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ()> {
    let canonical = std::fs::canonicalize(path).map_err(|_| ())?;
    std::fs::metadata(&canonical)
        .map_err(|_| ())?
        .is_dir()
        .then_some(canonical)
        .ok_or(())
}

#[cfg(test)]
pub(crate) fn scan_default_root(adapter: &'static str, root: &Path) -> AdapterScan {
    let mut scanner = AdapterScanner::default();
    let resolved = AdapterRoot {
        adapter,
        path: root.to_path_buf(),
        root_key: root.to_string_lossy().into_owned(),
        origin: AdapterRootOrigin::Default,
        preflight: None,
    };
    scanner.scan_root(&resolved, true)
}

fn empty_scan(root: &AdapterRoot, completion: ScanCompletion) -> AdapterScan {
    empty_scan_with_key(root, root.root_key.clone(), completion)
}

fn empty_scan_with_key(
    root: &AdapterRoot,
    root_key: String,
    completion: ScanCompletion,
) -> AdapterScan {
    AdapterScan {
        adapter: root.adapter,
        root_key,
        candidates: Vec::new(),
        seen_source_keys: HashSet::new(),
        excluded_source_keys: HashSet::new(),
        completion,
        reused_cache: false,
    }
}

#[derive(Debug)]
struct ScanPayload {
    candidates: Vec<SessionCandidate>,
    seen_source_keys: HashSet<String>,
    excluded_source_keys: HashSet<String>,
    malformed: usize,
    attempts: usize,
}

#[derive(Debug, Clone, Copy)]
enum ScanFailure {
    UnsupportedFormat,
    Failed,
}

fn perform_scan(adapter: &'static str, root: &Path, root_key: &str) -> (AdapterScan, usize) {
    let result = match adapter {
        "codex" => scan_jsonl_tree(
            root,
            adapter,
            |path| {
                is_jsonl(path)
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("rollout-"))
                    && !is_excluded_history_path(path)
            },
            parse_codex,
        ),
        "claude" => scan_jsonl_tree(
            root,
            adapter,
            |path| is_jsonl(path) && !is_excluded_history_path(path),
            parse_claude,
        ),
        "pi" => scan_jsonl_tree(
            root,
            adapter,
            |path| is_jsonl(path) && !is_excluded_history_path(path),
            parse_pi,
        ),
        "grok" => scan_grok(root),
        "opencode" => scan_opencode(root),
        _ => Err(ScanFailure::UnsupportedFormat),
    };

    match result {
        Ok(payload) => {
            let completion = if payload.malformed == 0 {
                ScanCompletion::Complete
            } else {
                ScanCompletion::Degraded
            };
            (
                AdapterScan {
                    adapter,
                    root_key: root_key.to_string(),
                    candidates: payload.candidates,
                    seen_source_keys: payload.seen_source_keys,
                    excluded_source_keys: payload.excluded_source_keys,
                    completion,
                    reused_cache: false,
                },
                payload.attempts,
            )
        }
        Err(failure) => {
            let completion = match failure {
                ScanFailure::UnsupportedFormat => ScanCompletion::UnsupportedFormat,
                ScanFailure::Failed => ScanCompletion::Failed,
            };
            (
                AdapterScan {
                    adapter,
                    root_key: root_key.to_string(),
                    candidates: Vec::new(),
                    seen_source_keys: HashSet::new(),
                    excluded_source_keys: HashSet::new(),
                    completion,
                    reused_cache: false,
                },
                usize::from(matches!(adapter, "opencode")),
            )
        }
    }
}

fn scan_jsonl_tree(
    root: &Path,
    adapter: &'static str,
    accepts: impl Fn(&Path) -> bool,
    parser: fn(&Path, &Path) -> Result<Option<SessionCandidate>, ()>,
) -> Result<ScanPayload, ScanFailure> {
    let walk = walk_regular_files(root, &accepts).map_err(|_| ScanFailure::Failed)?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut malformed = walk.errors;
    let mut attempts = 0usize;
    for entry in walk.files {
        let source_key = match canonical_source_key(root, &entry.path) {
            Ok(source_key) => source_key,
            Err(()) => {
                malformed = malformed.saturating_add(1);
                continue;
            }
        };
        seen.insert(source_key.clone());
        attempts = attempts.saturating_add(1);
        match parser(&entry.path, root) {
            Ok(Some(mut candidate)) => {
                normalize_candidate_source(&mut candidate, adapter, root, &source_key);
                candidates.push(candidate);
            }
            Ok(None) => {}
            Err(()) => malformed = malformed.saturating_add(1),
        }
    }
    Ok(ScanPayload {
        candidates,
        seen_source_keys: seen,
        excluded_source_keys: HashSet::new(),
        malformed,
        attempts,
    })
}

#[derive(Debug)]
struct FileEntry {
    path: PathBuf,
    metadata: Metadata,
}

#[derive(Debug)]
struct WalkResult {
    files: Vec<FileEntry>,
    errors: usize,
}

fn walk_regular_files(root: &Path, accepts: &impl Fn(&Path) -> bool) -> Result<WalkResult, ()> {
    let mut files = Vec::new();
    let mut errors = 0usize;
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut visited = HashSet::new();
    while let Some((directory, depth)) = pending.pop() {
        let canonical_directory = match std::fs::canonicalize(&directory) {
            Ok(path) => path,
            Err(_) if depth == 0 => return Err(()),
            Err(_) => {
                errors = errors.saturating_add(1);
                continue;
            }
        };
        if !canonical_directory.starts_with(root) || !visited.insert(canonical_directory.clone()) {
            continue;
        }
        let entries = match std::fs::read_dir(&canonical_directory) {
            Ok(entries) => entries,
            Err(_) if depth == 0 => return Err(()),
            Err(_) => {
                errors = errors.saturating_add(1);
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    errors = errors.saturating_add(1);
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    errors = errors.saturating_add(1);
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if depth < MAX_SCAN_DEPTH {
                    pending.push((path, depth.saturating_add(1)));
                }
            } else if metadata.is_file() && accepts(&path) {
                files.push(FileEntry { path, metadata });
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(WalkResult { files, errors })
}

fn root_fingerprint(root: &Path) -> Result<RootFingerprint, ()> {
    let walk = walk_regular_files(root, &|_| true)?;
    let mut files = Vec::with_capacity(walk.files.len());
    let mut errors = walk.errors;
    for entry in walk.files {
        match canonical_source_key(root, &entry.path) {
            Ok(source_key) => files.push(FileWatermark {
                source_key,
                size: entry.metadata.len(),
                modified_ns: modified_ns(&entry.metadata),
            }),
            Err(()) => errors = errors.saturating_add(1),
        }
    }
    files.sort_by(|left, right| left.source_key.cmp(&right.source_key));
    Ok(RootFingerprint {
        files,
        traversal_errors: errors,
    })
}

fn modified_ns(metadata: &Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn canonical_source_key(root: &Path, path: &Path) -> Result<String, ()> {
    let canonical = std::fs::canonicalize(path).map_err(|_| ())?;
    if !canonical.starts_with(root) {
        return Err(());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
}

fn is_default_opencode_title(title: &str) -> bool {
    title.trim_start().starts_with("New session - ")
}

fn is_excluded_history_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value.eq_ignore_ascii_case("subagents"))
    }) || path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with('.')
                || name.ends_with(".tmp")
                || name.ends_with(".partial")
                || name.ends_with(".lock")
                || name.contains(".tmp.")
        })
}

fn parse_codex(path: &Path, _root: &Path) -> Result<Option<SessionCandidate>, ()> {
    let mut identity = None;
    let mut cwd = None;
    let mut session_class = SessionClass::Interactive;
    let mut picker = TitlePicker::default();
    let mut weight = SessionWeight::known();
    visit_json_lines(path, |value| {
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = value.get("payload")?;
                identity = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                let originator = payload.get("originator").and_then(Value::as_str);
                let source = payload.get("source").and_then(Value::as_str);
                if originator == Some("codex_exec") || source == Some("exec") {
                    session_class = SessionClass::Automation;
                }
            }
            Some("event_msg")
                if value.pointer("/payload/type").and_then(Value::as_str)
                    == Some("user_message") =>
            {
                if let Some(message) = value.pointer("/payload/message").and_then(Value::as_str) {
                    weight.record(message);
                    picker.offer(message);
                }
            }
            Some("response_item")
                if value.pointer("/payload/role").and_then(Value::as_str) == Some("user") =>
            {
                if let Some(message) = value.pointer("/payload/content").and_then(first_text) {
                    weight.record(message);
                    picker.offer(message);
                }
            }
            _ => {}
        }
        // Keep reading past the first message so a short opener does not become the title and
        // so the turn count reflects the real session.
        (identity.is_some() && picker.is_settled()).then_some(())
    })?;
    let identity = SessionIdentity::id("codex", &identity.ok_or(())?).map_err(|_| ())?;
    let title = picker.take();
    let mut candidate = candidate_from_identity(identity, cwd, title, path)?;
    candidate.weight = weight;
    candidate.session_class = Some(session_class);
    Ok(Some(candidate))
}

fn parse_claude(path: &Path, _root: &Path) -> Result<Option<SessionCandidate>, ()> {
    let mut identity = None;
    let mut cwd = None;
    let mut sidechain = false;
    let mut picker = TitlePicker::default();
    let mut weight = SessionWeight::known();
    visit_json_lines(path, |value| {
        if value.get("type").and_then(Value::as_str) == Some("user") {
            sidechain |= value
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if identity.is_none() {
                identity = value
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if cwd.is_none() {
                cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            }
            if let Some(message) = value.pointer("/message/content").and_then(first_text) {
                weight.record(message);
                picker.offer(message);
            }
        }
        None
    })?;
    if sidechain {
        return Ok(None);
    }
    let identity = SessionIdentity::id("claude", &identity.ok_or(())?).map_err(|_| ())?;
    let mut candidate = candidate_from_identity(identity, cwd, picker.take(), path)?;
    candidate.weight = weight;
    Ok(Some(candidate))
}

fn parse_pi(path: &Path, root: &Path) -> Result<Option<SessionCandidate>, ()> {
    let mut official_id = None;
    let mut cwd = None;
    let mut picker = TitlePicker::default();
    let mut weight = SessionWeight::known();
    let mut saw_session_record = false;
    visit_json_lines(path, |value| {
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                saw_session_record = true;
                official_id = value.get("id").and_then(Value::as_str).map(str::to_string);
                cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            }
            Some("message")
                if value.pointer("/message/role").and_then(Value::as_str) == Some("user") =>
            {
                if let Some(message) = value.pointer("/message/content").and_then(first_text) {
                    weight.record(message);
                    picker.offer(message);
                }
            }
            _ => {}
        }
        None
    })?;
    if !saw_session_record {
        return Err(());
    }
    let identity =
        SessionIdentity::path("pi", path, root, &[root.to_path_buf()], false).map_err(|_| ())?;
    let mut candidate = candidate_from_identity(identity, cwd, picker.take(), path)?;
    candidate.weight = weight;
    if let Some(official_id) = official_id {
        candidate.aliases.push(SessionAliasCandidate {
            identity: SessionIdentity::id("pi", &official_id).map_err(|_| ())?,
            evidence: "pi session header id".to_string(),
        });
    }
    Ok(Some(candidate))
}

fn scan_grok(root: &Path) -> Result<ScanPayload, ScanFailure> {
    let walk = walk_regular_files(root, &|path| {
        path.file_name().and_then(|name| name.to_str()) == Some("summary.json")
            && !is_excluded_history_path(path)
    })
    .map_err(|_| ScanFailure::Failed)?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut malformed = walk.errors;
    let mut attempts = 0usize;
    for entry in walk.files {
        let source_key = match canonical_source_key(root, &entry.path) {
            Ok(source_key) => source_key,
            Err(_) => {
                malformed = malformed.saturating_add(1);
                continue;
            }
        };
        seen.insert(source_key.clone());
        if entry.metadata.len() > MAX_HISTORY_FILE_BYTES {
            malformed = malformed.saturating_add(1);
            continue;
        }
        attempts = attempts.saturating_add(1);
        let result = (|| {
            let summary = read_json(&entry.path)?;
            let session_dir = entry.path.parent().ok_or(())?;
            let path_identity =
                SessionIdentity::path("grok", session_dir, root, &[root.to_path_buf()], false)
                    .map_err(|_| ())?;
            let explicit_id = summary
                .get("id")
                .or_else(|| summary.get("session_id"))
                .or_else(|| summary.get("sessionId"))
                .and_then(Value::as_str);
            let (identity, aliases) = if let Some(explicit_id) = explicit_id {
                (
                    SessionIdentity::id("grok", explicit_id).map_err(|_| ())?,
                    vec![SessionAliasCandidate {
                        identity: path_identity,
                        evidence: "grok summary id and directory".to_string(),
                    }],
                )
            } else {
                (path_identity, Vec::new())
            };

            let context_path = session_dir.join("prompt_context.json");
            let context =
                if context_path.exists() || std::fs::symlink_metadata(&context_path).is_ok() {
                    match read_json(&context_path) {
                        Ok(value) => Some(value),
                        Err(()) => {
                            malformed = malformed.saturating_add(1);
                            None
                        }
                    }
                } else {
                    None
                };
            let cwd = context
                .as_ref()
                .and_then(|context| {
                    context
                        .get("working_directory")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                })
                .or_else(|| grok_cwd_from_path(root, session_dir));
            let title = summary
                .get("generated_title")
                .and_then(Value::as_str)
                .and_then(safe_title);
            let mut candidate = candidate_from_identity(identity, cwd, title, &entry.path)?;
            let history_path = session_dir.join("chat_history.jsonl");
            if history_path.exists() || std::fs::symlink_metadata(&history_path).is_ok() {
                let mut weight = SessionWeight::known();
                visit_json_lines(&history_path, |value| {
                    if value.get("type").and_then(Value::as_str) == Some("user") {
                        if let Some(message) = value.get("content").and_then(first_text) {
                            weight.record(message);
                        }
                    }
                    None
                })?;
                candidate.weight = weight;
            }
            candidate.aliases = aliases;
            normalize_candidate_source(&mut candidate, "grok", root, &source_key);
            Ok::<_, ()>(candidate)
        })();
        match result {
            Ok(candidate) => candidates.push(candidate),
            Err(()) => malformed = malformed.saturating_add(1),
        }
    }
    Ok(ScanPayload {
        candidates,
        seen_source_keys: seen,
        excluded_source_keys: HashSet::new(),
        malformed,
        attempts,
    })
}

fn scan_opencode(root: &Path) -> Result<ScanPayload, ScanFailure> {
    let database = root.join("opencode.db");
    let metadata =
        std::fs::symlink_metadata(&database).map_err(|_| ScanFailure::UnsupportedFormat)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ScanFailure::UnsupportedFormat);
    }
    let database_source_key =
        canonical_source_key(root, &database).map_err(|_| ScanFailure::Failed)?;
    let (file_first, file_last) = file_times_ms(&database).map_err(|_| ScanFailure::Failed)?;
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ScanFailure::Failed)?;
    let mut statement = connection
        .prepare(
            "WITH user_content AS (
                 SELECT m.session_id,
                        COUNT(DISTINCT m.id) AS user_turns,
                        COALESCE(SUM(length(COALESCE(json_extract(p.data, '$.text'), ''))), 0)
                            AS user_chars,
                        MAX(CASE WHEN
                              instr(json_extract(p.data, '$.text'),
                                    '把下面的编码会话按主题聚类') > 0
                              OR instr(json_extract(p.data, '$.text'),
                                       '你是个人工作地图整理员') > 0
                              OR instr(json_extract(p.data, '$.text'),
                                       '你是本机 Agent 会话项目整理员') > 0
                              OR instr(json_extract(p.data, '$.text'),
                                       '你是本机会话档案编辑') > 0
                              OR instr(json_extract(p.data, '$.text'),
                                       '你是工程 Project 命名编辑') > 0
                              OR instr(json_extract(p.data, '$.text'),
                                       '你是工程任务命名编辑') > 0
                              OR instr(json_extract(p.data, '$.text'),
                                       '4-12个字的清晰任务主题名') > 0
                            THEN 1 ELSE 0 END) AS classifier_artifact
                   FROM message m
                   JOIN part p ON p.message_id = m.id
                  WHERE json_extract(m.data, '$.role') = 'user'
                    AND json_extract(p.data, '$.type') = 'text'
                  GROUP BY m.session_id
             )
             SELECT s.id, s.directory, s.title, s.time_created, s.time_updated,
                    COALESCE(u.classifier_artifact, 0),
                    COALESCE(u.user_turns, 0), COALESCE(u.user_chars, 0)
             FROM session s
             LEFT JOIN user_content u ON u.session_id = s.id
             WHERE s.parent_id IS NULL AND s.time_archived IS NULL
             ORDER BY s.time_updated DESC, s.id ASC",
        )
        .map_err(|_| ScanFailure::UnsupportedFormat)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .map_err(|_| ScanFailure::Failed)?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut excluded = HashSet::new();
    let mut malformed = 0usize;
    for row in rows {
        let (
            id,
            cwd,
            title,
            first_activity_at,
            last_activity_at,
            classifier_artifact,
            user_turns,
            user_chars,
        ) = match row {
            Ok(row) => row,
            Err(_) => {
                malformed = malformed.saturating_add(1);
                continue;
            }
        };
        let source_key = format!("{database_source_key}#{id}");
        if classifier_artifact {
            // These are local classifier invocations, not user sessions. Report the source as an
            // explicit exclusion so the derived Catalog removes any copy imported by older builds.
            excluded.insert(source_key);
            continue;
        }
        seen.insert(source_key.clone());
        let identity = match SessionIdentity::id("opencode", &id) {
            Ok(identity) => identity,
            Err(_) => {
                malformed = malformed.saturating_add(1);
                continue;
            }
        };
        let first_activity_at = first_activity_at.unwrap_or(file_first);
        let last_activity_at = last_activity_at.unwrap_or(file_last);
        let observed_at = last_activity_at.max(first_activity_at);
        candidates.push(SessionCandidate {
            identity,
            title: title
                .as_deref()
                .filter(|value| !is_default_opencode_title(value))
                .and_then(safe_title)
                .map(|value| primary_field(value, observed_at, &source_key)),
            cwd: cwd
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|value| primary_field(value, observed_at, &source_key)),
            transcript_ref: None,
            first_activity_at: first_activity_at.min(last_activity_at),
            last_activity_at: last_activity_at.max(first_activity_at),
            adapter: "opencode".to_string(),
            root_key: root.to_string_lossy().into_owned(),
            source_key,
            observed_at,
            aliases: Vec::new(),
            runtime: None,
            weight: SessionWeight {
                turns: usize::try_from(user_turns).unwrap_or(0),
                chars: usize::try_from(user_chars).unwrap_or(0),
                known: true,
            },
            session_class: Some(SessionClass::Interactive),
        });
    }
    Ok(ScanPayload {
        candidates,
        seen_source_keys: seen,
        excluded_source_keys: excluded,
        malformed,
        attempts: 1,
    })
}

fn candidate_from_identity(
    identity: SessionIdentity,
    cwd: Option<PathBuf>,
    title: Option<String>,
    path: &Path,
) -> Result<SessionCandidate, ()> {
    let (first_activity_at, last_activity_at) = file_times_ms(path)?;
    let source_key = path.to_string_lossy().into_owned();
    Ok(SessionCandidate {
        identity,
        title: title.map(|value| transcript_field(value, last_activity_at, &source_key)),
        cwd: cwd.map(|value| transcript_field(value, last_activity_at, &source_key)),
        transcript_ref: Some(transcript_field(
            source_key.clone(),
            last_activity_at,
            &source_key,
        )),
        first_activity_at,
        last_activity_at,
        adapter: String::new(),
        root_key: String::new(),
        source_key,
        observed_at: last_activity_at,
        aliases: Vec::new(),
        runtime: None,
        weight: SessionWeight::default(),
        session_class: Some(SessionClass::Interactive),
    })
}

fn normalize_candidate_source(
    candidate: &mut SessionCandidate,
    adapter: &str,
    root: &Path,
    source_key: &str,
) {
    candidate.adapter = adapter.to_string();
    candidate.root_key = root.to_string_lossy().into_owned();
    candidate.source_key = source_key.to_string();
    if let Some(field) = candidate.title.as_mut() {
        field.source_key = source_key.to_string();
    }
    if let Some(field) = candidate.cwd.as_mut() {
        field.source_key = source_key.to_string();
    }
    if let Some(field) = candidate.transcript_ref.as_mut() {
        field.value = source_key.to_string();
        field.source_key = source_key.to_string();
    }
}

fn transcript_field<T>(value: T, observed_at: i64, source_key: &str) -> CandidateField<T> {
    CandidateField {
        value,
        observed_at,
        priority: SourcePriority::TranscriptFile,
        source_key: source_key.to_string(),
    }
}

fn primary_field<T>(value: T, observed_at: i64, source_key: &str) -> CandidateField<T> {
    CandidateField {
        value,
        observed_at,
        priority: SourcePriority::PrimaryIndex,
        source_key: source_key.to_string(),
    }
}

fn file_times_ms(path: &Path) -> Result<(i64, i64), ()> {
    let metadata = std::fs::metadata(path).map_err(|_| ())?;
    let modified = metadata.modified().map_err(|_| ())?;
    let modified = system_time_ms(modified);
    let created = metadata
        .created()
        .ok()
        .map(system_time_ms)
        .unwrap_or(modified);
    Ok((created.min(modified), modified.max(created)))
}

fn system_time_ms(value: std::time::SystemTime) -> i64 {
    value
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn visit_json_lines(path: &Path, mut visitor: impl FnMut(&Value) -> Option<()>) -> Result<(), ()> {
    let file = File::open(path).map_err(|_| ())?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut scanned_bytes = 0usize;
    let mut saw_valid_record = false;
    loop {
        let bytes = match read_limited_json_line(&mut reader, &mut line) {
            Ok(bytes) => bytes,
            // An over-long line is padding or truncation. Reading several turns for a title
            // reaches this where a first-message-only scan never did, so keep a valid prefix
            // rather than failing the whole file.
            Err(()) if saw_valid_record => return Ok(()),
            Err(()) => return Err(()),
        };
        if bytes == 0 {
            return Ok(());
        }
        scanned_bytes = scanned_bytes.saturating_add(bytes);
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            if saw_valid_record {
                return Ok(());
            }
            return Err(());
        };
        saw_valid_record = true;
        if visitor(&value).is_some() {
            return Ok(());
        }
        if scanned_bytes >= MAX_JSONL_SCAN_BYTES {
            return Ok(());
        }
    }
}

fn read_limited_json_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> Result<usize, ()> {
    line.clear();
    loop {
        let (bytes_to_consume, found_newline) = {
            let available = reader.fill_buf().map_err(|_| ())?;
            if available.is_empty() {
                return Ok(line.len());
            }
            let bytes_to_consume = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index.saturating_add(1));
            if line.len().saturating_add(bytes_to_consume) > MAX_JSON_LINE_BYTES {
                return Err(());
            }
            line.extend_from_slice(&available[..bytes_to_consume]);
            (
                bytes_to_consume,
                available.get(bytes_to_consume.saturating_sub(1)) == Some(&b'\n'),
            )
        };
        reader.consume(bytes_to_consume);
        if found_newline {
            return Ok(line.len());
        }
    }
}

fn read_json(path: &Path) -> Result<Value, ()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_HISTORY_FILE_BYTES
    {
        return Err(());
    }
    serde_json::from_reader(BufReader::new(File::open(path).map_err(|_| ())?)).map_err(|_| ())
}

fn first_text(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        Value::Array(items) => items.iter().find_map(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .or_else(|| item.as_str())
        }),
        _ => None,
    }
}

/// Markers of harness-injected preamble rather than anything the user typed.
///
/// Codex and Claude prepend AGENTS.md, environment and instruction blocks to the first user
/// message. Treating that as a title produced 9,714 of 10,291 sessions titled with instruction
/// text on this machine, which is both useless in the tree and a body-content leak into the
/// classifier prompt (SPEC §3.2).
const INJECTED_PREAMBLE_MARKERS: [&str; 6] = [
    "<user_instructions>",
    "<environment_context>",
    "AGENTS.md instructions for",
    "# Repository Guidelines",
    "<INSTRUCTIONS>",
    "Codebase and user instructions are shown below",
];

/// Minimum user turns for a session to be worth classifying.
///
/// Owner's rule: three turns or fewer is a false start ("hi", a cancelled command), so a real
/// exchange needs more than that. Below this the topic is guesswork, and the session keeps its
/// path-based Project rather than getting an invented topic.
pub(crate) const MIN_SUBSTANTIVE_TURNS: usize = 4;
/// Minimum total user characters for the same decision.
pub(crate) const MIN_SUBSTANTIVE_CHARS: usize = 80;
/// Floor of real user text regardless of turn count, so a session of only "hi" never qualifies.
pub(crate) const MIN_ANY_CHARS: usize = 24;
/// A short opener is not evidence of a short session, so keep collecting until this many turns
/// have been seen; "继续" can open a 13,000-character session.
const TITLE_SCAN_TURNS: usize = 4;
/// Hard cap on turns inspected, so a session of only one-word turns still terminates.
const MAX_TITLE_SCAN_TURNS: usize = 40;
/// A title shorter than this ("hi", "继续") does not describe the session, so keep looking.
const MIN_TITLE_CHARS: usize = 12;
/// Enough opener text to describe the work without pulling in transcript bulk.
const TITLE_MAX_CHARS: usize = 96;

/// How substantive a session's user side is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SessionWeight {
    pub turns: usize,
    pub chars: usize,
    pub known: bool,
}

impl SessionWeight {
    fn known() -> Self {
        Self {
            turns: 0,
            chars: 0,
            known: true,
        }
    }

    fn record(&mut self, message: &str) {
        // Measure only what the user actually wrote. Counting harness preamble made a session of
        // four "hi" turns look like 264 characters of substance and slip past the filter.
        let own_words = strip_injected_preamble(message).trim();
        if own_words.is_empty() {
            return;
        }
        self.turns = self.turns.saturating_add(1);
        self.chars = self.chars.saturating_add(own_words.chars().count());
    }

    /// True when there is enough material for a topic to mean anything.
    ///
    /// Either signal can carry a session — a long back-and-forth of short turns, or one detailed
    /// request — but a floor of real text is always required, otherwise a session of nothing but
    /// "hi" passes on turn count alone. Requiring both signals instead discarded a real
    /// 109k-character bug report because it took only two turns.
    ///
    /// The classifier query in `catalog.rs` applies this same rule in SQL so it can filter
    /// without loading every session; this is the readable definition the tests pin.
    #[cfg(test)]
    pub(crate) fn is_substantive(self) -> bool {
        if self.chars < MIN_ANY_CHARS {
            return false;
        }
        self.turns >= MIN_SUBSTANTIVE_TURNS || self.chars >= MIN_SUBSTANTIVE_CHARS
    }
}

/// Picks the most descriptive of the opening user messages.
///
/// Sessions often open with "在吗" or "继续" and only then say what they are about, so the best
/// title is the longest of the first few turns rather than simply the first.
#[derive(Debug, Default)]
pub(crate) struct TitlePicker {
    best: Option<String>,
    /// Turns that carried real user text.
    seen: usize,
    /// Every turn inspected, including preamble, for the hard cap.
    offered: usize,
}

impl TitlePicker {
    fn offer(&mut self, message: &str) {
        if self.is_settled() {
            return;
        }
        self.offered = self.offered.saturating_add(1);
        let Some(candidate) = safe_title(message) else {
            // Preamble and empty turns must not consume the scan budget, or a session that opens
            // with AGENTS.md plus "hi" is settled before the user says what it is about.
            return;
        };
        self.seen += 1;
        let better = match self.best.as_ref() {
            None => true,
            // Prefer a clearly more descriptive turn. Requiring a real gain stops a marginally
            // longer restatement of the same request from churning the title.
            Some(current) => candidate.chars().count() > current.chars().count() + 8,
        };
        if better {
            self.best = Some(candidate);
        }
    }

    fn is_settled(&self) -> bool {
        // Stop once enough real turns have been seen and one of them is descriptive enough to
        // stand as a title. A run of one-word turns keeps the scan open, but only up to a hard
        // cap so a long session of "hi" cannot make this read the whole transcript.
        if self.offered >= MAX_TITLE_SCAN_TURNS {
            return true;
        }
        self.seen >= TITLE_SCAN_TURNS
            && self
                .best
                .as_ref()
                .is_some_and(|title| title.chars().count() >= MIN_TITLE_CHARS)
    }

    fn take(self) -> Option<String> {
        self.best
    }
}

fn safe_title(value: &str) -> Option<String> {
    let cleaned = strip_injected_preamble(value);
    let normalized = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TITLE_MAX_CHARS)
        .collect::<String>();
    (!normalized.is_empty()).then_some(normalized)
}

/// Removes harness preamble, returning the user's own words when any remain.
///
/// Returns an empty string when the message is nothing but preamble, so the caller falls through
/// to the next title candidate instead of storing boilerplate.
fn strip_injected_preamble(value: &str) -> &str {
    let mut rest = value.trim();

    // Drop leading XML-ish instruction blocks, including unclosed ones.
    loop {
        let trimmed = rest.trim_start();
        let Some(open) = trimmed.strip_prefix('<') else {
            break;
        };
        let Some(name_end) = open.find('>') else {
            break;
        };
        let tag = &open[..name_end];
        if !tag
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            break;
        }
        let close = format!("</{tag}>");
        match trimmed.find(&close) {
            Some(index) => rest = &trimmed[index + close.len()..],
            // Unclosed block: everything after it is preamble too.
            None => return "",
        }
    }

    let rest = rest.trim();
    // Markers may sit behind a markdown heading prefix, e.g. "# AGENTS.md instructions for ...".
    let unprefixed = rest.trim_start_matches(['#', ' ']);
    if INJECTED_PREAMBLE_MARKERS
        .iter()
        .any(|marker| unprefixed.starts_with(marker))
    {
        return "";
    }
    rest
}

fn grok_cwd_from_path(root: &Path, session_dir: &Path) -> Option<PathBuf> {
    let relative = session_dir.strip_prefix(root).ok()?;
    let encoded = relative.components().next()?.as_os_str().to_str()?;
    if !encoded.contains('%') {
        return None;
    }
    let decoded = PathBuf::from(percent_decode(encoded)?);
    decoded.is_absolute().then_some(decoded)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(bytes.get(index + 1).copied()?)?;
            let low = hex(bytes.get(index + 2).copied()?)?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-project-adapter-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create fixture root");
        path
    }

    fn root(adapter: &'static str, path: &Path) -> AdapterRoot {
        AdapterRoot {
            adapter,
            path: std::fs::canonicalize(path).expect("canonical fixture root"),
            root_key: path.to_string_lossy().into_owned(),
            origin: AdapterRootOrigin::Explicit,
            preflight: None,
        }
    }

    fn write_fixture(adapter: &str, root: &Path, id: &str, title: Option<&str>, cwd: Option<&str>) {
        match adapter {
            "codex" => {
                let mut lines = vec![serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": id, "cwd": cwd, "unknown": {"future": true}}
                })];
                if let Some(title) = title {
                    lines.push(serde_json::json!({
                        "type": "response_item",
                        "payload": {"role": "user", "content": [{"text": title}], "future": 1}
                    }));
                }
                write_json_lines(&root.join(format!("rollout-{id}.jsonl")), &lines);
            }
            "claude" => {
                let content = title
                    .map(|title| serde_json::json!([{"type": "text", "text": title}]))
                    .unwrap_or_else(|| serde_json::json!([]));
                write_json_lines(
                    &root.join(format!("{id}.jsonl")),
                    &[serde_json::json!({
                        "type": "user",
                        "sessionId": id,
                        "cwd": cwd,
                        "isSidechain": false,
                        "message": {"content": content},
                        "unknown": true
                    })],
                );
            }
            "pi" => {
                let mut lines = vec![serde_json::json!({
                    "type": "session",
                    "id": id,
                    "cwd": cwd,
                    "unknown": [1, 2, 3]
                })];
                if let Some(title) = title {
                    lines.push(serde_json::json!({
                        "type": "message",
                        "message": {"role": "user", "content": title},
                        "future": "ignored"
                    }));
                }
                write_json_lines(&root.join(format!("{id}.jsonl")), &lines);
            }
            "grok" => {
                let session = root.join(id);
                std::fs::create_dir_all(&session).expect("grok fixture dir");
                std::fs::write(
                    session.join("summary.json"),
                    serde_json::to_vec(&serde_json::json!({
                        "id": id,
                        "generated_title": title,
                        "created_at": 1,
                        "last_active_at": 2,
                        "unknown": {"future": true}
                    }))
                    .expect("grok summary json"),
                )
                .expect("grok summary");
                if let Some(cwd) = cwd {
                    std::fs::write(
                        session.join("prompt_context.json"),
                        serde_json::to_vec(&serde_json::json!({
                            "working_directory": cwd,
                            "unknown": true
                        }))
                        .expect("grok context json"),
                    )
                    .expect("grok context");
                }
            }
            "opencode" => {
                let connection = opencode_connection(root);
                connection
                    .execute(
                        "INSERT OR REPLACE INTO session(
                            id, directory, title, time_created, time_updated,
                            parent_id, time_archived, future_field
                         ) VALUES (?1, ?2, ?3, 1, 2, NULL, NULL, 'ignored')",
                        params![id, cwd, title],
                    )
                    .expect("opencode fixture row");
            }
            _ => panic!("unknown adapter fixture: {adapter}"),
        }
    }

    fn write_json_lines(path: &Path, values: &[Value]) {
        let content = values
            .iter()
            .map(|value| serde_json::to_string(value).expect("fixture json"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(path, content).expect("jsonl fixture");
    }

    fn opencode_connection(root: &Path) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open(root.join("opencode.db"))
            .expect("opencode fixture database");
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS session (
                    id TEXT PRIMARY KEY,
                    directory TEXT,
                    title TEXT,
                    time_created INTEGER,
                    time_updated INTEGER,
                    parent_id TEXT,
                    time_archived INTEGER,
                    future_field TEXT
                );
                CREATE TABLE IF NOT EXISTS message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    data TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    data TEXT NOT NULL
                );",
            )
            .expect("opencode fixture schema");
        connection
    }

    fn write_malformed_sibling(adapter: &str, root: &Path) {
        match adapter {
            "codex" => std::fs::write(root.join("rollout-malformed.jsonl"), "not json\n")
                .expect("codex malformed"),
            "claude" | "pi" => {
                std::fs::write(root.join("malformed.jsonl"), "not json\n").expect("jsonl malformed")
            }
            "grok" => {
                let path = root.join("malformed");
                std::fs::create_dir_all(&path).expect("grok malformed dir");
                std::fs::write(path.join("summary.json"), "not json").expect("grok malformed")
            }
            "opencode" => {
                opencode_connection(root)
                    .execute(
                        "INSERT INTO session(
                            id, directory, title, time_created, time_updated,
                            parent_id, time_archived
                         ) VALUES (?1, '/tmp', 'bad', 1, 2, NULL, NULL)",
                        ["bad\nid"],
                    )
                    .expect("opencode malformed row");
            }
            _ => unreachable!(),
        }
    }

    fn write_excluded_sibling(adapter: &str, root: &Path) {
        match adapter {
            "codex" | "claude" | "pi" => {
                let subagents = root.join("subagents");
                std::fs::create_dir_all(&subagents).expect("subagent dir");
                let name = if adapter == "codex" {
                    "rollout-child.jsonl"
                } else {
                    "child.jsonl"
                };
                std::fs::write(subagents.join(name), "not json\n").expect("subagent fixture");
            }
            "grok" => {
                let subagent = root.join("subagents/child");
                std::fs::create_dir_all(&subagent).expect("grok subagent dir");
                std::fs::write(subagent.join("summary.json"), "not json").expect("grok subagent")
            }
            "opencode" => {
                opencode_connection(root)
                    .execute(
                        "INSERT INTO session(
                            id, directory, title, time_created, time_updated,
                            parent_id, time_archived
                         ) VALUES ('child', '/tmp', 'child', 1, 2, 'parent', NULL)",
                        [],
                    )
                    .expect("opencode child row");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn all_adapters_preserve_missing_optional_fields_and_ignore_unknown_fields() {
        for adapter in ADAPTER_NAMES {
            let missing_title = temp_dir(&format!("{adapter}-missing-title"));
            write_fixture(adapter, &missing_title, "missing-title", None, Some("/tmp"));
            let scan = scan_default_root(adapter, &missing_title);
            assert_eq!(scan.completion, ScanCompletion::Complete, "{adapter}");
            assert_eq!(scan.candidates.len(), 1, "{adapter}");
            assert!(scan.candidates[0].title.is_none(), "{adapter}");
            assert!(
                scan.candidates[0].fallback_title().contains("session ·"),
                "{adapter}"
            );
            let _ = std::fs::remove_dir_all(missing_title);

            let missing_cwd = temp_dir(&format!("{adapter}-missing-cwd"));
            write_fixture(adapter, &missing_cwd, "missing-cwd", Some("title"), None);
            let scan = scan_default_root(adapter, &missing_cwd);
            assert_eq!(scan.completion, ScanCompletion::Complete, "{adapter}");
            assert_eq!(scan.candidates.len(), 1, "{adapter}");
            assert!(scan.candidates[0].cwd.is_none(), "{adapter}");
            let _ = std::fs::remove_dir_all(missing_cwd);
        }
    }

    #[test]
    fn all_adapters_are_idempotent_across_duplicate_scans() {
        for adapter in ADAPTER_NAMES {
            let root = temp_dir(&format!("{adapter}-duplicate"));
            write_fixture(adapter, &root, "duplicate", Some("title"), Some("/tmp"));
            let first = scan_default_root(adapter, &root);
            let second = scan_default_root(adapter, &root);
            let mut catalog = super::super::ProjectCatalog::open_in_memory().expect("catalog");
            for candidate in first.candidates.iter().chain(&second.candidates) {
                catalog
                    .upsert_candidate(candidate)
                    .expect("duplicate upsert");
            }
            let count = catalog
                .snapshot(50)
                .expect("snapshot")
                .projects
                .iter()
                .map(|project| project.sessions.len())
                .sum::<usize>();
            assert_eq!(count, 1, "{adapter}");
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn all_adapters_isolate_malformed_siblings() {
        for adapter in ADAPTER_NAMES {
            let root = temp_dir(&format!("{adapter}-malformed-sibling"));
            write_fixture(adapter, &root, "valid", Some("title"), Some("/tmp"));
            write_malformed_sibling(adapter, &root);
            let scan = scan_default_root(adapter, &root);
            assert_eq!(scan.completion, ScanCompletion::Degraded, "{adapter}");
            assert_eq!(scan.candidates.len(), 1, "{adapter}");
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn all_adapters_exclude_subagents_or_temporary_records() {
        for adapter in ADAPTER_NAMES {
            let root = temp_dir(&format!("{adapter}-excluded"));
            write_fixture(adapter, &root, "valid", Some("title"), Some("/tmp"));
            write_excluded_sibling(adapter, &root);
            let scan = scan_default_root(adapter, &root);
            assert_eq!(scan.completion, ScanCompletion::Complete, "{adapter}");
            assert_eq!(scan.candidates.len(), 1, "{adapter}");
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn all_adapters_reuse_unchanged_watermarks_and_force_full_rescan() {
        for adapter in ADAPTER_NAMES {
            let root_path = temp_dir(&format!("{adapter}-incremental"));
            write_fixture(adapter, &root_path, "one", Some("title"), Some("/tmp"));
            let root = root(adapter, &root_path);
            let mut scanner = AdapterScanner::default();
            let first = scanner.scan_root(&root, false);
            let attempts = scanner.parse_attempts();
            let second = scanner.scan_root(&root, false);
            assert!(!first.reused_cache, "{adapter}");
            assert!(second.reused_cache, "{adapter}");
            assert_eq!(scanner.parse_attempts(), attempts, "{adapter}");
            let third = scanner.scan_root(&root, true);
            assert!(!third.reused_cache, "{adapter}");
            assert!(scanner.parse_attempts() > attempts, "{adapter}");
            let _ = std::fs::remove_dir_all(root_path);
        }
    }

    #[test]
    fn missing_default_root_is_not_installed_without_fatal_error() {
        let root = temp_dir("missing-default-parent").join("absent");
        let scan = scan_default_root("codex", &root);
        assert_eq!(scan.completion, ScanCompletion::NotInstalled);
        assert!(scan.candidates.is_empty());
        if let Some(parent) = root.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }

        let config = crate::config::ProjectsConfig::default();
        let resolved = resolve_roots_with_defaults(&config, vec![("codex", root)]);
        let codex = resolved
            .roots()
            .iter()
            .find(|root| root.adapter == "codex")
            .expect("codex default status");
        assert_eq!(codex.preflight, Some(ScanCompletion::NotInstalled));
    }

    #[test]
    fn traversal_is_bounded_at_depth_eight_and_oversized_input_is_isolated() {
        let root = temp_dir("bounded-depth");
        write_fixture("codex", &root, "root", Some("root"), Some("/tmp"));
        let mut directory = root.clone();
        for depth in 1..=9 {
            directory.push(format!("d{depth}"));
            std::fs::create_dir_all(&directory).expect("depth fixture dir");
            if depth == 8 || depth == 9 {
                write_fixture(
                    "codex",
                    &directory,
                    &format!("depth-{depth}"),
                    Some("depth"),
                    Some("/tmp"),
                );
            }
        }
        let oversized = root.join("rollout-oversized.jsonl");
        let file = File::create(&oversized).expect("oversized fixture");
        file.set_len(MAX_HISTORY_FILE_BYTES + 1)
            .expect("sparse oversized fixture");

        let scan = scan_default_root("codex", &root);
        assert_eq!(scan.completion, ScanCompletion::Degraded);
        let ids = scan
            .candidates
            .iter()
            .map(|candidate| candidate.identity.canonical_ref_value.as_str())
            .collect::<HashSet<_>>();
        assert!(ids.contains("root"));
        assert!(ids.contains("depth-8"));
        assert!(!ids.contains("depth-9"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_jsonl_with_valid_bounded_prefix_is_imported() {
        let root = temp_dir("oversized-valid-prefix");
        let path = root.join("rollout-large.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"large\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"text\":\"Large session\"}]}}\n"
            ),
        )
        .expect("large jsonl prefix");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open large jsonl")
            .set_len(MAX_HISTORY_FILE_BYTES + 1)
            .expect("extend large jsonl");

        let scan = scan_default_root("codex", &root);
        assert_eq!(scan.completion, ScanCompletion::Complete);
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].identity.canonical_ref_value, "large");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_directory_cycle_is_ignored_without_hiding_valid_siblings() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("cycle");
        write_fixture("codex", &root, "valid", Some("valid"), Some("/tmp"));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("cycle nested");
        symlink(&root, nested.join("back-to-root")).expect("cycle symlink");
        let scan = scan_default_root("codex", &root);
        assert_eq!(scan.completion, ScanCompletion::Complete);
        assert_eq!(scan.candidates.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_fixture_imports_new_response_item_title_and_cwd() {
        let root = temp_dir("codex");
        let path = root.join("rollout-fixture.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-1\",\"cwd\":\"/tmp\",\"future\":true}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Build Dense Tree\"}]}}\n"
            ),
        )
        .expect("write fixture");
        let scan = scan_default_root("codex", &root);
        assert_eq!(scan.completion, ScanCompletion::Complete);
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].identity.canonical_ref_value, "codex-1");
        assert_eq!(scan.candidates[0].fallback_title(), "Build Dense Tree");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_exec_is_automation_but_missing_originator_stays_interactive() {
        let root = temp_dir("codex-session-class");
        std::fs::write(
            root.join("rollout-exec.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"exec-1\",\"cwd\":\"/tmp\",\"originator\":\"codex_exec\",\"source\":\"exec\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"text\":\"automated task\"}]}}\n"
            ),
        )
        .expect("exec fixture");
        std::fs::write(
            root.join("rollout-legacy.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"legacy-1\",\"cwd\":\"/tmp\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"text\":\"interactive task\"}]}}\n"
            ),
        )
        .expect("legacy fixture");

        let scan = scan_default_root("codex", &root);
        let classes = scan
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.identity.canonical_ref_value.as_str(),
                    candidate.session_class,
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(classes.get("exec-1"), Some(&Some(SessionClass::Automation)));
        assert_eq!(
            classes.get("legacy-1"),
            Some(&Some(SessionClass::Interactive)),
            "old Codex files without originator must not be misclassified"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn opencode_classifier_artifacts_are_not_reimported_from_history() {
        let root = temp_dir("opencode-classifier-artifact");
        write_fixture(
            "opencode",
            &root,
            "classifier-1",
            Some("New session - 2026-08-17T00:00:00Z"),
            Some("/tmp/ork3"),
        );
        let connection = opencode_connection(&root);
        connection
            .execute(
                "INSERT INTO message(id, session_id, data)
                 VALUES ('message-1', 'classifier-1', ?1)",
                [serde_json::json!({"role": "user"}).to_string()],
            )
            .expect("classifier message");
        connection
            .execute(
                "INSERT INTO part(id, message_id, session_id, data)
                 VALUES ('part-1', 'message-1', 'classifier-1', ?1)",
                [serde_json::json!({
                    "type": "text",
                    "text": "把下面的编码会话按主题聚类。只输出 JSON。"
                })
                .to_string()],
            )
            .expect("classifier prompt");
        drop(connection);

        let scan = scan_default_root("opencode", &root);
        assert_eq!(scan.completion, ScanCompletion::Complete);
        assert!(scan.candidates.is_empty());
        assert!(scan.seen_source_keys.is_empty());
        assert_eq!(scan.excluded_source_keys.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_codex_adapters_report_known_user_weight() {
        let claude_root = temp_dir("claude-weight");
        write_json_lines(
            &claude_root.join("claude.jsonl"),
            &[
                serde_json::json!({"type":"user","sessionId":"c","cwd":"/tmp","message":{"content":[{"type":"text","text":"第一条真实请求"}]}}),
                serde_json::json!({"type":"user","sessionId":"c","cwd":"/tmp","message":{"content":[{"type":"text","text":"第二条真实请求"}]}}),
            ],
        );
        let claude = scan_default_root("claude", &claude_root);
        assert_eq!(claude.candidates[0].weight.turns, 2);
        assert!(claude.candidates[0].weight.chars > 0);
        assert!(claude.candidates[0].weight.known);

        let pi_root = temp_dir("pi-weight");
        write_json_lines(
            &pi_root.join("pi.jsonl"),
            &[
                serde_json::json!({"type":"session","id":"p","cwd":"/tmp"}),
                serde_json::json!({"type":"message","message":{"role":"user","content":"第一条真实请求"}}),
                serde_json::json!({"type":"message","message":{"role":"user","content":"第二条真实请求"}}),
            ],
        );
        let pi = scan_default_root("pi", &pi_root);
        assert_eq!(pi.candidates[0].weight.turns, 2);
        assert!(pi.candidates[0].weight.known);

        let grok_root = temp_dir("grok-weight");
        write_fixture("grok", &grok_root, "g", Some("Grok task"), Some("/tmp"));
        let grok_session = grok_root.join("g");
        write_json_lines(
            &grok_session.join("chat_history.jsonl"),
            &[
                serde_json::json!({"type":"user","content":[{"type":"text","text":"第一条真实请求"}]}),
                serde_json::json!({"type":"user","content":[{"type":"text","text":"第二条真实请求"}]}),
            ],
        );
        let grok = scan_default_root("grok", &grok_root);
        assert_eq!(grok.candidates[0].weight.turns, 2);
        assert!(grok.candidates[0].weight.known);

        let _ = std::fs::remove_dir_all(claude_root);
        let _ = std::fs::remove_dir_all(pi_root);
        let _ = std::fs::remove_dir_all(grok_root);
    }

    #[test]
    fn opencode_aggregates_user_weight_and_discards_default_title() {
        let root = temp_dir("opencode-weight");
        write_fixture(
            "opencode",
            &root,
            "weighted",
            Some("New session - 2026-08-17T00:00:00Z"),
            Some("/tmp"),
        );
        let connection = opencode_connection(&root);
        for (index, text) in ["第一条真实请求", "第二条真实请求"].into_iter().enumerate()
        {
            let message_id = format!("message-{index}");
            connection
                .execute(
                    "INSERT INTO message(id, session_id, data) VALUES (?1, 'weighted', ?2)",
                    params![message_id, serde_json::json!({"role":"user"}).to_string()],
                )
                .expect("message");
            connection
                .execute(
                    "INSERT INTO part(id, message_id, session_id, data) VALUES (?1, ?2, 'weighted', ?3)",
                    params![format!("part-{index}"), message_id, serde_json::json!({"type":"text","text":text}).to_string()],
                )
                .expect("part");
        }
        drop(connection);
        let scan = scan_default_root("opencode", &root);
        let candidate = &scan.candidates[0];
        assert_eq!(candidate.weight.turns, 2);
        assert!(candidate.weight.chars > 0);
        assert!(candidate.weight.known);
        assert!(
            candidate.title.is_none(),
            "default title should use fallback"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_file_does_not_hide_valid_sibling() {
        let root = temp_dir("malformed");
        std::fs::write(root.join("rollout-bad.jsonl"), "not json\n").expect("bad fixture");
        std::fs::write(
            root.join("rollout-good.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"good\",\"cwd\":\"/tmp\"}}\n",
        )
        .expect("good fixture");
        let scan = scan_default_root("codex", &root);
        assert_eq!(scan.completion, ScanCompletion::Degraded);
        assert_eq!(scan.candidates.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pi_uses_canonical_session_path_and_records_id_alias() {
        let root = temp_dir("pi");
        let path = root.join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"id\":\"pi-official\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"Ship it\"}}\n"
            ),
        )
        .expect("pi fixture");
        let scan = scan_default_root("pi", &root);
        assert_eq!(scan.candidates.len(), 1);
        let candidate = &scan.candidates[0];
        assert_eq!(
            candidate.identity.ref_kind,
            super::super::SessionRefKind::Path
        );
        assert_eq!(candidate.aliases.len(), 1);
        assert_eq!(
            candidate.aliases[0].identity.canonical_ref_value,
            "pi-official"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn grok_uses_id_when_present_and_path_when_absent() {
        let root = temp_dir("grok");
        let with_id = root.join("with-id");
        let without_id = root.join("%2Ftmp%2Fgrok");
        std::fs::create_dir_all(&with_id).expect("with id dir");
        std::fs::create_dir_all(&without_id).expect("without id dir");
        std::fs::write(
            with_id.join("summary.json"),
            "{\"id\":\"grok-id\",\"generated_title\":\"With ID\"}",
        )
        .expect("with id summary");
        std::fs::write(
            without_id.join("summary.json"),
            "{\"generated_title\":\"Path fallback\"}",
        )
        .expect("path summary");
        let scan = scan_default_root("grok", &root);
        assert_eq!(scan.candidates.len(), 2);
        assert!(scan
            .candidates
            .iter()
            .any(
                |candidate| candidate.identity.canonical_ref_value == "grok-id"
                    && candidate.aliases.len() == 1
            ));
        assert!(scan
            .candidates
            .iter()
            .any(|candidate| candidate.identity.ref_kind == super::super::SessionRefKind::Path));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_scan_reuses_watermark_and_forced_scan_reparses() {
        let root_path = temp_dir("incremental");
        std::fs::write(
            root_path.join("rollout-one.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"one\"}}\n",
        )
        .expect("fixture");
        let root = root("codex", &root_path);
        let mut scanner = AdapterScanner::default();
        let first = scanner.scan_root(&root, false);
        let attempts = scanner.parse_attempts();
        let second = scanner.scan_root(&root, false);
        assert!(!first.reused_cache);
        assert!(second.reused_cache);
        assert_eq!(scanner.parse_attempts(), attempts);
        let third = scanner.scan_root(&root, true);
        assert!(!third.reused_cache);
        assert!(scanner.parse_attempts() > attempts);
        let _ = std::fs::remove_dir_all(root_path);
    }

    #[test]
    fn explicit_roots_are_canonicalized_deduped_and_files_are_diagnostics() {
        let valid = temp_dir("configured-valid");
        let file = valid.join("not-a-root");
        std::fs::write(&file, "fixture").expect("configured file");
        let config: crate::config::ProjectsConfig = toml::from_str(&format!(
            "[adapters.codex]\nroots = [{:?}, {:?}, {:?}]\n",
            valid, valid, file
        ))
        .expect("projects config");
        let resolved = resolve_roots_with_defaults(&config, Vec::new());
        assert_eq!(resolved.allowed_roots("codex").len(), 1);
        assert_eq!(resolved.diagnostics().len(), 1);
        assert!(resolved.roots().iter().any(|root| {
            root.adapter == "codex" && root.preflight == Some(ScanCompletion::ConfigurationError)
        }));
        let _ = std::fs::remove_dir_all(valid);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_history_file_is_never_read() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("symlink");
        let outside_root = temp_dir("outside");
        let outside = outside_root.join("rollout-secret.jsonl");
        std::fs::write(
            &outside,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"secret\"}}\n",
        )
        .expect("outside fixture");
        symlink(&outside, root.join("rollout-link.jsonl")).expect("symlink fixture");
        let scan = scan_default_root("codex", &root);
        assert!(scan.candidates.is_empty());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside_root);
    }

    #[test]
    fn percent_decoding_handles_grok_working_directory() {
        assert_eq!(
            percent_decode("%2FUsers%2Fexample%2FDocuments%2Fait").as_deref(),
            Some("/Users/example/Documents/ait")
        );
        assert_eq!(percent_decode("%ZZ"), None);
    }

    #[test]
    #[ignore = "reads only configured/default local history roots; run through just projects-check"]
    fn strict_probe_standard_roots() {
        let loaded = crate::config::Config::load();
        match strict_probe(&loaded.config.projects) {
            Ok(statuses) => {
                for (adapter, state) in statuses {
                    eprintln!("{adapter}: {state}");
                }
            }
            Err(failures) => panic!("strict Projects probe failed: {}", failures.join(", ")),
        }
    }

    #[test]
    fn titles_drop_harness_injected_preamble() {
        // Real shapes from this machine: 9,714 of 10,291 sessions had titles like these, which
        // are harness preamble rather than anything the user typed.
        assert_eq!(
            safe_title("<user_instructions>\nbe brief\n</user_instructions>"),
            None
        );
        assert_eq!(
            safe_title("# AGENTS.md instructions for /Users/example/x <INSTRUCTIONS> ..."),
            None
        );
        // An unclosed block means everything after it is preamble too.
        assert_eq!(
            safe_title("<environment_context> <cwd>/Users/example</cwd>"),
            None
        );

        // The user's own words survive, including when they follow a preamble block.
        assert_eq!(
            safe_title("<user_instructions>x</user_instructions>\n修复登录 bug"),
            Some("修复登录 bug".to_string())
        );
        assert_eq!(safe_title("修复登录 bug"), Some("修复登录 bug".to_string()));
    }

    #[test]
    fn thin_sessions_are_skipped_but_short_openers_are_not() {
        // A couple of throwaway turns: nothing for a classifier to work with.
        let mut thin = SessionWeight::default();
        thin.record("hi");
        thin.record("?");
        assert!(!thin.is_substantive());

        // One detailed request is substantive even though it is a single turn. Requiring both
        // signals discarded a real 109k-character bug report that took only two turns.
        let mut single_long = SessionWeight::default();
        single_long.record(&"帮我看下 openclaw 为什么不继续工作了".repeat(10));
        assert!(single_long.is_substantive());

        // Harness preamble is not the user's words: four "hi" turns preceded by an AGENTS.md
        // block measured 264 characters and wrongly looked substantive.
        let mut preamble_only = SessionWeight::default();
        preamble_only
            .record("<environment_context><cwd>/Users/example</cwd></environment_context>");
        for _ in 0..4 {
            preamble_only.record("hi");
        }
        assert!(!preamble_only.is_substantive());

        // Real case from this machine: opens with "继续" but runs 11 turns and ~13k characters.
        // Judging by the opener alone would have thrown this away.
        let mut deep = SessionWeight::default();
        deep.record("继续");
        for _ in 0..10 {
            deep.record("请把 Projects 侧栏的折叠状态持久化到 snapshot，并补充回归测试");
        }
        assert!(deep.is_substantive());
    }

    #[test]
    fn title_prefers_the_most_descriptive_opening_turn() {
        let mut picker = TitlePicker::default();
        picker.offer("在吗");
        picker.offer("继续");
        picker.offer("帮我把语义聚类接到后台增量流程里");
        // The first message is a greeting; the third says what the session is about.
        assert_eq!(
            picker.take().as_deref(),
            Some("帮我把语义聚类接到后台增量流程里")
        );
    }

    #[test]
    fn title_scan_stops_before_reading_the_whole_transcript() {
        // A descriptive turn settles the scan quickly.
        let mut picker = TitlePicker::default();
        for _ in 0..TITLE_SCAN_TURNS {
            picker.offer("把语义聚类接到后台增量流程里");
        }
        assert!(picker.is_settled(), "a descriptive title should settle");

        // A session of only one-word turns must still terminate rather than read everything.
        let mut stubborn = TitlePicker::default();
        for _ in 0..MAX_TITLE_SCAN_TURNS {
            stubborn.offer("hi");
        }
        assert!(stubborn.is_settled(), "the hard cap must stop the scan");
    }

    #[test]
    fn injected_preamble_does_not_consume_the_title_scan_budget() {
        // Real shape from this machine: 8.5k of AGENTS.md, then "hi" twice, and only the fourth
        // turn says what the session is about. Counting preamble against the budget settled the
        // scan too early and left the title as "hi".
        let mut picker = TitlePicker::default();
        picker.offer("# AGENTS.md instructions for /Users/example <INSTRUCTIONS> ...");
        picker.offer("hi");
        picker.offer("hi");
        picker.offer("看下 openclaw 的浏览器为什么能做到跟其他工具不一样的事情");
        assert_eq!(
            picker.take().as_deref(),
            Some("看下 openclaw 的浏览器为什么能做到跟其他工具不一样的事情")
        );
    }
}
