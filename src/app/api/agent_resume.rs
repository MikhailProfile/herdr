use std::path::PathBuf;

use bytes::Bytes;

use crate::agent_resume::PersistedAgentSession;
use crate::api::schema::{AgentResumeParams, AgentResumePlacement, ResponseResult};
use crate::app::{App, Mode};

use super::responses::{encode_error, encode_success};

const CLAUDE_SOURCE: &str = "herdr:claude";
const CLAUDE_AGENT: &str = "claude";
const MAX_TAB_LABEL_CHARS: usize = 32;

impl App {
    pub(super) fn handle_agent_resume(&mut self, id: String, params: AgentResumeParams) -> String {
        if params.agent != CLAUDE_AGENT {
            return encode_error(
                id,
                "unsupported_agent",
                format!(
                    "agent {:?} cannot be resumed; supported agents: {CLAUDE_AGENT}",
                    params.agent
                ),
            );
        }
        let session_id = params.session_id.trim();
        let Some((session_ref, plan)) = crate::agent_resume::claude_resume_plan(session_id) else {
            return encode_error(
                id,
                "invalid_session_id",
                "session id is empty, too long, or contains control characters",
            );
        };

        if let Some((ws_idx, pane_id)) = self.find_open_agent_session_pane(CLAUDE_AGENT, session_id)
        {
            if params.focus {
                self.state.focus_pane_in_workspace(ws_idx, pane_id);
                self.state.mark_active_tab_seen();
                self.state.mode = Mode::Terminal;
            }
            return self.agent_resumed_response(id, ws_idx, pane_id, false, true, plan.argv);
        }

        let record = self
            .agent_history
            .index
            .get(CLAUDE_AGENT, session_id)
            .cloned();
        let cwd = match params.cwd.map(PathBuf::from).or_else(|| {
            record
                .as_ref()
                .map(|record| PathBuf::from(record.workspace_path()))
        }) {
            Some(cwd) => cwd,
            None => {
                return encode_error(
                    id,
                    "session_unknown",
                    "session is not in the agent history index; pass cwd to resume it anyway",
                )
            }
        };
        if !cwd.is_dir() {
            return encode_error(
                id,
                "project_missing",
                format!("project directory {} does not exist", cwd.display()),
            );
        }

        let existing = match params.placement {
            AgentResumePlacement::Tab => self.open_workspace_idx_for_checkout(&cwd),
            AgentResumePlacement::Workspace => None,
        };
        let (ws_idx, tab_idx, reused_workspace) = match existing {
            Some(ws_idx) => match self.create_resume_tab(ws_idx, cwd) {
                Ok(tab_idx) => (ws_idx, tab_idx, true),
                Err(err) => return encode_error(id, "tab_create_failed", err.to_string()),
            },
            None => match self.create_workspace_with_options(cwd, params.focus) {
                Ok(ws_idx) => (ws_idx, 0, false),
                Err(err) => return encode_error(id, "tab_create_failed", err.to_string()),
            },
        };

        let Some((pane_id, terminal_id)) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs.get(tab_idx))
            .and_then(|tab| Some((tab.root_pane, tab.terminal_id(tab.root_pane)?.clone())))
        else {
            return encode_error(id, "resume_input_failed", "resume pane disappeared");
        };
        let Some(command) = crate::app::agent_resume::shell_command_from_argv(&plan.argv) else {
            return encode_error(id, "resume_input_failed", "empty resume command");
        };
        let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
            return encode_error(id, "resume_input_failed", "resume pane has no shell");
        };
        let mut input = command;
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            return encode_error(
                id,
                "resume_input_failed",
                format!("failed to submit resume command: {err}"),
            );
        }

        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_persisted_agent_session(PersistedAgentSession {
                source: CLAUDE_SOURCE.into(),
                agent: CLAUDE_AGENT.into(),
                session_ref,
            });
        }
        if let Some(label) = record
            .as_ref()
            .map(|record| tab_label(record.display_title()))
        {
            if let Some(tab) = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .and_then(|ws| ws.tabs.get_mut(tab_idx))
            {
                tab.set_custom_name(label);
            }
        }
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mark_active_tab_seen();
            self.state.mode = Mode::Terminal;
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        if reused_workspace {
            self.emit_tab_created_events(ws_idx, tab_idx);
        } else {
            self.emit_workspace_open_events(ws_idx);
        }

        self.agent_resumed_response(id, ws_idx, pane_id, reused_workspace, false, plan.argv)
    }

    /// Creates a plain shell tab in `ws_idx` at `cwd` and returns its index.
    fn create_resume_tab(&mut self, ws_idx: usize, cwd: PathBuf) -> std::io::Result<usize> {
        let (rows, cols) = self.state.estimate_pane_size();
        let default_shell = self.state.default_shell.clone();
        let shell_mode = self.state.shell_mode;
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let ws = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))?;
        let (tab_idx, terminal, runtime) = ws.create_tab(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&default_shell, shell_mode),
            Vec::new(),
        )?;
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.remove_alias_shadowed_by_new_pane(
            self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
        );
        Ok(tab_idx)
    }

    fn agent_resumed_response(
        &self,
        id: String,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        reused_workspace: bool,
        focused_existing: bool,
        argv: Vec<String>,
    ) -> String {
        let Some(tab_idx) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.find_tab_index_for_pane(pane_id))
        else {
            return encode_error(id, "resume_input_failed", "resume pane disappeared");
        };
        let (Some(tab), Some(pane)) = (
            self.tab_info(ws_idx, tab_idx),
            self.pane_info(ws_idx, pane_id),
        ) else {
            return encode_error(id, "resume_input_failed", "resume pane disappeared");
        };
        encode_success(
            id,
            ResponseResult::AgentResumed {
                workspace: self.workspace_info(ws_idx),
                tab,
                pane,
                reused_workspace,
                focused_existing,
                argv,
            },
        )
    }

    /// Finds the pane whose running agent owns `session_id`. Panes that merely
    /// remember a session (agent exited, shell still open) do not count.
    fn find_open_agent_session_pane(
        &self,
        agent: &str,
        session_id: &str,
    ) -> Option<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| {
                ws.tabs.iter().find_map(|tab| {
                    tab.panes.iter().find_map(|(pane_id, pane)| {
                        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
                        if !terminal.is_agent_terminal() {
                            return None;
                        }
                        let hosts = terminal
                            .hook_authority
                            .as_ref()
                            .and_then(|authority| {
                                let session_ref = authority.session_ref.as_ref()?;
                                Some(
                                    authority.agent_label == agent
                                        && session_ref.value == session_id,
                                )
                            })
                            .unwrap_or(false)
                            || terminal
                                .persisted_agent_session
                                .as_ref()
                                .is_some_and(|session| {
                                    session.agent == agent
                                        && session.session_ref.value == session_id
                                });
                        hosts.then_some((ws_idx, *pane_id))
                    })
                })
            })
    }
}

