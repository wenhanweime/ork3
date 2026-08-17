//! Input handling — translates crossterm key/mouse events into state mutations.

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tracing::warn;

use crate::app::PaneClickState;
use crate::input::TerminalKey;
#[cfg(test)]
use ratatui::layout::Direction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollbarClickTarget {
    Thumb { grab_row_offset: u16 },
    Track { offset_from_bottom: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

const WORKSPACE_DRAG_THRESHOLD: u16 = 1;
const TAB_DRAG_THRESHOLD: u16 = 1;

fn modified_url_click_modifier() -> KeyModifiers {
    KeyModifiers::CONTROL
}

#[cfg(test)]
#[test]
fn modified_url_click_modifier_matches_terminal_mouse_reporting() {
    assert_eq!(modified_url_click_modifier(), KeyModifiers::CONTROL);
}

mod copy_mode;
mod modal;
mod mouse;
mod navigate;
mod overlays;
mod selection;
mod settings;
mod sidebar;
mod terminal;

pub(crate) use self::{
    modal::{
        handle_global_menu_key, handle_keybind_help_key, handle_navigator_key,
        insert_navigator_search_text, insert_rename_input_text,
    },
    navigate::{
        terminal_direct_indexed_navigation_action, terminal_direct_non_indexed_navigation_action,
    },
    settings::open_settings_at,
};
use self::{
    modal::{
        modal_action_from_key, ModalAction, ONBOARDING_WELCOME_ACTIONS, RELEASE_NOTES_ACTIONS,
    },
    mouse::MouseAction,
    settings::SettingsAction,
};
use super::state::{AppState, Mode};
use super::App;

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

impl App {
    pub(super) async fn handle_key(&mut self, key: TerminalKey) {
        if self.state.popup_pane.is_some() {
            self.handle_terminal_key(key).await;
            return;
        }
        let key_event = key.as_key_event();
        if modal_paste_target_active(&self.state) && is_modal_paste_shortcut(&key_event) {
            if let Some(text) = crate::platform::read_clipboard_text() {
                self.paste_into_active_text_input(&text);
            }
            return;
        }

        match self.state.mode {
            Mode::Terminal => self.handle_terminal_key(key).await,
            Mode::Prefix => self.handle_prefix_key(key),
            Mode::Navigate => self.handle_navigate_key(key),
            Mode::Copy => self.handle_copy_mode_key(key),
            _ => match self.state.mode {
                Mode::Onboarding => self.handle_onboarding_key(key_event),
                Mode::ReleaseNotes => self.handle_release_notes_key(key_event),
                Mode::ProductAnnouncement => self.handle_product_announcement_key(key_event),
                Mode::Prefix | Mode::Navigate | Mode::Copy => unreachable!(),
                Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane => {
                    self.handle_rename_key_via_api(key_event)
                }
                Mode::NewLinkedWorktree => self.handle_worktree_create_key(key_event),
                Mode::OpenExistingWorktree => self.handle_worktree_open_key(key_event),
                Mode::ConfirmRemoveWorktree => self.handle_worktree_remove_key(key_event),
                Mode::Resize => self.handle_resize_key_via_api(key),
                Mode::ConfirmClose => self.handle_confirm_close_key_via_api(key_event),
                Mode::ContextMenu => {
                    self.handle_context_menu_key_via_api(key_event);
                }
                Mode::Settings => self.handle_settings_key(key_event),
                Mode::GlobalMenu => handle_global_menu_key(&mut self.state, key_event),
                Mode::KeybindHelp => handle_keybind_help_key(&mut self.state, key_event),
                Mode::Navigator => {
                    handle_navigator_key(&mut self.state, &self.terminal_runtimes, key_event)
                }
                Mode::ProjectHistory => self.handle_project_history_key(key_event),
                Mode::Terminal => unreachable!(),
            },
        }
    }

    pub(super) async fn handle_paste(&mut self, text: String) {
        if self.state.popup_pane.is_some() {
            if let Some(runtime) = self.popup_runtime() {
                let _ = runtime.send_paste(text).await;
            } else {
                self.close_popup_pane();
            }
            return;
        }
        if self.state.mode != Mode::Terminal {
            self.paste_into_active_text_input(&text);
            return;
        }

        if let Some(ws_idx) = self.state.active {
            if let Some(rt) = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
            {
                let _ = rt.send_paste(text).await;
            }
        }
    }

    pub(crate) fn paste_into_active_text_input(&mut self, text: &str) -> bool {
        match self.state.mode {
            Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane => {
                insert_rename_input_text(&mut self.state, text);
                true
            }
            Mode::NewLinkedWorktree => {
                self.insert_worktree_create_text(text);
                true
            }
            Mode::OpenExistingWorktree => {
                if !self
                    .state
                    .worktree_open
                    .as_ref()
                    .is_some_and(|open| open.search_focused)
                {
                    return false;
                }
                self.insert_worktree_open_search_text(text);
                true
            }
            Mode::Navigator => {
                if !self.state.navigator.search_focused {
                    return false;
                }
                insert_navigator_search_text(&mut self.state, &self.terminal_runtimes, text);
                true
            }
            Mode::Navigate
                if self.state.sidebar_view.is_project_browser()
                    && self.state.projects.search_focused =>
            {
                self.state
                    .projects
                    .query
                    .extend(text.chars().filter(|character| !character.is_control()));
                self.normalize_project_selection();
                true
            }
            Mode::Copy => {
                let Some(prompt) = self
                    .state
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search.prompt.as_mut())
                else {
                    return false;
                };
                prompt
                    .query
                    .extend(text.chars().filter(|ch| !ch.is_control()));
                true
            }
            _ => false,
        }
    }

    pub(super) fn handle_project_history_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state.projects.history_session_key = None;
                self.state.projects.history_scroll = 0;
                self.restore_focus_after_project_history();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.projects.history_scroll =
                    self.state.projects.history_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.projects.history_scroll =
                    self.state.projects.history_scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.state.projects.history_scroll =
                    self.state.projects.history_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.state.projects.history_scroll =
                    self.state.projects.history_scroll.saturating_add(10);
            }
            _ => {}
        }
    }

    pub(super) fn handle_projects_navigate_key(&mut self, key: KeyEvent) -> bool {
        if !self.state.sidebar_view.is_project_browser() {
            return false;
        }

        if self.state.projects.search_focused {
            match key.code {
                KeyCode::Esc => {
                    self.state.projects.query.clear();
                    self.state.projects.search_focused = false;
                    if let Some((selected, scroll)) =
                        self.state.projects.search_restore_selection.take()
                    {
                        self.state.projects.selected_row = selected;
                        self.state.projects.scroll = scroll;
                    }
                }
                KeyCode::Enter => self.state.projects.search_focused = false,
                KeyCode::Backspace => {
                    self.state.projects.query.pop();
                    if self.state.projects.query.is_empty() {
                        if let Some((selected, scroll)) =
                            self.state.projects.search_restore_selection
                        {
                            self.state.projects.selected_row = selected;
                            self.state.projects.scroll = scroll;
                        }
                    } else {
                        self.normalize_project_selection();
                    }
                }
                KeyCode::Char(character) if key.modifiers.is_empty() && !character.is_control() => {
                    self.state.projects.query.push(character);
                    self.normalize_project_selection();
                }
                _ => {}
            }
            return true;
        }

        match key.code {
            KeyCode::Left if key.modifiers.is_empty() => {
                if self.state.sidebar_view == crate::app::state::SidebarView::Clusters {
                    self.state.sidebar_view = crate::app::state::SidebarView::Projects;
                } else {
                    self.state.sidebar_view = crate::app::state::SidebarView::SpacesAgents;
                    self.state.projects.search_focused = false;
                }
                self.state.projects.grouping = crate::app::state::ProjectGrouping::Directories;
                self.state.projects.selected_row = 0;
                self.state.projects.scroll = 0;
                true
            }
            KeyCode::Right if key.modifiers.is_empty() => {
                self.state.sidebar_view = crate::app::state::SidebarView::Clusters;
                self.state.projects.grouping = crate::app::state::ProjectGrouping::Topics;
                if self.state.projects.filter == crate::app::state::ProjectFilter::Unclassified {
                    self.state.projects.filter = crate::app::state::ProjectFilter::All;
                }
                self.state.projects.selected_row = 0;
                self.state.projects.scroll = 0;
                true
            }
            KeyCode::Char('/') if key.modifiers.is_empty() => {
                self.state.projects.search_restore_selection =
                    Some((self.state.projects.selected_row, self.state.projects.scroll));
                self.state.projects.search_focused = true;
                true
            }
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
                self.state.projects.selected_row =
                    self.state.projects.selected_row.saturating_sub(1);
                self.ensure_project_selection_visible();
                true
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                let last = crate::ui::project_tree_rows(&self.state)
                    .len()
                    .saturating_sub(1);
                self.state.projects.selected_row =
                    self.state.projects.selected_row.saturating_add(1).min(last);
                self.ensure_project_selection_visible();
                true
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                let action = crate::ui::project_tree_rows(&self.state)
                    .get(self.state.projects.selected_row)
                    .and_then(crate::ui::ProjectTreeRow::action);
                if let Some(action) = action {
                    self.execute_project_tree_action(action);
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn normalize_project_selection(&mut self) {
        let row_count = crate::ui::project_tree_rows(&self.state).len();
        self.state.projects.selected_row = self
            .state
            .projects
            .selected_row
            .min(row_count.saturating_sub(1));
        self.ensure_project_selection_visible();
    }

    fn ensure_project_selection_visible(&mut self) {
        let viewport = usize::from(self.state.view.project_tree_rect.height).max(1);
        if self.state.projects.selected_row < self.state.projects.scroll {
            self.state.projects.scroll = self.state.projects.selected_row;
        } else if self.state.projects.selected_row
            >= self.state.projects.scroll.saturating_add(viewport)
        {
            self.state.projects.scroll = self
                .state
                .projects
                .selected_row
                .saturating_add(1)
                .saturating_sub(viewport);
        }
    }

    pub(super) fn execute_project_tree_action(
        &mut self,
        action: crate::app::state::ProjectTreeAction,
    ) {
        match action {
            crate::app::state::ProjectTreeAction::ToggleProject { project_key } => {
                if !self.state.collapsed_project_keys.remove(&project_key) {
                    self.state.collapsed_project_keys.insert(project_key);
                }
                self.state.mark_session_dirty();
                self.normalize_project_selection();
            }
            crate::app::state::ProjectTreeAction::Activate(activation) => {
                self.activate_project_session(activation)
            }
            crate::app::state::ProjectTreeAction::LoadOlder { project_key } => {
                self.load_older_project_sessions(&project_key)
            }
        }
    }

    fn activate_project_session(
        &mut self,
        activation: crate::app::state::ProjectSessionActivation,
    ) {
        match activation {
            crate::app::state::ProjectSessionActivation::Live {
                session_key,
                workspace_id,
                pane_id,
                runtime_generation,
            } => {
                let mapping_is_current = self
                    .state
                    .projects
                    .snapshot
                    .projects
                    .iter()
                    .chain(self.state.projects.snapshot.topics.iter())
                    .flat_map(|project| project.sessions.iter())
                    .find(|session| session.stable_key == session_key)
                    .is_some_and(|session| {
                        session.live
                            && session.workspace_id.as_deref() == Some(workspace_id.as_str())
                            && session.pane_id.as_deref() == Some(pane_id.as_str())
                            && session.runtime_generation == Some(runtime_generation)
                    });
                let target = mapping_is_current
                    .then(|| self.parse_pane_id(&pane_id))
                    .flatten()
                    .filter(|(ws_idx, _)| self.public_workspace_id(*ws_idx) == workspace_id);
                if let Some((ws_idx, pane_id)) = target {
                    self.state.projects.history_session_key = None;
                    self.focus_pane_internal_via_api(ws_idx, pane_id);
                    self.state.mode = Mode::Terminal;
                } else {
                    self.open_project_history(session_key);
                }
            }
            crate::app::state::ProjectSessionActivation::History { session_key } => {
                self.open_project_history(session_key)
            }
        }
    }

    fn open_project_history(&mut self, session_key: String) {
        self.state.previous_pane_focus = self.state.current_pane_focus_target();
        self.state.projects.history_return_tab = self.state.active.and_then(|ws_idx| {
            let ws = self.state.workspaces.get(ws_idx)?;
            let tab = ws.tabs.get(ws.active_tab)?;
            Some((ws.id.clone(), tab.root_pane))
        });
        self.state.projects.history_session_key = Some(session_key);
        self.state.projects.history_scroll = 0;
        self.state.mode = Mode::ProjectHistory;
    }

    /// Restores focus when leaving read-only history, walking the PRD fallback chain:
    /// original pane, then the original tab's current pane, then the first live pane,
    /// and finally Navigate mode when no live pane remains.
    fn restore_focus_after_project_history(&mut self) {
        let previous = self.state.previous_pane_focus.take();
        // The return tab is only meaningful while history is open, so it is consumed here on
        // every path rather than left behind for the next history session to trip over.
        let return_tab = self.state.projects.history_return_tab.take();
        if let Some(target) = previous {
            if let Some((ws_idx, _tab_idx)) = self.state.pane_focus_target_indices(&target) {
                self.state.mode = Mode::Terminal;
                self.focus_pane_internal_via_api(ws_idx, target.pane_id);
                return;
            }
        }

        // The original pane exited. Fall back to whatever that tab focuses now.
        if let Some((workspace_id, root_pane)) = return_tab {
            let tab_focus = self
                .state
                .workspaces
                .iter()
                .position(|ws| ws.id == workspace_id)
                .and_then(|ws_idx| {
                    let ws = self.state.workspaces.get(ws_idx)?;
                    let tab = ws.tabs.iter().find(|tab| tab.root_pane == root_pane)?;
                    Some((ws_idx, tab.layout.focused()))
                });
            if let Some((ws_idx, pane_id)) = tab_focus {
                self.state.mode = Mode::Terminal;
                self.focus_pane_internal_via_api(ws_idx, pane_id);
                return;
            }
        }

        // The original tab is gone too. Fall back to the first live pane anywhere.
        let first_live = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| {
                let tab = ws.tabs.first()?;
                tab.layout
                    .pane_ids()
                    .first()
                    .map(|pane_id| (ws_idx, *pane_id))
            });
        if let Some((ws_idx, pane_id)) = first_live {
            self.state.mode = Mode::Terminal;
            self.focus_pane_internal_via_api(ws_idx, pane_id);
            return;
        }

        // Nothing live is left to focus.
        self.state.mode = Mode::Navigate;
    }

    fn load_older_project_sessions(&mut self, project_key: &str) {
        let grouping = self
            .state
            .sidebar_view
            .project_grouping()
            .unwrap_or(self.state.projects.grouping);
        let groups = match grouping {
            crate::app::state::ProjectGrouping::Directories => {
                &self.state.projects.snapshot.projects
            }
            crate::app::state::ProjectGrouping::Topics => &self.state.projects.snapshot.topics,
        };
        let cursor = groups
            .iter()
            .find(|project| project.canonical_key == project_key)
            .and_then(|project| project.next_cursor.clone());
        let Some(cursor) = cursor else {
            return;
        };
        let Ok(page) =
            self.project_service
                .sessions_page(project_key.to_string(), Some(cursor), 50)
        else {
            return;
        };
        let groups = match grouping {
            crate::app::state::ProjectGrouping::Directories => {
                &mut self.state.projects.snapshot.projects
            }
            crate::app::state::ProjectGrouping::Topics => &mut self.state.projects.snapshot.topics,
        };
        let Some(project) = groups
            .iter_mut()
            .find(|project| project.canonical_key == project_key)
        else {
            return;
        };
        let mut seen = project
            .sessions
            .iter()
            .map(|session| session.stable_key.clone())
            .collect::<std::collections::HashSet<_>>();
        project.sessions.extend(
            page.sessions
                .into_iter()
                .filter(|session| seen.insert(session.stable_key.clone())),
        );
        project.sessions.sort_by(|left, right| {
            right
                .last_activity_at
                .cmp(&left.last_activity_at)
                .then_with(|| left.stable_key.cmp(&right.stable_key))
        });
        project.next_cursor = page.next_cursor;
        self.state.projects.snapshot.revision =
            self.state.projects.snapshot.revision.max(page.revision);
        self.normalize_project_selection();
    }

    pub(crate) fn handle_onboarding_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => self.open_settings_from_onboarding(),
            _ => {
                if let Some(ModalAction::Continue) =
                    modal_action_from_key(&key, ONBOARDING_WELCOME_ACTIONS)
                {
                    self.open_settings_from_onboarding();
                }
            }
        }
    }

    pub(crate) fn handle_release_notes_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_release_notes(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_release_notes(1),
            KeyCode::PageUp => self.scroll_release_notes(-8),
            KeyCode::PageDown => self.scroll_release_notes(8),
            KeyCode::Home => {
                if let Some(notes) = &mut self.state.release_notes {
                    notes.scroll = 0;
                }
            }
            KeyCode::End => {
                let max_scroll = self.state.release_notes_max_scroll();
                if let Some(notes) = &mut self.state.release_notes {
                    notes.scroll = max_scroll;
                }
            }
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, RELEASE_NOTES_ACTIONS)
                {
                    self.dismiss_release_notes();
                }
            }
        }
    }

    pub(crate) fn handle_product_announcement_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_product_announcement(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_product_announcement(1),
            KeyCode::PageUp => self.scroll_product_announcement(-8),
            KeyCode::PageDown => self.scroll_product_announcement(8),
            KeyCode::Home => {
                if let Some(announcement) = &mut self.state.product_announcement {
                    announcement.scroll = 0;
                }
            }
            KeyCode::End => {
                let max_scroll = self.state.product_announcement_max_scroll();
                if let Some(announcement) = &mut self.state.product_announcement {
                    announcement.scroll = max_scroll;
                }
            }
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, RELEASE_NOTES_ACTIONS)
                {
                    self.dismiss_product_announcement();
                }
            }
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.state.popup_pane.is_some() {
            self.handle_popup_mouse(mouse);
            return;
        }
        if self.handle_overlay_mouse(mouse) {
            return;
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.state.on_sidebar_divider(mouse.column, mouse.row)
        {
            let now = std::time::Instant::now();
            let is_double_click = self
                .last_sidebar_divider_click
                .is_some_and(|last| now.duration_since(last) <= super::SIDEBAR_DOUBLE_CLICK_WINDOW);
            self.last_sidebar_divider_click = Some(now);

            if is_double_click {
                self.state.sidebar_width = self.state.default_sidebar_width;
                self.state.sidebar_width_source =
                    crate::app::state::SidebarWidthSource::ConfigDefault;
                self.state.sidebar_width_auto = false;
                self.state.mark_session_dirty();
                self.state.drag = None;
                return;
            }
        }

        if self.handle_modified_url_click(mouse) {
            return;
        }

        let handled_pane_double_click = self.handle_pane_double_click(mouse);

        let previous_agent_panel_sort = self.state.agent_panel_sort;
        let previous_settings_section = self.state.settings.section;
        if !handled_pane_double_click {
            let right_button = matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Right)
                    | MouseEventKind::Up(MouseButton::Right)
                    | MouseEventKind::Drag(MouseButton::Right)
            );
            let intentional_pane_press = matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left | MouseButton::Middle)
            );
            if !right_button
                && intentional_pane_press
                && matches!(self.state.mode, Mode::Terminal | Mode::Resize)
            {
                if let (Some(ws_idx), Some(info)) = (
                    self.state.active,
                    self.state.pane_at(mouse.column, mouse.row).cloned(),
                ) {
                    self.focus_pane_internal_via_api(ws_idx, info.id);
                }
            }
            if let Some(action) = self.state.handle_mouse(&mut self.terminal_runtimes, mouse) {
                match action {
                    MouseAction::Settings(action) => match action {
                        SettingsAction::SaveTheme(name) => self.save_theme(&name),
                        SettingsAction::SaveSound(enabled) => self.save_sound(enabled),
                        SettingsAction::SaveToastDelivery(delivery) => {
                            self.save_toast_delivery(delivery)
                        }
                        SettingsAction::SaveAgentBorderLabels(enabled) => {
                            self.save_agent_border_labels(enabled)
                        }
                        SettingsAction::SavePaneHistory(enabled) => {
                            self.save_pane_history_persistence(enabled)
                        }
                        SettingsAction::SaveSwitchAsciiInputSourceInPrefix(enabled) => {
                            self.save_switch_ascii_input_source_in_prefix(enabled)
                        }
                        SettingsAction::InstallRecommendedIntegrations => {
                            self.install_recommended_integrations()
                        }
                    },
                    MouseAction::FocusWorkspace { ws_idx } => {
                        self.focus_workspace_idx_via_api(ws_idx)
                    }
                    MouseAction::FocusTab { tab_idx } => self.focus_tab_idx_via_api(tab_idx),
                    MouseAction::FocusPane { ws_idx, pane_id } => {
                        self.focus_pane_internal_via_api(ws_idx, pane_id)
                    }
                    MouseAction::FocusToastTarget => self.focus_toast_target_via_api(),
                    MouseAction::ProjectTree(action) => self.execute_project_tree_action(action),
                    MouseAction::MoveWorkspace {
                        source_ws_idx,
                        insert_idx,
                    } => self.move_workspace_via_api(source_ws_idx, insert_idx),
                    MouseAction::MoveTab {
                        ws_idx,
                        source_tab_idx,
                        insert_idx,
                    } => self.move_tab_via_api(ws_idx, source_tab_idx, insert_idx),
                    MouseAction::SetSplitRatio { path, ratio } => {
                        self.set_split_ratio_via_api(path, ratio)
                    }
                    MouseAction::RenameModal(action) => {
                        self.apply_rename_mouse_action_via_api(action)
                    }
                    MouseAction::ConfirmCloseAccept => self.confirm_close_accept_via_api(),
                    MouseAction::ContextMenu { menu, idx } => {
                        self.apply_context_menu_action_via_api(menu, idx)
                    }
                }
            }
        }
        if previous_settings_section != crate::app::state::SettingsSection::Integrations
            && self.state.settings.section == crate::app::state::SettingsSection::Integrations
        {
            self.refresh_integration_recommendations();
        }
        if self.state.agent_panel_sort != previous_agent_panel_sort {
            self.save_agent_panel_sort(self.state.agent_panel_sort);
        }

        if let Some(content) = self.state.request_clipboard_write.take() {
            if self
                .event_tx
                .try_send(crate::events::AppEvent::ClipboardWrite { content })
                .is_err()
            {
                tracing::warn!("failed to queue clipboard write event");
            }
        }

        // Sync autoscroll deadline with state (mouse handler may have
        // set or cleared selection_autoscroll during handle_mouse).
        if self.state.selection_autoscroll.is_none() {
            self.selection_autoscroll_deadline = None;
        } else if self.selection_autoscroll_deadline.is_none() {
            self.selection_autoscroll_deadline =
                Some(std::time::Instant::now() + super::SELECTION_AUTOSCROLL_INTERVAL);
        }
    }

    fn handle_popup_mouse(&mut self, mouse: MouseEvent) {
        let Some((_outer, inner)) =
            crate::ui::popup_pane_rects(&self.state, self.state.view.terminal_area)
        else {
            return;
        };
        if mouse.column < inner.x
            || mouse.column >= inner.x.saturating_add(inner.width)
            || mouse.row < inner.y
            || mouse.row >= inner.y.saturating_add(inner.height)
        {
            return;
        }
        let Some(rt) = self.popup_runtime() else {
            self.close_popup_pane();
            return;
        };
        let column = mouse.column.saturating_sub(inner.x);
        let row = mouse.row.saturating_sub(inner.y);
        let bytes = match mouse.kind {
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => match rt.wheel_routing() {
                Some(crate::pane::WheelRouting::MouseReport) => {
                    rt.encode_mouse_wheel(mouse.kind, column, row, mouse.modifiers)
                }
                Some(crate::pane::WheelRouting::AlternateScroll) => {
                    rt.encode_alternate_scroll(mouse.kind)
                }
                Some(crate::pane::WheelRouting::HostScroll) | None => {
                    let lines_per_notch = self.state.mouse_scroll_lines;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => rt.scroll_up(lines_per_notch),
                        MouseEventKind::ScrollDown => rt.scroll_down(lines_per_notch),
                        _ => {}
                    }
                    return;
                }
            },
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                rt.encode_mouse_button(mouse.kind, column, row, mouse.modifiers)
            }
            MouseEventKind::Moved => {
                rt.encode_mouse_motion(mouse.kind, column, row, mouse.modifiers)
            }
        };
        let Some(bytes) = bytes else {
            return;
        };
        rt.scroll_reset();
        if let Err(err) = rt.try_send_bytes(Bytes::from(bytes)) {
            warn!(err = %err, kind = ?mouse.kind, "failed to forward popup mouse event");
        }
    }

    fn handle_modified_url_click(&mut self, mouse: MouseEvent) -> bool {
        if self.state.mode != Mode::Terminal
            || !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            || !mouse.modifiers.contains(modified_url_click_modifier())
        {
            return false;
        }

        let Some(info) = self.state.pane_at(mouse.column, mouse.row).cloned() else {
            return false;
        };
        let viewport_row = mouse.row.saturating_sub(info.inner_rect.y);
        let col = mouse.column.saturating_sub(info.inner_rect.x);
        let Some(url) =
            self.state
                .url_at_pane_cell(&self.terminal_runtimes, info.id, viewport_row, col)
        else {
            return false;
        };

        self.last_pane_click = None;
        match self.invoke_plugin_link_handler_for_url(&url, info.id) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(err = %err, url = %url, "failed to invoke plugin link handler");
            }
        }
        if let Err(err) = crate::platform::open_url(&url) {
            tracing::warn!(err = %err, url = %url, "failed to open pane URL");
        }
        true
    }

    fn handle_pane_double_click(&mut self, mouse: MouseEvent) -> bool {
        if !self.state.copy_on_select {
            self.last_pane_click = None;
            return false;
        }

        // A pane press stops being a double-click candidate once it becomes
        // a drag or completes as a real text selection.
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.last_pane_click = None;
                return false;
            }
            MouseEventKind::Up(MouseButton::Left)
                if self
                    .state
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.is_visible()) =>
            {
                self.last_pane_click = None;
                return false;
            }
            _ => {}
        }

        // Only terminal-pane left-clicks can start this gesture; other clicks
        // should keep their existing mouse behavior and clear stale candidates.
        let Some(click) = self.pane_click_candidate(mouse) else {
            return false;
        };

        // Require the second click to land near the first click in the same pane
        // and within the double-click window so adjacent interactions do not copy.
        if !self.take_pane_double_click(click) {
            return false;
        }

        // Preserve a short highlight after copying so the user gets visible
        // confirmation without leaving a persistent selection behind.
        self.copy_double_clicked_word(click)
    }

    fn pane_click_candidate(&mut self, mouse: MouseEvent) -> Option<PaneClickState> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }

        if !mouse.modifiers.is_empty() {
            self.last_pane_click = None;
            return None;
        }

        if self.state.mode != Mode::Terminal {
            self.last_pane_click = None;
            return None;
        }

        let Some(info) = self.state.pane_at(mouse.column, mouse.row).cloned() else {
            self.last_pane_click = None;
            return None;
        };

        Some(PaneClickState {
            pane_id: info.id,
            viewport_row: mouse.row - info.inner_rect.y,
            col: mouse.column - info.inner_rect.x,
            at: std::time::Instant::now(),
        })
    }

    fn take_pane_double_click(&mut self, click: PaneClickState) -> bool {
        if !self
            .last_pane_click
            .is_some_and(|last| last.is_double_click_for(click))
        {
            self.last_pane_click = Some(click);
            return false;
        }

        self.last_pane_click = None;
        true
    }

    fn copy_double_clicked_word(&mut self, click: PaneClickState) -> bool {
        let copied = self.state.copy_word_at_pane_cell(
            &self.terminal_runtimes,
            click.pane_id,
            click.viewport_row,
            click.col,
        );
        if copied {
            self.selection_highlight_clear_deadline =
                Some(std::time::Instant::now() + super::PANE_COPY_HIGHLIGHT_DURATION);
        }
        copied
    }
}

