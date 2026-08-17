use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const PROJECTS_SCHEMA_VERSION: u32 = 1;
const SESSION_KEY_DOMAIN: &[u8] = b"herdr-projects-session-v1\0";
const PROJECT_KEY_DOMAIN: &[u8] = b"herdr-projects-project-v1\0";
/// Inputs that decide a session's topic. Chatting further in the same session changes
/// `last_activity_at` but not this fingerprint, so it is not reclassified.
const SEMANTIC_FINGERPRINT_DOMAIN: &[u8] = b"ork3-semantic-v1\0";
const SEMANTIC_TOPIC_DOMAIN: &[u8] = b"ork3-semantic-topic-v1\0";

/// Fingerprint of the metadata sent to a classifier.
///
/// Only these three fields are ever sent, so they are exactly what must invalidate a cached
/// topic. Transcript bodies are deliberately excluded — see SPEC §3.2.
pub(crate) fn semantic_fingerprint(title: &str, cwd: Option<&str>, backend: &str) -> String {
    versioned_key(
        SEMANTIC_FINGERPRINT_DOMAIN,
        &[
            title.as_bytes(),
            cwd.unwrap_or_default().as_bytes(),
            backend.as_bytes(),
        ],
    )
}

/// Stable key for a topic label, so the same topic name always maps to the same Project.
pub(crate) fn semantic_topic_key(label: &str) -> String {
    versioned_key(
        SEMANTIC_TOPIC_DOMAIN,
        &[normalize_topic_label(label).as_bytes()],
    )
}

/// Stable identity for repeated session templates.
///
/// Automation detection deliberately ignores casing and whitespace differences so a runner that
/// wraps the same prompt differently across versions still forms one template group.
pub(crate) fn normalize_session_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Case- and whitespace-insensitive topic identity.
///
/// Classifiers return `"Herdr 重构"` and `"herdr  重构"` for the same cluster across batches;
/// without this they would become two Projects.
fn normalize_topic_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionRefKind {
    Id,
    Path,
}

impl SessionRefKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Path => "path",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionIdentity {
    pub backend: String,
    pub ref_kind: SessionRefKind,
    pub canonical_ref_value: String,
    pub stable_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdentityError {
    InvalidBackend,
    EmptyReference,
    InvalidId,
    IdTooLong,
    InvalidPath,
    PathOutsideAllowedRoots,
    MissingPath,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidBackend => "invalid backend",
            Self::EmptyReference => "empty session reference",
            Self::InvalidId => "session id contains a control character",
            Self::IdTooLong => "session id exceeds 512 bytes",
            Self::InvalidPath => "invalid session path",
            Self::PathOutsideAllowedRoots => "session path is outside allowed roots",
            Self::MissingPath => "session path does not exist",
        };
        f.write_str(message)
    }
}

impl std::error::Error for IdentityError {}

impl SessionIdentity {
    pub(crate) fn id(backend: &str, value: &str) -> Result<Self, IdentityError> {
        let backend = normalize_backend(backend)?;
        validate_id(value)?;
        Ok(Self::from_canonical(
            backend,
            SessionRefKind::Id,
            value.to_string(),
        ))
    }

    pub(crate) fn path(
        backend: &str,
        value: &Path,
        base_dir: &Path,
        allowed_roots: &[PathBuf],
        allow_missing_live_path: bool,
    ) -> Result<Self, IdentityError> {
        let backend = normalize_backend(backend)?;
        let canonical_ref_value =
            normalize_path(value, base_dir, allowed_roots, allow_missing_live_path)?
                .to_string_lossy()
                .into_owned();
        Ok(Self::from_canonical(
            backend,
            SessionRefKind::Path,
            canonical_ref_value,
        ))
    }