fn tab_label(title: &str) -> String {
    let mut chars = title.char_indices();
    match chars.nth(MAX_TAB_LABEL_CHARS) {
        Some((index, _)) => format!("{}…", &title[..index]),
        None => title.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::api::schema::{Method, Request};
    use crate::app::agent_history::tests::{point_at_fixtures, scan_now};
    use crate::config::{Config, ShellModeConfig};

    const ALPHA_ONE: &str = "11111111-1111-4111-8111-111111111111";
    const ALPHA_TWO: &str = "22222222-2222-4222-8222-222222222222";

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        app
    }

    /// Marks the pane's terminal as running Claude, as screen detection would.
    fn mark_claude_running(app: &mut App, ws_idx: usize, tab_idx: usize) {
        let terminal_id = app.state.workspaces[ws_idx].tabs[tab_idx]
            .terminal_id(app.state.workspaces[ws_idx].tabs[tab_idx].root_pane)
            .cloned()
            .expect("tab terminal");
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal state");
        terminal.set_detected_state(
            Some(crate::detect::Agent::Claude),
            crate::detect::AgentState::Idle,
        );
    }

    fn resume(app: &mut App, params: AgentResumeParams) -> serde_json::Value {
        let response = app.handle_api_request(Request {
            id: "test:resume".into(),
            method: Method::AgentResume(params),
        });
        serde_json::from_str(&response).expect("json response")
    }

    fn params(session_id: &str) -> AgentResumeParams {
        AgentResumeParams {
            agent: "claude".into(),
            session_id: session_id.into(),
            cwd: None,
            focus: true,
            placement: AgentResumePlacement::Tab,
        }
    }

    #[test]
    fn rejects_bad_agent_id_unknown_session_and_missing_project() {
        let mut app = test_app();
        point_at_fixtures(&mut app, "resume-errors");
        scan_now(&mut app);

        let mut wrong_agent = params(ALPHA_ONE);
        wrong_agent.agent = "codex".into();
        assert_eq!(
            resume(&mut app, wrong_agent)["error"]["code"],
            "unsupported_agent"
        );
        assert_eq!(
            resume(&mut app, params("bad\nid"))["error"]["code"],
            "invalid_session_id"
        );
        assert_eq!(
            resume(&mut app, params("99999999-9999-4999-8999-999999999999"))["error"]["code"],
            "session_unknown"
        );
        // The fixture's recorded project path does not exist on this machine.
        assert_eq!(
            resume(&mut app, params(ALPHA_ONE))["error"]["code"],
            "project_missing"
        );
        assert!(app.state.workspaces.is_empty());
        let _ = std::fs::remove_dir_all(app.agent_history.cache.root());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resumes_into_new_workspace_then_reuses_it_and_focuses_open_sessions() {
        let mut app = test_app();
        // A shell that stays alive, so panes are not respawned mid-test.
        app.state.default_shell = "/bin/sh".into();
        point_at_fixtures(&mut app, "resume-flow");
        // Pane runtimes need the tokio runtime, so scan without blocking the thread.
        app.start_agent_history_scan_if_due(std::time::Instant::now());
        let event = app.event_rx.recv().await.expect("scan event");
        app.handle_internal_event_with_render_impact(event);
        assert_eq!(app.agent_history.index.len(), 3);
        let project =
            std::env::temp_dir().join(format!("herdr-agent-resume-project-{}", std::process::id()));
        std::fs::create_dir_all(&project).expect("project dir");

        let mut first = params(ALPHA_ONE);
        first.cwd = Some(project.display().to_string());
        let response = resume(&mut app, first.clone());
        let result = &response["result"];
        assert_eq!(result["type"], "agent_resumed", "{response}");
        assert_eq!(result["reused_workspace"], false);
        assert_eq!(result["focused_existing"], false);
        assert_eq!(result["argv"][1], "--resume");
        assert_eq!(result["argv"][2], ALPHA_ONE);
        assert_eq!(result["pane"]["agent_session"]["value"], ALPHA_ONE);
        assert_eq!(result["pane"]["agent_session"]["source"], "herdr:claude");
        assert_eq!(result["tab"]["label"], "telegram bot deploy");
        let first_pane = result["pane"]["pane_id"]
            .as_str()
            .expect("pane id")
            .to_string();
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.active, Some(0));

        // Same session again while no agent is detected yet: the remembered
        // session does not count as open, so a fresh tab is created.
        let response = resume(&mut app, first.clone());
        assert_eq!(response["result"]["focused_existing"], false);
        assert_eq!(response["result"]["reused_workspace"], true);
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);

        // Once Claude is running in the first pane, resuming focuses it instead.
        mark_claude_running(&mut app, 0, 0);
        let response = resume(&mut app, first);
        assert_eq!(response["result"]["focused_existing"], true, "{response}");
        assert_eq!(response["result"]["pane"]["pane_id"], first_pane);
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);

        // Another session in the same project: new tab in the matching workspace.
        let mut second = params(ALPHA_TWO);
        second.cwd = Some(project.display().to_string());
        second.focus = false;
        let response = resume(&mut app, second);
        assert_eq!(response["result"]["reused_workspace"], true);
        assert_eq!(response["result"]["focused_existing"], false);
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].tabs.len(), 3);
        assert_eq!(response["result"]["tab"]["label"], "Review pull request 42");

        // Explicit workspace placement always creates a new workspace.
        let mut third = params(ALPHA_TWO);
        third.cwd = Some(project.display().to_string());
        third.placement = AgentResumePlacement::Workspace;
        // Use an id the index does not know; cwd makes it resumable anyway.
        third.session_id = "33333333-3333-4333-8333-333333333333".into();
        let response = resume(&mut app, third);
        assert_eq!(response["result"]["reused_workspace"], false, "{response}");
        assert_eq!(app.state.workspaces.len(), 2);

        shutdown_test_runtimes(&mut app);
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(app.agent_history.cache.root());
    }

    #[test]
    fn tab_labels_are_truncated() {
        assert_eq!(tab_label("short"), "short");
        let long = "x".repeat(40);
        let label = tab_label(&long);
        assert_eq!(label.chars().count(), MAX_TAB_LABEL_CHARS + 1);
        assert!(label.ends_with('…'));
    }
}