pub(crate) fn is_modal_paste_shortcut(key: &KeyEvent) -> bool {
    if !matches!(key.code, KeyCode::Char('v' | 'V')) {
        return false;
    }

    #[cfg(target_os = "macos")]
    {
        key.modifiers.contains(KeyModifiers::SUPER) || key.modifiers.contains(KeyModifiers::CONTROL)
    }

    #[cfg(not(target_os = "macos"))]
    {
        key.modifiers.contains(KeyModifiers::CONTROL)
    }
}

pub(crate) fn modal_paste_target_active(state: &AppState) -> bool {
    match state.mode {
        Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane | Mode::NewLinkedWorktree => {
            true
        }
        Mode::OpenExistingWorktree => state
            .worktree_open
            .as_ref()
            .is_some_and(|open| open.search_focused),
        Mode::Navigator => state.navigator.search_focused,
        Mode::Copy => state
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.search.prompt.is_some()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

// Note: split_pane needs runtime (event_tx for PTY spawn), so it lives on App
impl AppState {
    #[cfg(test)]
    pub(crate) fn split_pane(
        &mut self,
        terminal_runtimes: &mut crate::terminal::TerminalRuntimeRegistry,
        direction: Direction,
    ) {
        // Actual PTY spawning happens in Workspace::split_focused
        // which needs events channel — this is called from navigate_key
        // where we don't have async context, so the workspace handles it
        let (rows, cols) = self.estimate_pane_size();
        let new_rows = (rows / 2).max(4);
        let new_cols = (cols / 2).max(10);

        let follow_cwd = self
            .active
            .and_then(|i| self.workspaces.get(i))
            .and_then(|ws| {
                let tab = ws.active_tab()?;
                let pane_id = tab.layout.focused();
                tab.follow_cwd_for_pane(pane_id, &self.terminals, terminal_runtimes)
            });
        let cwd = Some(super::creation::resolve_new_terminal_cwd(
            &self.new_terminal_cwd,
            follow_cwd,
        ));

        let previous_focus = self.current_pane_focus_target();
        if let Some(ws_idx) = self.active {
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return;
            };
            if let Ok(new_pane) = ws.split_focused(
                direction,
                new_rows,
                new_cols,
                cwd,
                self.pane_scrollback_limit_bytes,
                self.host_terminal_theme,
                crate::pane::PaneShellConfig::new(&self.default_shell, self.shell_mode),
                Vec::new(),
            ) {
                let new_id = new_pane.pane_id;
                terminal_runtimes.insert(new_pane.terminal.id.clone(), new_pane.runtime);
                self.remove_alias_shadowed_by_new_pane(new_id);
                self.terminals
                    .insert(new_pane.terminal.id.clone(), new_pane.terminal);
                self.record_pane_focus_change(previous_focus, ws_idx, new_id);
                self.mark_session_dirty();
                self.mode = Mode::Terminal;
            }
        }
    }
}

#[cfg(test)]
fn state_with_workspaces(names: &[&str]) -> AppState {
    let mut state = AppState::test_new();
    state.workspaces = names
        .iter()
        .map(|name| crate::workspace::Workspace::test_new(name))
        .collect();
    if !state.workspaces.is_empty() {
        state.active = Some(0);
        state.selected = 0;
        state.mode = Mode::Navigate;
    }
    state
}

#[cfg(test)]
fn app_for_mouse_test() -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.mode = Mode::Terminal;
    app.state.update_available = None;
    app.state.latest_release_notes_available = false;
    app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 20);
    app.state.view.terminal_area = ratatui::layout::Rect::new(26, 0, 80, 20);
    app
}