    pub(crate) fn from_canonical(
        backend: String,
        ref_kind: SessionRefKind,
        canonical_ref_value: String,
    ) -> Self {
        let stable_key = versioned_key(
            SESSION_KEY_DOMAIN,
            &[
                backend.as_bytes(),
                ref_kind.as_str().as_bytes(),
                canonical_ref_value.as_bytes(),
            ],
        );
        Self {
            backend,
            ref_kind,
            canonical_ref_value,
            stable_key,
        }
    }
}

pub(crate) fn normalize_backend(value: &str) -> Result<String, IdentityError> {
    let normalized = value.trim().to_ascii_lowercase();
    let mut chars = normalized.chars();
    let Some(first) = chars.next() else {
        return Err(IdentityError::InvalidBackend);
    };
    if normalized.len() > 64 || !first.is_ascii_alphanumeric() {
        return Err(IdentityError::InvalidBackend);
    }
    if chars.any(|character| {
        !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    }) {
        return Err(IdentityError::InvalidBackend);
    }
    Ok(normalized)
}

fn validate_id(value: &str) -> Result<(), IdentityError> {
    if value.is_empty() {
        return Err(IdentityError::EmptyReference);
    }
    if value.len() > 512 {
        return Err(IdentityError::IdTooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(IdentityError::InvalidId);
    }
    Ok(())
}

fn normalize_path(
    value: &Path,
    base_dir: &Path,
    allowed_roots: &[PathBuf],
    allow_missing_live_path: bool,
) -> Result<PathBuf, IdentityError> {
    if value.as_os_str().is_empty() || allowed_roots.is_empty() {
        return Err(IdentityError::InvalidPath);
    }
    let expanded = expand_current_home(value)?;
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    };
    let lexical = lexical_normalize(&absolute)?;
    let normalized = if lexical.exists() {
        std::fs::canonicalize(&lexical).map_err(|_| IdentityError::InvalidPath)?
    } else if allow_missing_live_path {
        canonicalize_missing_path(&lexical)?
    } else {
        return Err(IdentityError::MissingPath);
    };

    let within_root = allowed_roots.iter().any(|root| {
        let normalized_root = std::fs::canonicalize(root)
            .ok()
            .or_else(|| lexical_normalize(root).ok());
        normalized_root
            .as_ref()
            .is_some_and(|root| normalized.starts_with(root))
    });
    if !within_root {
        return Err(IdentityError::PathOutsideAllowedRoots);
    }
    Ok(normalized)
}

fn canonicalize_missing_path(path: &Path) -> Result<PathBuf, IdentityError> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or(IdentityError::InvalidPath)?;
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or(IdentityError::InvalidPath)?;
    }
    let mut normalized = std::fs::canonicalize(existing).map_err(|_| IdentityError::InvalidPath)?;
    for part in suffix.iter().rev() {
        normalized.push(part);
    }
    Ok(normalized)
}

