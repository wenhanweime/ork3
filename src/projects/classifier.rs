use std::path::Path;

use super::{ProjectClassification, ProjectKind};

const TEMP_RUNNER_PREFIXES: [&str; 2] = ["paseo-multica-agent-", "ork-direct-accept."];
const RUNTIME_STATE_DIR: &str = "general";

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

/// Recognizes disposable Agent launch and application-state directories conservatively.
///
/// Existing user projects win unless their location and name both identify an application-owned
/// scratch area. Missing paths can still be recognized by their generated epoch/UUID leaf.
pub(crate) fn is_ephemeral_agent_cwd(cwd: &Path) -> bool {
    if is_temp_runner_path(cwd) || is_application_support_scratch(cwd) {
        return true;
    }
    !cwd.is_dir()
        && cwd
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| is_generated_temp_leaf(name) || is_uuid(name))
}

fn is_temp_runner_path(cwd: &Path) -> bool {
    let temp_roots = [
        std::env::temp_dir(),
        Path::new("/tmp").to_path_buf(),
        Path::new("/private/tmp").to_path_buf(),
    ];
    temp_roots.iter().any(|root| {
        cwd.strip_prefix(root).ok().is_some_and(|relative| {
            relative
                .components()
                .next()
                .and_then(|component| component.as_os_str().to_str())
                .is_some_and(|name| {
                    TEMP_RUNNER_PREFIXES
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                })
        })
    })
}

fn is_application_support_scratch(cwd: &Path) -> bool {
    let components = cwd
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    let Some(index) = components
        .windows(2)
        .position(|window| window == ["Library", "Application Support"])
    else {
        return false;
    };
    let tail = &components[index + 2..];
    if tail.len() == 2 && tail[0].contains('.') && tail[1].eq_ignore_ascii_case(RUNTIME_STATE_DIR) {
        return true;
    }
    tail.last().is_some_and(|name| is_generated_temp_leaf(name))
}

fn is_generated_temp_leaf(name: &str) -> bool {
    let Some((prefix, epoch)) = name.rsplit_once("-temp-") else {
        return false;
    };
    !prefix.is_empty() && epoch.len() >= 10 && epoch.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_uuid(name: &str) -> bool {
    let parts = name.split('-').collect::<Vec<_>>();
    let expected = [8, 4, 4, 4, 12];
    parts.len() == expected.len()
        && parts.iter().zip(expected).all(|(part, len)| {
            part.len() == len && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
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
    fn aionui_epoch_and_runtime_general_are_ephemeral() {
        let epoch = Path::new(
            "/Users/example/Library/Application Support/AionUi/aionui/codex-temp-17234567890",
        );
        let state =
            Path::new("/Users/example/Library/Application Support/dev.runboard.runboard/general");
        assert!(is_ephemeral_agent_cwd(epoch));
        assert!(is_ephemeral_agent_cwd(state));
    }

    #[test]
    fn nested_ork_runner_state_is_ephemeral() {
        let cwd = Path::new("/private/tmp/ork-direct-accept.E1hQlA/state/general");
        assert!(is_ephemeral_agent_cwd(cwd));
    }

    #[test]
    fn existing_git_project_with_temp_epoch_name_is_not_ephemeral() {
        let root = temp_dir("real-temp-project");
        let cwd = root.join("my-temp-1234567890");
        std::fs::create_dir_all(&cwd).expect("project");
        init_git_repo(&cwd);
        assert!(!is_ephemeral_agent_cwd(&cwd));
        assert_eq!(classify(Some(&cwd)).kind, ProjectKind::GitCommonDir);
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
