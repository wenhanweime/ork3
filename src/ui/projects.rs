use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::app::state::{
    AppState, ProjectFilter, ProjectRowHitArea, ProjectSessionActivation, ProjectTreeAction,
};
use crate::projects::{IndexedSessionSummary, ProjectKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectTreeRow {
    Project {
        project_key: String,
        display_name: String,
        session_count: usize,
        collapsed: bool,
        kind: ProjectKind,
    },
    Session(IndexedSessionSummary),
    LoadOlder {
        project_key: String,
    },
    Empty(String),
    Diagnostic(String),
    ScanStatus(String),
}

impl ProjectTreeRow {
    pub(crate) fn action(&self) -> Option<ProjectTreeAction> {
        match self {
            Self::Project { project_key, .. } => Some(ProjectTreeAction::ToggleProject {
                project_key: project_key.clone(),
            }),
            Self::Session(session) => {
                let activation = match (
                    session.live,
                    session.workspace_id.as_ref(),
                    session.pane_id.as_ref(),
                    session.runtime_generation,
                ) {
                    (true, Some(workspace_id), Some(pane_id), Some(runtime_generation)) => {
                        ProjectSessionActivation::Live {
                            session_key: session.stable_key.clone(),
                            workspace_id: workspace_id.clone(),
                            pane_id: pane_id.clone(),
                            runtime_generation,
                        }
                    }
                    _ => ProjectSessionActivation::History {
                        session_key: session.stable_key.clone(),
                    },
                };
                Some(ProjectTreeAction::Activate(activation))
            }
            Self::LoadOlder { project_key } => Some(ProjectTreeAction::LoadOlder {
                project_key: project_key.clone(),
            }),
            Self::Empty(_) | Self::Diagnostic(_) | Self::ScanStatus(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectSidebarGeometry {
    pub sidebar_tabs: [Rect; 3],
    pub filter_tabs: [Rect; 3],
    pub search: Rect,
    pub tree: Rect,
    pub row_hits: Vec<ProjectRowHitArea>,
    pub normalized_scroll: usize,
}

pub(crate) fn project_tree_rows(app: &AppState) -> Vec<ProjectTreeRow> {
    if let Some(category) = app.projects.snapshot.diagnostic_category.as_ref() {
        return vec![ProjectTreeRow::Diagnostic(format!(
            "Catalog unavailable · {category}"
        ))];
    }

    let query = app.projects.query.trim().to_lowercase();
    let mut rows = Vec::new();
    let grouping = app
        .sidebar_view
        .project_grouping()
        .unwrap_or(app.projects.grouping);
    let groups = match grouping {
        crate::app::state::ProjectGrouping::Directories => &app.projects.snapshot.projects,
        crate::app::state::ProjectGrouping::Topics => &app.projects.snapshot.topics,
    };
    for project in groups {
        if app.projects.filter == ProjectFilter::Unclassified
            && project.kind != ProjectKind::Unclassified
        {
            continue;
        }

        let project_matches = query.is_empty()
            || format!("{} {}", project.display_name, project.canonical_path)
                .to_lowercase()
                .contains(&query);
        let sessions = project
            .sessions
            .iter()
            .filter(|session| app.projects.filter != ProjectFilter::Live || session.live)
            .filter(|session| {
                project_matches
                    || format!(
                        "{} {} {}",
                        session.title,
                        session.backend,
                        session.cwd.as_deref().unwrap_or_default()
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .cloned()
            .collect::<Vec<_>>();
        if sessions.is_empty() {
            continue;
        }

        let collapsed =
            query.is_empty() && app.collapsed_project_keys.contains(&project.canonical_key);
        rows.push(ProjectTreeRow::Project {
            project_key: project.canonical_key.clone(),
            display_name: project.display_name.clone(),
            session_count: sessions.len(),
            collapsed,
            kind: project.kind,
        });
        if !collapsed {
            rows.extend(sessions.into_iter().map(ProjectTreeRow::Session));
            if project.next_cursor.is_some() && app.projects.filter == ProjectFilter::All {
                rows.push(ProjectTreeRow::LoadOlder {
                    project_key: project.canonical_key.clone(),
                });
            }
        }
    }

    if rows.is_empty() {
        // Only a genuinely empty Catalog gets the "not indexed yet" copy; an empty filter or
        // search result must not claim the Catalog is empty.
        let catalog_is_empty = groups.is_empty();
        let label = if !app.projects.query.trim().is_empty() {
            "No matching sessions"
        } else if catalog_is_empty {
            match grouping {
                crate::app::state::ProjectGrouping::Directories => "No indexed projects yet",
                crate::app::state::ProjectGrouping::Topics => "No clustered topics yet",
            }
        } else {
            "No sessions in this filter"
        };
        rows.push(ProjectTreeRow::Empty(label.to_string()));
        if catalog_is_empty {
            rows.extend(scan_status_rows(app));
        }
    }
    rows
}

/// Renders adapter scan state so an empty or partial Catalog is explained rather than
/// looking like a Catalog with nothing in it.
fn scan_status_rows(app: &AppState) -> Vec<ProjectTreeRow> {
    app.projects
        .snapshot
        .scan_status
        .iter()
        .map(|status| {
            let detail = status
                .diagnostic_category
                .as_deref()
                .map(|category| format!(" · {category}"))
                .unwrap_or_default();
            ProjectTreeRow::ScanStatus(format!("{}: {}{detail}", status.adapter, status.state))
        })
        .collect()
}

/// The number of projects still awaiting review. This is the only place the pending count is
/// derived, so the `unclassified` chip stays the single display of it.
pub(crate) fn unclassified_pending_count(app: &AppState) -> usize {
    let grouping = app
        .sidebar_view
        .project_grouping()
        .unwrap_or(app.projects.grouping);
    if grouping == crate::app::state::ProjectGrouping::Topics {
        return 0;
    }
    app.projects
        .snapshot
        .projects
        .iter()
        .filter(|project| project.kind == ProjectKind::Unclassified)
        .count()
}

pub(crate) struct UnclassifiedChip {
    pub label: String,
    pub disabled: bool,
}

/// The `unclassified` chip's rendered contract, kept out of the render loop so both the drawing
/// code and the tests read the same label and disabled state.
pub(crate) fn unclassified_chip(app: &AppState) -> UnclassifiedChip {
    let pending = unclassified_pending_count(app);
    UnclassifiedChip {
        label: if pending == 0 {
            "unclass".to_string()
        } else {
            format!("unclass {pending}")
        },
        disabled: pending == 0,
    }
}

pub(crate) fn project_sidebar_geometry(app: &AppState, area: Rect) -> ProjectSidebarGeometry {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.width == 0 || content.height == 0 {
        return ProjectSidebarGeometry {
            sidebar_tabs: [Rect::default(); 3],
            filter_tabs: [Rect::default(); 3],
            search: Rect::default(),
            tree: Rect::default(),
            row_hits: Vec::new(),
            normalized_scroll: 0,
        };
    }

    let first_width = content.width / 3;
    let second_width = content.width.saturating_sub(first_width) / 2;
    let sidebar_tabs = [
        Rect::new(content.x, content.y, first_width, 1),
        Rect::new(content.x + first_width, content.y, second_width, 1),
        Rect::new(
            content.x + first_width + second_width,
            content.y,
            content.width.saturating_sub(first_width + second_width),
            1,
        ),
    ];
    let controls_y = content.y.saturating_add(1);
    let first_width = content.width.min(5);
    let second_width = content.width.saturating_sub(first_width).min(6);
    let filter_tabs = [
        Rect::new(content.x, controls_y, first_width, 1),
        Rect::new(content.x + first_width, controls_y, second_width, 1),
        Rect::new(
            content.x + first_width + second_width,
            controls_y,
            content.width.saturating_sub(first_width + second_width),
            1,
        ),
    ];
    let search = Rect::new(
        content.x,
        content.y.saturating_add(2),
        content.width,
        u16::from(content.height >= 3),
    );
    let tree_y = content.y.saturating_add(3);
    let tree = Rect::new(
        content.x,
        tree_y,
        content.width,
        content.height.saturating_sub(3),
    );

    let rows = project_tree_rows(app);
    let viewport = usize::from(tree.height);
    let max_scroll = rows.len().saturating_sub(viewport);
    let normalized_scroll = app.projects.scroll.min(max_scroll);
    let row_hits = rows
        .iter()
        .skip(normalized_scroll)
        .take(viewport)
        .enumerate()
        .filter_map(|(visible_idx, row)| {
            row.action().map(|action| ProjectRowHitArea {
                rect: Rect::new(tree.x, tree.y + visible_idx as u16, tree.width, 1),
                action,
            })
        })
        .collect();

    ProjectSidebarGeometry {
        sidebar_tabs,
        filter_tabs,
        search,
        tree,
        row_hits,
        normalized_scroll,
    }
}

pub(crate) fn render_sidebar_tabs(app: &AppState, frame: &mut Frame, tabs: [Rect; 3]) {
    let labels = ["sessions", "projects", "clusters"];
    for (index, (label, rect)) in labels.into_iter().zip(tabs).enumerate() {
        if rect.width == 0 {
            continue;
        }
        let active = matches!(
            (index, app.sidebar_view),
            (0, crate::app::state::SidebarView::SpacesAgents)
                | (1, crate::app::state::SidebarView::Projects)
                | (2, crate::app::state::SidebarView::Clusters)
        );
        let style = if active {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        frame.render_widget(Paragraph::new(label).style(style).centered(), rect);
    }
}

pub(crate) fn render_projects_sidebar(app: &AppState, frame: &mut Frame, area: Rect) {
    let geometry = project_sidebar_geometry(app, area);
    if area.width > 0 {
        let separator_x = area.x + area.width.saturating_sub(1);
        let separator_style = Style::default().fg(if app.mode == crate::app::Mode::Navigate {
            app.palette.accent
        } else {
            app.palette.surface_dim
        });
        for y in area.y..area.y + area.height {
            frame.buffer_mut()[(separator_x, y)]
                .set_symbol("│")
                .set_style(separator_style);
        }
    }
    render_sidebar_tabs(app, frame, geometry.sidebar_tabs);

    let filters = [
        (ProjectFilter::All, "all".to_string(), false),
        (ProjectFilter::Live, "live".to_string(), false),
        {
            let chip = unclassified_chip(app);
            (ProjectFilter::Unclassified, chip.label, chip.disabled)
        },
    ];
    for ((filter, label, disabled), rect) in filters.into_iter().zip(geometry.filter_tabs) {
        if rect.width == 0 {
            continue;
        }
        let style = if disabled {
            Style::default().fg(app.palette.surface_dim)
        } else if app.projects.filter == filter {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface1)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        frame.render_widget(Paragraph::new(label).style(style).centered(), rect);
    }

    if geometry.search.height > 0 {
        let (prefix, query) = if app.projects.query.is_empty() {
            ("/ ", "search groups, sessions, agents")
        } else {
            ("/ ", app.projects.query.as_str())
        };
        let query_style = if app.projects.query.is_empty() {
            Style::default().fg(app.palette.surface_dim)
        } else {
            Style::default().fg(app.palette.text)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(app.palette.accent)),
                Span::styled(query, query_style),
            ])),
            geometry.search,
        );
    }

    let rows = project_tree_rows(app);
    for (visible_idx, row) in rows
        .iter()
        .skip(geometry.normalized_scroll)
        .take(usize::from(geometry.tree.height))
        .enumerate()
    {
        let absolute_idx = geometry.normalized_scroll + visible_idx;
        let rect = Rect::new(
            geometry.tree.x,
            geometry.tree.y + visible_idx as u16,
            geometry.tree.width,
            1,
        );
        let selected =
            app.mode == crate::app::Mode::Navigate && app.projects.selected_row == absolute_idx;
        if selected {
            frame.render_widget(
                Paragraph::new("").style(Style::default().bg(app.palette.surface0)),
                rect,
            );
        }
        let line = match row {
            ProjectTreeRow::Project {
                display_name,
                session_count,
                collapsed,
                kind,
                ..
            } => {
                let marker = if *collapsed { "▸" } else { "▾" };
                let badge = if *kind == ProjectKind::Unclassified {
                    " ?"
                } else {
                    ""
                };
                Line::from(vec![
                    Span::styled(
                        format!("{marker} "),
                        Style::default().fg(app.palette.accent),
                    ),
                    Span::styled(
                        display_name.clone(),
                        Style::default()
                            .fg(app.palette.text)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {session_count}{badge}"),
                        Style::default().fg(app.palette.overlay0),
                    ),
                ])
            }
            ProjectTreeRow::Session(session) => Line::from(vec![
                Span::styled(
                    if session.live { "  ● " } else { "  ○ " },
                    Style::default().fg(if session.live {
                        app.palette.green
                    } else {
                        app.palette.overlay0
                    }),
                ),
                Span::styled(
                    session.title.clone(),
                    Style::default().fg(app.palette.subtext0),
                ),
                Span::styled(
                    format!(" · {}", session.backend),
                    Style::default().fg(app.palette.overlay0),
                ),
            ]),
            ProjectTreeRow::LoadOlder { .. } => Line::from(Span::styled(
                "  Load older…",
                Style::default().fg(app.palette.accent),
            )),
            ProjectTreeRow::Empty(message) | ProjectTreeRow::Diagnostic(message) => {
                Line::from(Span::styled(
                    format!(" {message}"),
                    Style::default().fg(app.palette.overlay0),
                ))
            }
            ProjectTreeRow::ScanStatus(message) => Line::from(Span::styled(
                format!("   {message}"),
                Style::default().fg(app.palette.surface_dim),
            )),
        };
        frame.render_widget(Paragraph::new(line), rect);
    }
}

pub(crate) fn render_project_history(app: &AppState, frame: &mut Frame, area: Rect) {
    let session = app
        .projects
        .history_session_key
        .as_deref()
        .and_then(|session_key| {
            app.projects
                .snapshot
                .projects
                .iter()
                .chain(app.projects.snapshot.topics.iter())
                .flat_map(|project| project.sessions.iter())
                .find(|session| session.stable_key == session_key)
        });
    let (title, body) = session.map_or_else(
        || (
            "Historical session".to_string(),
            "This session is no longer available in the current snapshot.".to_string(),
        ),
        |session| {
            let cwd = session.cwd.as_deref().unwrap_or("unknown cwd");
            (
                session.title.clone(),
                format!(
                    "Read-only history\n\nAgent: {}\nWorking directory: {}\nFirst activity: {}\nLast activity: {}\n\nNo process was created and no resume command was run. Press Esc to return.",
                    session.backend,
                    cwd,
                    session.first_activity_at,
                    session.last_activity_at
                ),
            )
        },
    );
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.palette.surface_dim));
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .style(Style::default().fg(app.palette.subtext0))
            .wrap(Wrap { trim: false })
            .scroll((app.projects.history_scroll.min(u16::MAX as usize) as u16, 0)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::{
        AdapterScanStatus, IndexedSessionSummary, ProjectSummary, ProjectsSnapshot, SessionRefKind,
    };

    fn snapshot() -> ProjectsSnapshot {
        ProjectsSnapshot {
            projects_schema_version: 1,
            revision: 1,
            projects: vec![ProjectSummary {
                canonical_key: "p1".into(),
                kind: ProjectKind::Cwd,
                display_name: "ait".into(),
                canonical_path: "/tmp/ait".into(),
                sessions: vec![IndexedSessionSummary {
                    stable_key: "s1".into(),
                    backend: "codex".into(),
                    ref_kind: SessionRefKind::Id,
                    title: "Dense Tree".into(),
                    cwd: Some("/tmp/ait".into()),
                    first_activity_at: 1,
                    last_activity_at: 2,
                    live: true,
                    workspace_id: Some("workspace-1".into()),
                    pane_id: Some("workspace-1:p1".into()),
                    runtime_generation: Some(4),
                }],
                next_cursor: None,
            }],
            topics: Vec::new(),
            scan_status: Vec::new(),
            diagnostic_category: None,
        }
    }

    #[test]
    fn filters_are_one_row_and_share_geometry_with_hit_testing() {
        let mut state = AppState::test_new();
        state.projects.snapshot = snapshot();
        let geometry = project_sidebar_geometry(&state, Rect::new(0, 0, 30, 12));
        assert!(geometry.sidebar_tabs.iter().all(|rect| rect.height == 1));
        assert_eq!(geometry.sidebar_tabs[0].y, geometry.filter_tabs[0].y - 1);
        assert_eq!(geometry.sidebar_tabs[2].right(), 29);
        assert!(geometry.filter_tabs.iter().all(|rect| rect.height == 1));
        assert_eq!(geometry.row_hits.len(), 2);
        assert_eq!(geometry.row_hits[0].rect.height, 1);
    }

    #[test]
    fn live_leaf_has_one_typed_focus_activation() {
        let mut state = AppState::test_new();
        state.projects.snapshot = snapshot();
        let rows = project_tree_rows(&state);
        assert!(matches!(
            rows[1].action(),
            Some(ProjectTreeAction::Activate(ProjectSessionActivation::Live {
                ref workspace_id,
                ref pane_id,
                runtime_generation: 4,
                ..
            })) if workspace_id == "workspace-1" && pane_id == "workspace-1:p1"
        ));
    }

    #[test]
    fn incomplete_live_mapping_is_read_only_history() {
        let mut state = AppState::test_new();
        let mut snapshot = snapshot();
        snapshot.projects[0].sessions[0].runtime_generation = None;
        state.projects.snapshot = snapshot;
        assert!(matches!(
            project_tree_rows(&state)[1].action(),
            Some(ProjectTreeAction::Activate(
                ProjectSessionActivation::History { .. }
            ))
        ));
    }

    #[test]
    fn search_expands_without_mutating_persisted_fold() {
        let mut state = AppState::test_new();
        state.projects.snapshot = snapshot();
        state.collapsed_project_keys.insert("p1".into());
        state.projects.query = "dense".into();
        let rows = project_tree_rows(&state);
        assert_eq!(rows.len(), 2);
        assert!(state.collapsed_project_keys.contains("p1"));
    }

    #[test]
    fn empty_catalog_shows_indexed_copy_and_scan_status_without_sample_data() {
        let mut state = AppState::test_new();
        state.projects.snapshot = ProjectsSnapshot {
            projects_schema_version: 1,
            revision: 3,
            projects: Vec::new(),
            topics: Vec::new(),
            scan_status: vec![
                AdapterScanStatus {
                    adapter: "codex".into(),
                    state: "ready".into(),
                    diagnostic_category: None,
                },
                AdapterScanStatus {
                    adapter: "claude".into(),
                    state: "failed".into(),
                    diagnostic_category: Some("root_unavailable".into()),
                },
            ],
            diagnostic_category: None,
        };

        let rows = project_tree_rows(&state);

        assert_eq!(
            rows[0],
            ProjectTreeRow::Empty("No indexed projects yet".to_string())
        );
        assert_eq!(
            rows[1],
            ProjectTreeRow::ScanStatus("codex: ready".to_string())
        );
        assert_eq!(
            rows[2],
            ProjectTreeRow::ScanStatus("claude: failed · root_unavailable".to_string())
        );
        assert_eq!(rows.len(), 3);
        // No fabricated project or session rows stand in for the empty Catalog.
        assert!(!rows.iter().any(|row| matches!(
            row,
            ProjectTreeRow::Project { .. } | ProjectTreeRow::Session(_)
        )));
    }

    #[test]
    fn locking_last_unclassified_project_zeroes_chip_count_and_leaves_no_pseudo_node() {
        let mut state = AppState::test_new();
        let mut snapshot = snapshot();
        snapshot.projects[0].kind = ProjectKind::Unclassified;
        state.projects.snapshot = snapshot;

        assert_eq!(unclassified_pending_count(&state), 1);
        let chip = unclassified_chip(&state);
        assert_eq!(chip.label, "unclass 1");
        assert!(!chip.disabled);
        state.projects.filter = ProjectFilter::Unclassified;
        assert_eq!(project_tree_rows(&state).len(), 2);

        // Moving the last pending session to a real Project and locking it clears the queue.
        state.projects.snapshot.projects[0].kind = ProjectKind::Cwd;

        assert_eq!(unclassified_pending_count(&state), 0);
        let chip = unclassified_chip(&state);
        assert_eq!(chip.label, "unclass");
        assert!(
            chip.disabled,
            "chip must be disabled once nothing is pending"
        );
        let rows = project_tree_rows(&state);
        assert_eq!(
            rows[0],
            ProjectTreeRow::Empty("No sessions in this filter".to_string())
        );
        assert!(!rows.iter().any(|row| matches!(
            row,
            ProjectTreeRow::Project { .. } | ProjectTreeRow::Session(_)
        )));
    }

    #[test]
    fn directory_and_topic_modes_render_independent_parent_groups() {
        let mut state = AppState::test_new();
        let mut snapshot = snapshot();
        let mut topic = snapshot.projects[0].clone();
        topic.canonical_key = "topic-1".into();
        topic.kind = ProjectKind::Semantic;
        topic.display_name = "ORK3 architecture".into();
        topic.canonical_path = "semantic-topic-key".into();
        snapshot.topics.push(topic);
        state.projects.snapshot = snapshot;

        let directory_rows = project_tree_rows(&state);
        assert!(matches!(
            &directory_rows[0],
            ProjectTreeRow::Project { display_name, kind: ProjectKind::Cwd, .. }
                if display_name == "ait"
        ));

        state.sidebar_view = crate::app::state::SidebarView::Clusters;
        let topic_rows = project_tree_rows(&state);
        assert!(matches!(
            &topic_rows[0],
            ProjectTreeRow::Project { display_name, kind: ProjectKind::Semantic, .. }
                if display_name == "ORK3 architecture"
        ));
        assert!(!topic_rows.iter().any(|row| matches!(
            row,
            ProjectTreeRow::Project { display_name, .. } if display_name == "ait"
        )));
    }
}