fn expand_current_home(path: &Path) -> Result<PathBuf, IdentityError> {
    let value = path.to_string_lossy();
    if value == "~" || value.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or(IdentityError::InvalidPath)?;
        let suffix = value.strip_prefix("~/").unwrap_or_default();
        return Ok(PathBuf::from(home).join(suffix));
    }
    Ok(path.to_path_buf())
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, IdentityError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(IdentityError::InvalidPath);
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    if !normalized.is_absolute() {
        return Err(IdentityError::InvalidPath);
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i64)]
pub(crate) enum SourcePriority {
    TranscriptFile = 1,
    PrimaryIndex = 2,
    RuntimeReport = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateField<T> {
    pub value: T,
    pub observed_at: i64,
    pub priority: SourcePriority,
    pub source_key: String,
}

impl<T> CandidateField<T> {
    pub(crate) fn outranks(&self, observed_at: i64, priority: i64, source_key: &str) -> bool {
        self.observed_at > observed_at
            || (self.observed_at == observed_at
                && ((self.priority as i64) > priority
                    || ((self.priority as i64) == priority
                        && self.source_key.as_str() < source_key)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeMapping {
    pub workspace_id: String,
    pub pane_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionAliasCandidate {
    pub identity: SessionIdentity,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionCandidate {
    pub identity: SessionIdentity,
    pub title: Option<CandidateField<String>>,
    pub cwd: Option<CandidateField<PathBuf>>,
    pub transcript_ref: Option<CandidateField<String>>,
    pub first_activity_at: i64,
    pub last_activity_at: i64,
    pub adapter: String,
    pub root_key: String,
    pub source_key: String,
    pub observed_at: i64,
    pub aliases: Vec<SessionAliasCandidate>,
    pub runtime: Option<RuntimeMapping>,
    /// How much the user actually said. Thin sessions are skipped by the classifier.
    pub weight: super::adapters::SessionWeight,
    /// `None` means this source has no authority to change an existing class (runtime reports).
    pub session_class: Option<SessionClass>,
}

impl SessionCandidate {
    pub(crate) fn fallback_title(&self) -> String {
        self.title.as_ref().map_or_else(
            || {
                let backend = title_case_backend(&self.identity.backend);
                format!("{backend} session · {}", &self.identity.stable_key[..8])
            },
            |field| field.value.clone(),
        )
    }
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionClass {
    #[default]
    Interactive,
    Automation,
    Ephemeral,
}

impl SessionClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Automation => "automation",
            Self::Ephemeral => "ephemeral",
        }
    }
}

fn title_case_backend(backend: &str) -> String {
    let mut chars = backend.chars();
    let Some(first) = chars.next() else {
        return "Agent".to_string();
    };
    first.to_ascii_uppercase().to_string() + chars.as_str()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    GitCommonDir,
    Cwd,
    /// Grouped by topic from a local Agent CLI rather than by path.
    ///
    /// Semantic Projects merge sessions across different cwds and backends, so this kind is the
    /// only one whose canonical key is not derived from the filesystem.
    Semantic,
    Unclassified,
}

impl ProjectKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::GitCommonDir => "git-common-dir",
            Self::Cwd => "cwd",
            Self::Semantic => "semantic",
            Self::Unclassified => "unclassified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectClassification {
    pub canonical_key: String,
    pub kind: ProjectKind,
    pub canonical_path: String,
    pub display_name: String,
    pub evidence: String,
}

impl ProjectClassification {
    pub(crate) fn new(
        kind: ProjectKind,
        canonical_path: String,
        display_name: String,
        evidence: String,
    ) -> Self {
        let canonical_key = versioned_key(
            PROJECT_KEY_DOMAIN,
            &[kind.as_str().as_bytes(), canonical_path.as_bytes()],
        );
        Self {
            canonical_key,
            kind,
            canonical_path,
            display_name,
            evidence,
        }
    }

    pub(crate) fn unclassified() -> Self {
        Self::new(
            ProjectKind::Unclassified,
            "unclassified".to_string(),
            "Unclassified".to_string(),
            "cwd unavailable".to_string(),
        )
    }

    /// Internal holding group for sessions launched in disposable agent sandboxes.
    ///
    /// The assignment remains available to the session and semantic indexes, while directory
    /// snapshots omit it by evidence so a temporary execution cwd is never presented as a user
    /// project.
    pub(crate) fn ephemeral_agent() -> Self {
        Self::new(
            ProjectKind::Unclassified,
            "ephemeral-agent-cwd".to_string(),
            "Ephemeral agent sessions".to_string(),
            "ephemeral-agent-cwd".to_string(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionCursor {
    pub last_activity_at: i64,
    pub stable_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexedSessionSummary {
    pub stable_key: String,
    pub backend: String,
    pub ref_kind: SessionRefKind,
    pub title: String,
    pub cwd: Option<String>,
    pub first_activity_at: i64,
    pub last_activity_at: i64,
    pub live: bool,
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    pub runtime_generation: Option<u64>,
    /// Server-owned classification. Additive/defaulted so v1 clients remain compatible.
    #[serde(default)]
    pub session_class: SessionClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AutomationTemplateSummary {
    /// Latest session in the template, useful as a stable representative for inspection.
    pub representative_session_key: String,
    pub title: String,
    pub backend: String,
    pub count: u64,
    pub last_activity_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectSummary {
    pub canonical_key: String,
    pub kind: ProjectKind,
    pub display_name: String,
    pub canonical_path: String,
    pub sessions: Vec<IndexedSessionSummary>,
    /// Repeated automation templates stay visible as one collapsed row under their cwd Project.
    #[serde(default)]
    pub automation: Vec<AutomationTemplateSummary>,
    /// Interactive sessions below the substantive threshold, retained but collapsed by clients.
    #[serde(default)]
    pub thin_count: u64,
    pub next_cursor: Option<SessionCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectSessionsPage {
    pub projects_schema_version: u32,
    pub revision: u64,
    pub project_key: String,
    pub sessions: Vec<IndexedSessionSummary>,
    pub next_cursor: Option<SessionCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AdapterScanStatus {
    pub adapter: String,
    pub state: String,
    pub diagnostic_category: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectsSnapshot {
    pub projects_schema_version: u32,
    pub revision: u64,
    /// Filesystem-backed directory groups (`git-common-dir`, `cwd`, or `unclassified`).
    pub projects: Vec<ProjectSummary>,
    /// Semantic topic groups inferred independently from directory ownership.
    ///
    /// This is an additive v1 field so older snapshots deserialize as an empty topic list.
    #[serde(default)]
    pub topics: Vec<ProjectSummary>,
    pub scan_status: Vec<AdapterScanStatus>,
    pub diagnostic_category: Option<String>,
}

impl ProjectsSnapshot {
    pub(crate) fn empty() -> Self {
        Self {
            projects_schema_version: PROJECTS_SCHEMA_VERSION,
            revision: 0,
            projects: Vec::new(),
            topics: Vec::new(),
            scan_status: Vec::new(),
            diagnostic_category: None,
        }
    }

    pub(crate) fn degraded(category: &str) -> Self {
        let mut snapshot = Self::empty();
        snapshot.diagnostic_category = Some(category.to_string());
        snapshot
    }
}

/// A session awaiting topic classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSemanticSession {
    pub stable_key: String,
    pub title: String,
    pub cwd: Option<String>,
    pub backend: String,
    pub stored_fingerprint: Option<String>,
    /// Other pending sessions with the same normalized title. Only this representative is sent
    /// to the classifier; a successful result is expanded to every member atomically.
    pub duplicates: Vec<PendingSemanticDuplicate>,
    /// A current assignment from another session with this title, when one already exists.
    pub inherited_topic: Option<InheritedSemanticTopic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSemanticDuplicate {
    pub stable_key: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InheritedSemanticTopic {
    pub topic_key: String,
    pub topic_label: String,
    pub backend_used: String,
    pub model_used: Option<String>,
}

/// One classified session, ready to be written to the Catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticAssignment {
    pub session_key: String,
    pub topic_key: String,
    pub topic_label: String,
    pub fingerprint: String,
    pub backend_used: String,
    pub model_used: Option<String>,
}

/// A validated label-only semantic maintenance operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticTopicMerge {
    pub into: String,
    pub from: Vec<String>,
}

fn versioned_key(domain: &[u8], fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-project-domain-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn backend_normalization_is_ascii_and_bounded() {
        assert_eq!(normalize_backend(" Codex ").unwrap(), "codex");
        assert_eq!(normalize_backend("open_code-1.2").unwrap(), "open_code-1.2");
        assert_eq!(
            normalize_backend("-codex"),
            Err(IdentityError::InvalidBackend)
        );
        assert_eq!(
            normalize_backend("cødex"),
            Err(IdentityError::InvalidBackend)
        );
        assert_eq!(
            normalize_backend(&"a".repeat(65)),
            Err(IdentityError::InvalidBackend)
        );
    }

    #[test]
    fn id_identity_preserves_exact_bytes_and_rejects_controls() {
        let upper = SessionIdentity::id("codex", " Session-A ").unwrap();
        let lower = SessionIdentity::id("codex", " session-a ").unwrap();
        assert_ne!(upper.stable_key, lower.stable_key);
        assert_eq!(upper.canonical_ref_value, " Session-A ");
        assert_eq!(
            SessionIdentity::id("codex", ""),
            Err(IdentityError::EmptyReference)
        );
        assert_eq!(
            SessionIdentity::id("codex", "bad\nvalue"),
            Err(IdentityError::InvalidId)
        );
        assert_eq!(
            SessionIdentity::id("codex", &"x".repeat(513)),
            Err(IdentityError::IdTooLong)
        );
    }

    #[test]
    fn length_prefixed_identity_has_no_delimiter_collision() {
        let first = SessionIdentity::id("a-b", "c").unwrap();
        let second = SessionIdentity::id("a", "b-c").unwrap();
        assert_ne!(first.stable_key, second.stable_key);
        assert_eq!(first.stable_key.len(), 64);
    }

    #[test]
    fn path_identity_allows_missing_live_path_inside_root_only() {
        let root = temp_dir("allowed");
        let inside = root.join("future/session.jsonl");
        let allowed =
            SessionIdentity::path("pi", &inside, &root, std::slice::from_ref(&root), true)
                .expect("inside live path");
        assert_eq!(allowed.ref_kind, SessionRefKind::Path);

        let outside = root
            .parent()
            .unwrap_or(Path::new("/"))
            .join("outside.jsonl");
        assert_eq!(
            SessionIdentity::path("pi", &outside, &root, std::slice::from_ref(&root), true),
            Err(IdentityError::PathOutsideAllowedRoots)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn path_identity_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("symlink-root");
        let outside = temp_dir("symlink-outside");
        let outside_file = outside.join("session.jsonl");
        std::fs::write(&outside_file, "{}").expect("write outside file");
        let link = root.join("escape.jsonl");
        symlink(&outside_file, &link).expect("create symlink");
        assert_eq!(
            SessionIdentity::path("pi", &link, &root, std::slice::from_ref(&root), false),
            Err(IdentityError::PathOutsideAllowedRoots)
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn candidate_field_tie_break_is_total() {
        let runtime = CandidateField {
            value: "runtime",
            observed_at: 10,
            priority: SourcePriority::RuntimeReport,
            source_key: "z".to_string(),
        };
        assert!(runtime.outranks(10, SourcePriority::PrimaryIndex as i64, "a"));

        let lexical = CandidateField {
            value: "a",
            observed_at: 10,
            priority: SourcePriority::PrimaryIndex,
            source_key: "a".to_string(),
        };
        assert!(lexical.outranks(10, SourcePriority::PrimaryIndex as i64, "b"));
        assert!(!lexical.outranks(11, SourcePriority::TranscriptFile as i64, "z"));
    }

    #[test]
    fn fallback_title_is_stable_and_backend_specific() {
        let identity = SessionIdentity::id("codex", "s1").unwrap();
        let candidate = SessionCandidate {
            identity: identity.clone(),
            title: None,
            cwd: None,
            transcript_ref: None,
            first_activity_at: 1,
            last_activity_at: 2,
            adapter: "codex".to_string(),
            root_key: "root".to_string(),
            source_key: "source".to_string(),
            observed_at: 2,
            aliases: Vec::new(),
            runtime: None,
            weight: Default::default(),
            session_class: Some(SessionClass::Interactive),
        };
        assert_eq!(
            candidate.fallback_title(),
            format!("Codex session · {}", &identity.stable_key[..8])
        );
    }
}
