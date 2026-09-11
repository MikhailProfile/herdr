//! Agent history overlay: search past agent sessions (served by `agent_history.search`)
//! grouped by project, and resume one through `agent.resume`.

use std::time::{Duration, Instant};

use super::*;
use crate::api::schema::{
    AgentHistoryMessagesParams, AgentHistorySearchParams, AgentResumeParams, AgentResumePlacement,
    EmptyParams, Method, ResponseResult,
};

/// Delay after the last keystroke before a search request is sent.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
const RESULT_LIMIT: u32 = 200;

/// Flattens the last search results into rows: one per project, followed by its
/// sessions unless the project is collapsed.
pub(super) fn history_rows(overlay: &ClientHistoryOverlay) -> Vec<ClientHistoryRow> {
    let mut rows = Vec::new();
    for group in &overlay.groups {
        let collapsed = overlay.collapsed_projects.contains(&group.project_path);
        let count = group.sessions.len();
        rows.push(ClientHistoryRow {
            depth: 0,
            label: group.label.clone(),
            meta: format!("{count} session{}", if count == 1 { "" } else { "s" }),
            badge: "",
            detail: group.project_path.clone(),
            open: group.workspace_id.is_some(),
            collapsed,
            target: ClientHistoryTarget::Project {
                project_path: group.project_path.clone(),
            },
        });
        if collapsed {
            continue;
        }
        for session in &group.sessions {
            let fallback = if session.first_prompt.is_empty() {
                session.session_id.clone()
            } else {
                session.first_prompt.clone()
            };
            let label = session
                .title
                .clone()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| fallback.clone());
            let badge = match session.match_tier {
                Some(crate::agent_history::MatchTier::Title) => "T",
                Some(crate::agent_history::MatchTier::Prompt) => "P",
                Some(crate::agent_history::MatchTier::Text) => "~",
                None => " ",
            };
            let date = crate::agent_history::format_date_ms(session.last_ts_ms);
            let meta = if session.git_branch.is_empty() {
                date
            } else {
                format!("{date} · {}", session.git_branch)
            };
            let detail = match &session.snippet {
                Some(snippet) => format!("{}: {}", snippet.role, snippet.text),
                None => fallback,
            };
            rows.push(ClientHistoryRow {
                depth: 1,
                label,
                meta,
                badge,
                detail,
                open: session.open_pane_id.is_some(),
                collapsed: false,
                target: ClientHistoryTarget::Session {
                    agent: session.agent.clone(),
                    session_id: session.session_id.clone(),
                },
            });
        }
    }
    rows
}

pub(super) fn history_selected_index(
    rows: &[ClientHistoryRow],
    overlay: &ClientHistoryOverlay,
) -> Option<usize> {
    match overlay.selected.as_ref() {
        Some(target) => rows.iter().position(|row| row.target == *target),
        None => first_session_index(rows),
    }
}

pub(super) fn selected_history_target(
    rows: &[ClientHistoryRow],
    overlay: &ClientHistoryOverlay,
) -> Option<ClientHistoryTarget> {
    history_selected_index(rows, overlay).map(|index| rows[index].target.clone())
}

fn first_session_index(rows: &[ClientHistoryRow]) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row.target, ClientHistoryTarget::Session { .. }))
        .or_else(|| (!rows.is_empty()).then_some(0))
}

impl ClientShellState {
    pub(super) fn open_agent_history_overlay(&mut self, outcome: &mut ClientShellInput) {
        self.overlay = Some(ClientShellOverlay::AgentHistory(ClientHistoryOverlay {
            preview: None,
            query: String::new(),
            search_focused: true,
            selected: None,
            scroll: 0,
            collapsed_projects: HashSet::new(),
            groups: Vec::new(),
            results_query: String::new(),
            request_generation: 0,
            results_generation: 0,
            searching: false,
            resuming: false,
            error: None,
        }));
        self.send_agent_history_search(outcome);
        outcome.repaint = true;
    }

