use std::path::Path;

use super::{ProjectClassification, ProjectKind};

const PASEO_MULTICA_AGENT_PREFIX: &str = "paseo-multica-agent-";

pub(crate) fn classify(cwd: Option<&Path>) -> ProjectClassification {
    let Some(cwd) = cwd else {
        return ProjectClassification::unclassified();
    };
    if is_ephemeral_agent_cwd(cwd) {
        return ProjectClassification::ephemeral_agent();
    }
    if !cwd.is_dir() {
        return ProjectClassification::unclassified();
    }

    if let Some(info) = crate::workspace::git_space_metadata(cwd) {
        return ProjectClassification::new(
            ProjectKind::GitCommonDir,
            info.key,
            git_display_name(cwd),
            "git-common-dir".to_string(),
        );
    }

    let Ok(canonical) = std::fs::canonicalize(cwd) else {
        return ProjectClassification::unclassified();
    };
    ProjectClassification::new(
        ProjectKind::Cwd,
        canonical.to_string_lossy().into_owned(),
        path_display_name(&canonical),
        "cwd".to_string(),
    )
}

/// Paseo/Multica launches OpenCode agents in one disposable directory per run. Those directories
/// are execution sandboxes, not projects, and may disappear before the next Catalog scan.
pub(crate) fn is_ephemeral_agent_cwd(cwd: &Path) -> bool {
    let is_paseo_agent = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(PASEO_MULTICA_AGENT_PREFIX));
    if !is_paseo_agent {
        return false;
    }

    let Some(parent) = cwd.parent() else {
        return false;
    };
    let system_temp = std::env::temp_dir();
    parent == system_temp
        || std::fs::canonicalize(parent)
            .ok()
            .zip(std::fs::canonicalize(system_temp).ok())
            .is_some_and(|(parent, system_temp)| parent == system_temp)
}

fn git_display_name(cwd: &Path) -> String {
    let Some(info) = crate::workspace::git_space_metadata(cwd) else {
        return path_display_name(cwd);
    };
    let common = Path::new(&info.key);
    if common.file_name().and_then(|name| name.to_str()) == Some(".git") {
        return common
            .parent()
            .map(path_display_name)
            .unwrap_or_else(|| path_display_name(cwd));
    }
    let raw = common
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("repo");
    raw.strip_suffix(".git")
        .filter(|name| !name.is_empty())
        .unwrap_or(raw)
        .to_string()
}

fn path_display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-project-classifier-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn init_git_repo(path: &Path) {
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
    }

    #[test]
    fn equal_non_git_cwd_has_equal_project_identity() {
        let cwd = temp_dir("cwd");
        let first = classify(Some(&cwd));
        let second = classify(Some(&cwd));
        assert_eq!(first.kind, ProjectKind::Cwd);
        assert_eq!(first.canonical_key, second.canonical_key);
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn missing_or_unreadable_cwd_is_unclassified() {
        let missing = temp_dir("missing").join("gone");
        let project = classify(Some(&missing));
        assert_eq!(project.kind, ProjectKind::Unclassified);
        assert_eq!(classify(None).kind, ProjectKind::Unclassified);
    }

    #[test]
    fn paseo_agent_temp_cwd_is_ephemeral_even_after_child_directory_disappears() {
        let cwd = std::env::temp_dir().join("paseo-multica-agent-test-session");
        let _ = std::fs::remove_dir_all(&cwd);

        let project = classify(Some(&cwd));

        assert_eq!(project.kind, ProjectKind::Unclassified);
        assert_eq!(project.evidence, "ephemeral-agent-cwd");
        assert_eq!(project.display_name, "Ephemeral agent sessions");
    }

    #[test]
    fn similarly_named_persistent_directory_is_not_ephemeral() {
        let root = temp_dir("persistent-parent");
        let cwd = root.join("paseo-multica-agent-real-project");
        std::fs::create_dir_all(&cwd).expect("persistent cwd");

        let project = classify(Some(&cwd));

        assert_eq!(project.kind, ProjectKind::Cwd);
        assert_eq!(project.display_name, "paseo-multica-agent-real-project");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_checkout_uses_common_dir_and_stable_display_name() {
        let repo = temp_dir("repo");
        init_git_repo(&repo);
        let project = classify(Some(&repo));
        assert_eq!(project.kind, ProjectKind::GitCommonDir);
        assert_eq!(
            project.display_name,
            repo.file_name().unwrap().to_string_lossy()
        );
        assert!(project.canonical_path.ends_with(".git"));
        let _ = std::fs::remove_dir_all(repo);
    }
}
