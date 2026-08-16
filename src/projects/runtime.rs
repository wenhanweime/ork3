use std::path::{Path, PathBuf};

use super::adapters::AdapterRootSet;
use super::{CandidateField, RuntimeMapping, SessionCandidate, SessionIdentity, SourcePriority};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeLease {
    pub session_key: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub generation: u64,
    pub observed_at: i64,
}

pub(crate) fn identity_from_report(
    roots: &AdapterRootSet,
    adapter: &str,
    session_ref: &crate::agent_resume::AgentSessionRef,
) -> Option<SessionIdentity> {
    if !super::adapters::ADAPTER_NAMES.contains(&adapter) {
        return None;
    }
    match session_ref.kind {
        crate::agent_resume::AgentSessionRefKind::Id => {
            SessionIdentity::id(adapter, &session_ref.value).ok()
        }
        crate::agent_resume::AgentSessionRefKind::Path => SessionIdentity::path(
            adapter,
            Path::new(&session_ref.value),
            Path::new(std::path::MAIN_SEPARATOR_STR),
            roots.allowed_roots(adapter),
            true,
        )
        .ok(),
    }
}

pub(crate) struct RuntimeCandidateInput<'a> {
    pub identity: SessionIdentity,
    pub cwd: PathBuf,
    pub workspace_id: &'a str,
    pub pane_id: &'a str,
    pub generation: u64,
    pub observed_at: i64,
}

pub(crate) fn candidate_from_report(input: RuntimeCandidateInput<'_>) -> SessionCandidate {
    let source_key = format!("runtime:{}:{}", input.workspace_id, input.pane_id);
    SessionCandidate {
        identity: input.identity,
        title: None,
        cwd: Some(CandidateField {
            value: input.cwd,
            observed_at: input.observed_at,
            priority: SourcePriority::RuntimeReport,
            source_key: source_key.clone(),
        }),
        transcript_ref: None,
        first_activity_at: input.observed_at,
        last_activity_at: input.observed_at,
        adapter: "runtime".to_string(),
        root_key: "runtime".to_string(),
        source_key,
        observed_at: input.observed_at,
        aliases: Vec::new(),
        runtime: Some(RuntimeMapping {
            workspace_id: input.workspace_id.to_string(),
            pane_id: input.pane_id.to_string(),
            generation: input.generation,
        }),
        // A runtime report carries no transcript, so weight comes from the file scan instead.
        weight: super::adapters::SessionWeight::default(),
        session_class: None,
    }
}

pub(crate) fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_runtime_identity_uses_the_same_native_tuple_as_file_scans() {
        let roots = AdapterRootSet::default();
        let session_ref = crate::agent_resume::AgentSessionRef::id("same-id").unwrap();
        let identity = identity_from_report(&roots, "codex", &session_ref).unwrap();
        assert_eq!(
            identity,
            SessionIdentity::id("codex", "same-id").expect("file identity")
        );
    }
}