    fn send_agent_history_search(&mut self, outcome: &mut ClientShellInput) {
        self.history_search_deadline = None;
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        overlay.request_generation = overlay.request_generation.saturating_add(1);
        let generation = overlay.request_generation;
        let query = overlay.query.clone();
        overlay.searching = true;
        overlay.error = None;
        let sent = self.push_endpoint_method_with_kind(
            Method::AgentHistorySearch(AgentHistorySearchParams {
                query,
                limit: Some(RESULT_LIMIT),
                deep: true,
            }),
            PendingEndpointKind::AgentHistorySearch { generation },
            outcome,
        );
        if !sent {
            if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                overlay.searching = false;
            }
        }
    }

    fn schedule_agent_history_search(&mut self) {
        self.history_search_deadline = Some(Instant::now() + SEARCH_DEBOUNCE);
    }

    /// Sends the debounced search once its deadline has passed.
    pub(crate) fn tick_agent_history_search(&mut self, now: Instant) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        if self
            .history_search_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return outcome;
        }
        self.history_search_deadline = None;
        if matches!(self.overlay, Some(ClientShellOverlay::AgentHistory(_))) {
            self.send_agent_history_search(&mut outcome);
            outcome.repaint = true;
        }
        outcome
    }

    pub(super) fn move_agent_history_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let rows = history_rows(overlay);
        if rows.is_empty() {
            overlay.selected = None;
            return;
        }
        let selected = history_selected_index(&rows, overlay).unwrap_or(0);
        let next =
            (selected as isize + delta).clamp(0, rows.len().saturating_sub(1) as isize) as usize;
        overlay.selected = Some(rows[next].target.clone());
    }

    fn select_last_agent_history_row(&mut self) {
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        overlay.selected = history_rows(overlay).last().map(|row| row.target.clone());
    }

    fn toggle_selected_history_project(&mut self) {
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let rows = history_rows(overlay);
        let Some(index) = history_selected_index(&rows, overlay) else {
            return;
        };
        let project_path = match &rows[index].target {
            ClientHistoryTarget::Project { project_path } => project_path.clone(),
            ClientHistoryTarget::Session { .. } => {
                let Some(project) = rows[..index]
                    .iter()
                    .rev()
                    .find_map(|row| match &row.target {
                        ClientHistoryTarget::Project { project_path } => Some(project_path.clone()),
                        _ => None,
                    })
                else {
                    return;
                };
                project
            }
        };
        if !overlay.collapsed_projects.remove(&project_path) {
            overlay.collapsed_projects.insert(project_path.clone());
        }
        overlay.selected = Some(ClientHistoryTarget::Project { project_path });
    }

    pub(super) fn accept_agent_history_selection(
        &mut self,
        placement: AgentResumePlacement,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if overlay.resuming {
            return;
        }
        let rows = history_rows(overlay);
        match selected_history_target(&rows, overlay) {
            Some(ClientHistoryTarget::Session { agent, session_id }) => {
                overlay.resuming = true;
                overlay.error = None;
                let sent = self.push_endpoint_method_with_kind(
                    Method::AgentResume(AgentResumeParams {
                        agent,
                        session_id,
                        cwd: None,
                        focus: true,
                        placement,
                    }),
                    PendingEndpointKind::AgentResume,
                    outcome,
                );
                if !sent {
                    if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                        overlay.resuming = false;
                    }
                }
            }
            Some(ClientHistoryTarget::Project { .. }) => self.toggle_selected_history_project(),
            None => {}
        }
        outcome.repaint = true;
    }

    /// Opens the conversation preview for the selected session.
    fn open_agent_history_preview(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let rows = history_rows(overlay);
        let Some(index) = history_selected_index(&rows, overlay) else {
            return;
        };
        let ClientHistoryTarget::Session { agent, session_id } = rows[index].target.clone() else {
            self.toggle_selected_history_project();
            return;
        };
        overlay.preview = Some(ClientHistoryPreview {
            agent: agent.clone(),
            session_id: session_id.clone(),
            title: rows[index].label.clone(),
            messages: Vec::new(),
            total: 0,
            truncated: false,
            scroll: 0,
            loading: true,
        });
        overlay.error = None;
        let sent = self.push_endpoint_method_with_kind(
            Method::AgentHistoryMessages(AgentHistoryMessagesParams {
                agent: agent.clone(),
                session_id: session_id.clone(),
                offset: None,
                limit: None,
            }),
            PendingEndpointKind::AgentHistoryMessages { agent, session_id },
            outcome,
        );
        if !sent {
            if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                overlay.preview = None;
            }
        }
    }

    fn close_agent_history_preview(&mut self) {
        if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
            overlay.preview = None;
        }
    }

    /// Scrolls the preview when it is open, otherwise moves the list selection.
    pub(super) fn scroll_agent_history(&mut self, delta: isize) {
        let max = self.hits.history_preview_max_scroll;
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match overlay.preview.as_mut() {
            Some(preview) => {
                let next = preview.scroll as isize + delta;
                preview.scroll = next.clamp(0, max as isize) as usize;
            }
            None => self.move_agent_history_selection(delta),
        }
    }

    fn set_agent_history_preview_scroll(&mut self, scroll: usize) {
        let max = self.hits.history_preview_max_scroll;
        if let Some(ClientShellOverlay::AgentHistory(ClientHistoryOverlay {
            preview: Some(preview),
            ..
        })) = self.overlay.as_mut()
        {
            preview.scroll = scroll.min(max);
        }
    }

    fn refresh_agent_history(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        overlay.searching = true;
        overlay.error = None;
        let sent = self.push_endpoint_method_with_kind(
            Method::AgentHistoryRefresh(EmptyParams::default()),
            PendingEndpointKind::AgentHistoryRefresh,
            outcome,
        );
        if !sent {
            if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                overlay.searching = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn insert_agent_history_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        if !overlay.search_focused || overlay.resuming {
            return false;
        }
        overlay
            .query
            .extend(text.chars().filter(|character| !character.is_control()));
        overlay.selected = None;
        overlay.scroll = 0;
        self.schedule_agent_history_search();
        true
    }

    /// Returns true when the key was consumed by the agent history overlay.
    pub(super) fn route_agent_history_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::KeyModifiers;

        let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_ref() else {
            return false;
        };
        let search_focused = overlay.search_focused;
        let resuming = overlay.resuming;
        let preview_open = overlay.preview.is_some();
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        outcome.repaint = true;

        if preview_open && !resuming {
            let control = modifiers.contains(KeyModifiers::CONTROL);
            match code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char(' ') | KeyCode::Char('h') => {
                    self.close_agent_history_preview()
                }
                KeyCode::Enter => {
                    self.accept_agent_history_selection(AgentResumePlacement::Tab, outcome)
                }
                KeyCode::Char('w') if modifiers.is_empty() => {
                    self.accept_agent_history_selection(AgentResumePlacement::Workspace, outcome)
                }
                KeyCode::Down | KeyCode::Char('j') => self.scroll_agent_history(1),
                KeyCode::Up | KeyCode::Char('k') => self.scroll_agent_history(-1),
                KeyCode::PageDown => self.scroll_agent_history(10),
                KeyCode::PageUp => self.scroll_agent_history(-10),
                KeyCode::Char('d') if control => self.scroll_agent_history(10),
                KeyCode::Char('u') if control => self.scroll_agent_history(-10),
                KeyCode::Home | KeyCode::Char('g') => self.set_agent_history_preview_scroll(0),
                KeyCode::End | KeyCode::Char('G') => {
                    self.set_agent_history_preview_scroll(usize::MAX)
                }
                _ => {}
            }
            return true;
        }

        if code == KeyCode::Esc {
            if search_focused && !resuming {
                if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                    overlay.search_focused = false;
                }
            } else {
                self.overlay = None;
                self.history_search_deadline = None;
            }
            return true;
        }
        if resuming {
            return true;
        }
        if code == KeyCode::Enter {
            self.accept_agent_history_selection(AgentResumePlacement::Tab, outcome);
            return true;
        }
        let control = modifiers.contains(KeyModifiers::CONTROL);
        if code == KeyCode::Up || (code == KeyCode::Char('p') && control) {
            self.move_agent_history_selection(-1);
            return true;
        }
        if code == KeyCode::Down || (code == KeyCode::Char('n') && control) {
            self.move_agent_history_selection(1);
            return true;
        }
        if code == KeyCode::Char('d') && control {
            self.move_agent_history_selection(8);
            return true;
        }
        if code == KeyCode::Right {
            // Works from search mode too, where space and `l` are query text.
            self.open_agent_history_preview(outcome);
            return true;
        }
        if search_focused {
            let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
                return true;
            };
            if code == KeyCode::Char('u') && control {
                overlay.query.clear();
                overlay.selected = None;
                self.schedule_agent_history_search();
            } else if code == KeyCode::Backspace {
                if overlay.query.pop().is_some() {
                    overlay.selected = None;
                    self.schedule_agent_history_search();
                }
            } else if let KeyCode::Char(character) = code {
                if modifiers.difference(KeyModifiers::SHIFT).is_empty() {
                    let text = key
                        .generated_text
                        .clone()
                        .unwrap_or_else(|| character.to_string());
                    self.insert_agent_history_text(&text);
                }
            }
            return true;
        }
        if code == KeyCode::Char('u') && control {
            self.move_agent_history_selection(-8);
            return true;
        }
        if modifiers.is_empty() {
            match code {
                KeyCode::Char('/') => {
                    if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                        overlay.search_focused = true;
                    }
                }
                KeyCode::Char('j') => self.move_agent_history_selection(1),
                KeyCode::Char('k') => self.move_agent_history_selection(-1),
                KeyCode::Home => {
                    if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                        overlay.selected = None;
                        overlay.scroll = 0;
                    }
                }
                KeyCode::End | KeyCode::Char('G') => self.select_last_agent_history_row(),
                KeyCode::Char(' ') | KeyCode::Char('l') => self.open_agent_history_preview(outcome),
                KeyCode::Char('w') => {
                    self.accept_agent_history_selection(AgentResumePlacement::Workspace, outcome)
                }
                KeyCode::Char('r') => self.refresh_agent_history(outcome),
                KeyCode::Backspace => {
                    if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                        if !overlay.query.is_empty() {
                            overlay.query.clear();
                            overlay.selected = None;
                            self.schedule_agent_history_search();
                        }
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// Applies a server response for one of the overlay's requests; returns repaint.
    pub(super) fn handle_agent_history_endpoint_result(
        &mut self,
        kind: PendingEndpointKind,
        result: Result<ResponseResult, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        match (kind, result) {
            (
                PendingEndpointKind::AgentHistorySearch { generation },
                Ok(ResponseResult::AgentHistoryResults { query, groups, .. }),
            ) => {
                let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
                    return false;
                };
                if generation <= overlay.results_generation {
                    return false;
                }
                let query_changed = overlay.results_query != query;
                overlay.results_generation = generation;
                overlay.results_query = query;
                overlay.groups = groups;
                overlay.searching = generation < overlay.request_generation;
                let rows = history_rows(overlay);
                if query_changed
                    || overlay
                        .selected
                        .as_ref()
                        .is_none_or(|target| !rows.iter().any(|row| row.target == *target))
                {
                    overlay.selected =
                        first_session_index(&rows).map(|index| rows[index].target.clone());
                    overlay.scroll = 0;
                }
                true
            }
            (PendingEndpointKind::AgentHistorySearch { generation }, Err(error)) => {
                let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() else {
                    return false;
                };
                if generation >= overlay.request_generation {
                    overlay.searching = false;
                }
                overlay.error = Some(error.message);
                true
            }
            (
                PendingEndpointKind::AgentHistoryMessages { agent, session_id },
                Ok(ResponseResult::AgentHistoryMessages { conversation }),
            ) => {
                let Some(ClientShellOverlay::AgentHistory(ClientHistoryOverlay {
                    preview: Some(preview),
                    ..
                })) = self.overlay.as_mut()
                else {
                    return false;
                };
                if preview.agent != agent || preview.session_id != session_id {
                    return false;
                }
                if let Some(title) = conversation.title.filter(|title| !title.is_empty()) {
                    preview.title = title;
                }
                preview.messages = conversation.messages;
                preview.total = conversation.total as usize;
                preview.truncated = conversation.truncated;
                preview.scroll = 0;
                preview.loading = false;
                true
            }
            (PendingEndpointKind::AgentHistoryMessages { .. }, Err(error)) => {
                if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                    overlay.preview = None;
                    overlay.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::AgentHistoryRefresh, Ok(_)) => {
                if matches!(self.overlay, Some(ClientShellOverlay::AgentHistory(_))) {
                    self.send_agent_history_search(outcome);
                }
                true
            }
            (PendingEndpointKind::AgentHistoryRefresh, Err(error)) => {
                if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                    overlay.searching = false;
                    overlay.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::AgentResume, Ok(_)) => {
                // The server only answers agent.resume with a success result once the
                // session pane exists and is focused, so any Ok closes the picker.
                if matches!(self.overlay, Some(ClientShellOverlay::AgentHistory(_))) {
                    self.overlay = None;
                    self.history_search_deadline = None;
                }
                true
            }
            (PendingEndpointKind::AgentResume, Err(error)) => {
                if let Some(ClientShellOverlay::AgentHistory(overlay)) = self.overlay.as_mut() {
                    overlay.resuming = false;
                    overlay.error = Some(error.message);
                }
                true
            }
            _ => false,
        }
    }
}
