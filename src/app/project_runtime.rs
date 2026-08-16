use super::App;

impl App {
    pub(crate) fn sync_all_project_runtime_mappings(&mut self) {
        if !self.project_service.is_available() {
            return;
        }
        let pane_ids = self
            .state
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.keys().copied())
            })
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            self.sync_project_runtime_for_pane(pane_id, false);
        }
    }

    pub(crate) fn sync_project_runtime_for_pane(
        &mut self,
        pane_id: crate::layout::PaneId,
        force_metadata_refresh: bool,
    ) {
        if !self.project_service.is_available() {
            return;
        }
        let report = self.find_pane(pane_id).and_then(|(ws_idx, pane)| {
            let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
            let session = terminal.persisted_agent_session.clone()?;
            Some((
                session,
                terminal.cwd.clone(),
                self.public_workspace_id(ws_idx),
                self.public_pane_id(ws_idx, pane_id)?,
            ))
        });
        let Some((session, cwd, workspace_id, public_pane_id)) = report else {
            self.clear_project_runtime_for_pane(pane_id);
            return;
        };
        let Some(identity) = crate::projects::runtime::identity_from_report(
            &self.project_roots,
            &session.agent,
            &session.session_ref,
        ) else {
            self.clear_project_runtime_for_pane(pane_id);
            return;
        };

        let existing = self.project_runtime_leases.get(&pane_id).cloned();
        let same_mapping = existing.as_ref().is_some_and(|lease| {
            lease.session_key == identity.stable_key
                && lease.workspace_id == workspace_id
                && lease.pane_id == public_pane_id
        });
        if same_mapping && !force_metadata_refresh {
            return;
        }

        let generation = if let Some(existing) = existing {
            if same_mapping {
                existing.generation
            } else {
                self.clear_project_runtime_for_pane(pane_id);
                self.take_next_project_runtime_generation()
            }
        } else {
            self.take_next_project_runtime_generation()
        };
        let observed_at = crate::projects::runtime::unix_time_ms().max(
            self.project_runtime_leases
                .get(&pane_id)
                .map(|lease| lease.observed_at.saturating_add(1))
                .unwrap_or_default(),
        );
        let candidate = crate::projects::runtime::candidate_from_report(
            crate::projects::runtime::RuntimeCandidateInput {
                identity: identity.clone(),
                cwd,
                workspace_id: &workspace_id,
                pane_id: &public_pane_id,
                generation,
                observed_at,
            },
        );
        match self.project_service.upsert_candidate(candidate) {
            Ok(_) => {
                self.project_runtime_leases.insert(
                    pane_id,
                    crate::projects::runtime::RuntimeLease {
                        session_key: identity.stable_key,
                        workspace_id,
                        pane_id: public_pane_id,
                        generation,
                        observed_at,
                    },
                );
            }
            Err(error) => tracing::warn!(
                adapter = session.agent,
                category = error.code,
                "Project runtime report was not committed"
            ),
        }
    }

    pub(crate) fn clear_project_runtime_for_pane(&mut self, pane_id: crate::layout::PaneId) {
        let Some(lease) = self.project_runtime_leases.remove(&pane_id) else {
            return;
        };
        if let Err(error) = self
            .project_service
            .clear_runtime_mapping(lease.session_key, lease.generation)
        {
            tracing::warn!(
                category = error.code,
                "Project runtime lease could not be cleared"
            );
        }
    }

    #[cfg(unix)]
    pub(crate) fn activate_project_service_after_handoff(&mut self) {
        if self.project_service.is_available() {
            return;
        }
        let mut service = crate::projects::ProjectService::open(
            &crate::session::data_dir().join("projects/catalog.sqlite3"),
            self.event_hub.clone(),
        );
        service.start_background_scan(self.project_roots.roots());
        self.project_service = service;
        self.project_runtime_leases.clear();
        self.next_project_runtime_generation = 1;
        self.sync_all_project_runtime_mappings();
        self.state.projects.snapshot = self.project_service.snapshot();
    }

    fn take_next_project_runtime_generation(&mut self) -> u64 {
        let generation = self.next_project_runtime_generation;
        self.next_project_runtime_generation =
            self.next_project_runtime_generation.saturating_add(1);
        generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AppEvent;

    fn app_with_project_runtime() -> (App, crate::layout::PaneId) {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.project_service = crate::projects::ProjectService::in_memory(event_hub);
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("runtime")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        (app, pane_id)
    }

    fn report(app: &mut App, pane_id: crate::layout::PaneId, id: &str, seq: u64, start: &str) {
        app.handle_internal_event(AppEvent::AgentSessionReported {
            pane_id,
            source: "herdr:codex".to_string(),
            agent_label: "codex".to_string(),
            seq: Some(seq),
            session_ref: crate::agent_resume::AgentSessionRef::id(id),
            session_start_source: Some(start.to_string()),
        });
    }

    #[test]
    fn accepted_session_report_upserts_live_mapping_and_pane_exit_clears_it() {
        let (mut app, pane_id) = app_with_project_runtime();
        report(&mut app, pane_id, "live-one", 1, "startup");
        let snapshot = app.project_service.snapshot();
        let session = snapshot.projects[0].sessions.first().expect("live session");
        assert!(session.live);
        assert_eq!(
            session.workspace_id.as_deref(),
            Some(app.state.workspaces[0].id.as_str())
        );
        assert_eq!(session.runtime_generation, Some(1));

        app.handle_internal_event(AppEvent::PaneDied { pane_id });
        let snapshot = app.project_service.snapshot();
        assert!(!snapshot.projects[0].sessions[0].live);
        assert!(app.project_runtime_leases.is_empty());
    }

    #[test]
    fn replacement_session_clears_old_generation_and_maps_new_native_identity() {
        let (mut app, pane_id) = app_with_project_runtime();
        report(&mut app, pane_id, "session-a", 1, "startup");
        report(&mut app, pane_id, "session-b", 2, "clear");
        let snapshot = app.project_service.snapshot();
        let sessions = snapshot
            .projects
            .iter()
            .flat_map(|project| &project.sessions)
            .collect::<Vec<_>>();
        assert_eq!(sessions.len(), 2);
        let old = sessions
            .iter()
            .find(|session| session.title.contains("session-a") || !session.live)
            .expect("old session");
        assert!(!old.live);
        let live = sessions
            .iter()
            .find(|session| session.live)
            .expect("new live");
        assert_eq!(live.runtime_generation, Some(2));
    }

    #[test]
    fn cwd_report_refreshes_runtime_metadata_without_rotating_generation() {
        let (mut app, pane_id) = app_with_project_runtime();
        report(&mut app, pane_id, "cwd-session", 1, "startup");
        let cwd =
            std::env::temp_dir().join(format!("herdr-project-runtime-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("runtime cwd");
        app.handle_internal_event(AppEvent::TerminalCwdReported {
            pane_id,
            cwd: cwd.clone(),
        });
        let snapshot = app.project_service.snapshot();
        let session = snapshot
            .projects
            .iter()
            .flat_map(|project| &project.sessions)
            .find(|session| session.live)
            .expect("live session");
        assert_eq!(session.runtime_generation, Some(1));
        assert_eq!(session.cwd.as_deref(), cwd.to_str());
        let _ = std::fs::remove_dir_all(cwd);
    }
}