#[cfg(test)]
fn mouse(
    kind: crossterm::event::MouseEventKind,
    col: u16,
    row: u16,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: col,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    }
}

#[cfg(test)]
fn numbered_lines_bytes(count: usize) -> Vec<u8> {
    (0..count)
        .map(|i| format!("{i:06}\r\n"))
        .collect::<String>()
        .into_bytes()
}

#[cfg(test)]
fn capture_snapshot(state: &AppState) -> crate::persist::SessionSnapshot {
    let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
    crate::persist::capture(
        &state.workspaces,
        &state.terminals,
        &terminal_runtimes,
        state.active,
        state.selected,
        state.sidebar_width,
        state.sidebar_section_split,
        state.collapsed_space_keys.clone(),
        state.collapsed_project_keys.clone(),
    )
}

#[cfg(test)]
fn root_layout_ratio(snapshot: &crate::persist::SessionSnapshot) -> Option<f32> {
    match &snapshot.workspaces.first()?.tabs.first()?.layout {
        crate::persist::LayoutSnapshot::Split { ratio, .. } => Some(*ratio),
        crate::persist::LayoutSnapshot::Pane(_) => None,
    }
}

#[cfg(test)]
fn unique_temp_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
#[cfg(unix)]
fn wait_for_file(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.is_empty() {
                return content;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("timed out waiting for {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    fn app_with_project_runtime() -> (
        App,
        crate::app::state::ProjectSessionActivation,
        tokio::sync::mpsc::Receiver<bytes::Bytes>,
    ) {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("project-live")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let workspace_id = app.public_workspace_id(0);
        let public_pane_id = app.public_pane_id(0, pane_id).expect("public pane id");
        let (runtime, receiver) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);
        let session = crate::projects::IndexedSessionSummary {
            stable_key: "session-live".into(),
            backend: "codex".into(),
            ref_kind: crate::projects::SessionRefKind::Id,
            title: "live project session".into(),
            cwd: Some("/tmp/project-live".into()),
            first_activity_at: 1,
            last_activity_at: 2,
            live: true,
            workspace_id: Some(workspace_id.clone()),
            pane_id: Some(public_pane_id.clone()),
            runtime_generation: Some(9),
            session_class: crate::projects::SessionClass::Interactive,
        };
        app.state.projects.snapshot = crate::projects::ProjectsSnapshot {
            projects_schema_version: crate::projects::domain::PROJECTS_SCHEMA_VERSION,
            revision: 7,
            projects: vec![crate::projects::ProjectSummary {
                canonical_key: "project-live".into(),
                kind: crate::projects::ProjectKind::Cwd,
                display_name: "project-live".into(),
                canonical_path: "/tmp/project-live".into(),
                sessions: vec![session],
                automation: Vec::new(),
                thin_count: 0,
                next_cursor: None,
            }],
            topics: Vec::new(),
            scan_status: Vec::new(),
            diagnostic_category: None,
        };
        let activation = crate::app::state::ProjectSessionActivation::Live {
            session_key: "session-live".into(),
            workspace_id,
            pane_id: public_pane_id,
            runtime_generation: 9,
        };
        (app, activation, receiver)
    }

    #[tokio::test]
    async fn live_project_activation_needs_no_extra_enter_before_terminal_input() {
        let (mut app, activation, mut receiver) = app_with_project_runtime();
        app.state.mode = Mode::Navigate;

        app.execute_project_tree_action(crate::app::state::ProjectTreeAction::Activate(activation));
        assert_eq!(app.state.mode, Mode::Terminal);

        app.handle_key(TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()))
            .await;
        assert_eq!(
            receiver.recv().await.expect("terminal input"),
            bytes::Bytes::from_static(b"x")
        );
    }

    #[tokio::test]
    async fn history_activation_is_read_only_and_never_writes_to_the_pty() {
        let (mut app, _activation, mut receiver) = app_with_project_runtime();
        let workspace_count = app.state.workspaces.len();
        let pane_count = app.state.workspaces[0].tabs[0].layout.pane_count();
        let revision = app.state.projects.snapshot.revision;

        app.execute_project_tree_action(crate::app::state::ProjectTreeAction::Activate(
            crate::app::state::ProjectSessionActivation::History {
                session_key: "session-live".into(),
            },
        ));
        assert_eq!(app.state.mode, Mode::ProjectHistory);
        app.handle_key(TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()))
            .await;

        assert!(receiver.try_recv().is_err());
        assert_eq!(app.state.workspaces.len(), workspace_count);
        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.pane_count(),
            pane_count
        );
        assert_eq!(app.state.projects.snapshot.revision, revision);
    }

    #[tokio::test]
    async fn history_esc_walks_dead_pane_fallback_chain_to_navigate_mode() {
        // 1. Original pane still alive: Esc restores it and returns to Terminal mode.
        let (mut app, _activation, _receiver) = app_with_project_runtime();
        let original_pane = app.state.workspaces[0].tabs[0].root_pane;
        app.execute_project_tree_action(crate::app::state::ProjectTreeAction::Activate(
            crate::app::state::ProjectSessionActivation::History {
                session_key: "session-live".into(),
            },
        ));
        assert_eq!(app.state.mode, Mode::ProjectHistory);
        app.handle_key(TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        assert_eq!(app.state.mode, Mode::Terminal);
        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.focused(),
            original_pane
        );

        // 2. Original pane exited, its tab still exists: focus that tab's current pane.
        let (mut app, _activation, _receiver) = app_with_project_runtime();
        let root_pane = app.state.workspaces[0].tabs[0].root_pane;
        // A second pane makes this case distinguishable from case 1: the tab's current focus
        // is the split, not the root pane the history session came from.
        let split_pane = app.state.workspaces[0].tabs[0]
            .layout
            .split_focused(ratatui::layout::Direction::Horizontal);
        assert_ne!(split_pane, root_pane);
        app.execute_project_tree_action(crate::app::state::ProjectTreeAction::Activate(
            crate::app::state::ProjectSessionActivation::History {
                session_key: "session-live".into(),
            },
        ));
        // Simulate the recorded pane having exited while history was open.
        app.state.previous_pane_focus = Some(crate::app::state::PaneFocusTarget {
            workspace_id: app.state.workspaces[0].id.clone(),
            pane_id: crate::layout::PaneId::from_raw(9999),
        });
        app.handle_key(TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        assert_eq!(app.state.mode, Mode::Terminal);
        assert_eq!(app.state.workspaces[0].tabs[0].layout.focused(), split_pane);

        // 3. Original tab is gone: fall back to the first live pane elsewhere.
        let (mut app, _activation, _receiver) = app_with_project_runtime();
        app.execute_project_tree_action(crate::app::state::ProjectTreeAction::Activate(
            crate::app::state::ProjectSessionActivation::History {
                session_key: "session-live".into(),
            },
        ));
        app.state.previous_pane_focus = Some(crate::app::state::PaneFocusTarget {
            workspace_id: "workspace-gone".into(),
            pane_id: crate::layout::PaneId::from_raw(9999),
        });
        app.state.projects.history_return_tab = Some((
            "workspace-gone".into(),
            crate::layout::PaneId::from_raw(9999),
        ));
        let surviving_pane = app.state.workspaces[0].tabs[0].root_pane;
        app.handle_key(TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        assert_eq!(app.state.mode, Mode::Terminal);
        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.focused(),
            surviving_pane
        );

        // 4. No live pane at all: enter Navigate mode instead of focusing nothing.
        let (mut app, _activation, _receiver) = app_with_project_runtime();
        app.execute_project_tree_action(crate::app::state::ProjectTreeAction::Activate(
            crate::app::state::ProjectSessionActivation::History {
                session_key: "session-live".into(),
            },
        ));
        app.state.previous_pane_focus = None;
        app.state.projects.history_return_tab = None;
        app.state.workspaces.clear();
        app.state.active = None;
        app.handle_key(TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()))
            .await;
        assert_eq!(app.state.mode, Mode::Navigate);
    }

    #[tokio::test]
    async fn paste_routes_to_rename_modal_input() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::RenameTab;
        app.state.name_input = "2".into();
        app.state.name_input_replace_on_type = true;

        app.handle_paste("feature/logs".into()).await;

        assert_eq!(app.state.name_input, "feature/logs");
        assert!(!app.state.name_input_replace_on_type);
    }

    #[tokio::test]
    async fn paste_routes_to_new_linked_worktree_input() {
        let mut app = test_app();
        app.state.mode = Mode::NewLinkedWorktree;
        app.state.name_input = "generated-branch".into();
        app.state.name_input_replace_on_type = true;
        app.state.worktree_create = Some(crate::app::state::WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/herdr".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/herdr".into(),
            repo_key: "repo-key".into(),
            repo_name: "herdr".into(),
            branch: "generated-branch".into(),
            checkout_path: "/repo/herdr-generated-branch".into(),
            error: None,
            creating: false,
        });

        app.handle_paste("feature/linear-302".into()).await;

        assert_eq!(app.state.name_input, "feature/linear-302");
        assert_eq!(
            app.state
                .worktree_create
                .as_ref()
                .map(|create| create.branch.as_str()),
            Some("feature/linear-302")
        );
    }

    #[test]
    fn modal_paste_shortcut_matches_platform_primary_v() {
        #[cfg(target_os = "macos")]
        let modifiers = KeyModifiers::SUPER;
        #[cfg(not(target_os = "macos"))]
        let modifiers = KeyModifiers::CONTROL;

        assert!(is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('v'),
            modifiers
        )));
        assert!(is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('V'),
            modifiers | KeyModifiers::SHIFT
        )));
        assert!(!is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::ALT
        )));
    }

    #[test]
    fn modal_paste_target_is_active_only_for_text_inputs() {
        let mut state = AppState::test_new();

        state.mode = Mode::RenameTab;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::Navigator;
        state.navigator.search_focused = false;
        assert!(!modal_paste_target_active(&state));
        state.navigator.search_focused = true;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::ConfirmClose;
        assert!(!modal_paste_target_active(&state));
    }

    #[test]
    fn project_browser_arrow_keys_move_between_peer_sidebar_tabs() {
        let mut app = test_app();
        app.state.sidebar_view = crate::app::state::SidebarView::Projects;

        assert!(
            app.handle_projects_navigate_key(KeyEvent::new(KeyCode::Right, KeyModifiers::empty()))
        );
        assert_eq!(
            app.state.sidebar_view,
            crate::app::state::SidebarView::Clusters
        );
        assert_eq!(
            app.state.projects.grouping,
            crate::app::state::ProjectGrouping::Topics
        );

        assert!(
            app.handle_projects_navigate_key(KeyEvent::new(KeyCode::Left, KeyModifiers::empty()))
        );
        assert_eq!(
            app.state.sidebar_view,
            crate::app::state::SidebarView::Projects
        );

        assert!(
            app.handle_projects_navigate_key(KeyEvent::new(KeyCode::Left, KeyModifiers::empty()))
        );
        assert_eq!(
            app.state.sidebar_view,
            crate::app::state::SidebarView::SpacesAgents
        );
    }
}
