use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::agent_history::{self, Query, SearchOptions, DEFAULT_RESULT_LIMIT};
use crate::api::schema::{
    AgentHistoryMessagesInfo, AgentHistoryMessagesParams, AgentHistoryProjectInfo,
    AgentHistorySearchParams, AgentHistorySessionInfo, AgentHistoryStatusInfo, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

const DISABLED_CODE: &str = "agent_history_disabled";
const DEFAULT_MESSAGE_LIMIT: usize = 400;
const DISABLED_MESSAGE: &str = "agent history is disabled; set [agent_history] enabled = true";

impl App {
    pub(super) fn handle_agent_history_search(
        &mut self,
        id: String,
        params: AgentHistorySearchParams,
    ) -> String {
        if !self.agent_history.enabled {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        let query = Query::parse(&params.query);
        let options = SearchOptions {
            deep: params.deep && self.agent_history.scan_options.deep_text,
            limit: params
                .limit
                .map(|limit| limit as usize)
                .filter(|limit| *limit > 0)
                .unwrap_or(DEFAULT_RESULT_LIMIT),
        };
        let groups = agent_history::search(
            &self.agent_history.index,
            &self.agent_history.cache,
            &query,
            options,
            agent_history::now_ms(),
        );
        let open_panes = self.open_agent_session_panes();
        let groups = groups
            .into_iter()
            .map(|group| {
                let workspace_id = self
                    .open_workspace_idx_for_checkout(Path::new(&group.project_path))
                    .map(|ws_idx| self.public_workspace_id(ws_idx));
                let sessions = group
                    .sessions
                    .into_iter()
                    .filter_map(|session| {
                        let record = self
                            .agent_history
                            .index
                            .get(&session.agent, &session.session_id)?;
                        let open_pane_id = open_panes
                            .get(&(session.agent.clone(), session.session_id.clone()))
                            .cloned();
                        Some(AgentHistorySessionInfo {
                            agent: session.agent,
                            session_id: session.session_id,
                            title: record.title.clone(),
                            title_kind: record.title_kind,
                            first_prompt: record.first_prompt.clone(),
                            project_path: record.project_path.clone(),
                            git_branch: record.git_branch.clone(),
                            first_ts_ms: record.first_ts_ms,
                            last_ts_ms: record.last_ts_ms,
                            message_count: record.message_count,
                            score: session.score,
                            match_tier: session.tier,
                            snippet: session.snippet,
                            open_pane_id,
                        })
                    })
                    .collect();
                AgentHistoryProjectInfo {
                    project_path: group.project_path,
                    label: group.label,
                    workspace_id,
                    sessions,
                }
            })
            .collect();

        encode_success(
            id,
            ResponseResult::AgentHistoryResults {
                query: query.raw().to_string(),
                deep: options.deep,
                groups,
            },
        )
    }

    pub(super) fn handle_agent_history_messages(
        &mut self,
        id: String,
        params: AgentHistoryMessagesParams,
    ) -> String {
        if !self.agent_history.enabled {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        let Some(record) = self
            .agent_history
            .index
            .get(&params.agent, params.session_id.trim())
        else {
            return encode_error(
                id,
                "session_unknown",
                "session is not in the agent history index",
            );
        };
        let messages = match agent_history::session_messages(record, &self.agent_history.cache) {
            Ok(messages) => messages,
            Err(err) => {
                return encode_error(
                    id,
                    "transcript_unreadable",
                    format!("failed to read transcript: {err}"),
                )
            }
        };
        let total = messages.len();
        let offset = params
            .offset
            .map(|offset| offset as usize)
            .unwrap_or(0)
            .min(total);
        let limit = params
            .limit
            .map(|limit| limit as usize)
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_MESSAGE_LIMIT);
        let mut truncated = offset + limit < total;
        let messages = messages
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|message| {
                let (text, cut) = agent_history::truncate_message(
                    &message.text,
                    agent_history::MAX_PREVIEW_MESSAGE_CHARS,
                );
                truncated |= cut;
                agent_history::SessionMessage {
                    role: message.role,
                    text,
                }
            })
            .collect();
        encode_success(
            id,
            ResponseResult::AgentHistoryMessages {
                conversation: AgentHistoryMessagesInfo {
                    agent: record.agent.clone(),
                    session_id: record.session_id.clone(),
                    title: record.title.clone(),
                    project_path: record.project_path.clone(),
                    total: total as u32,
                    offset: offset as u32,
                    truncated,
                    messages,
                },
            },
        )
    }

    pub(super) fn handle_agent_history_status(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentHistoryStatus {
                status: self.agent_history_status_info(),
            },
        )
    }

    pub(super) fn handle_agent_history_refresh(&mut self, id: String) -> String {
        if !self.agent_history.enabled {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        let now = Instant::now();
        self.request_agent_history_scan(now);
        self.start_agent_history_scan_if_due(now);
        encode_success(
            id,
            ResponseResult::AgentHistoryStatus {
                status: self.agent_history_status_info(),
            },
        )
    }

    pub(crate) fn agent_history_status_info(&self) -> AgentHistoryStatusInfo {
        let runtime = &self.agent_history;
        AgentHistoryStatusInfo {
            enabled: runtime.enabled,
            deep_search: runtime.scan_options.deep_text,
            indexing: runtime.in_flight,
            sessions: runtime.index.len() as u64,
            projects: runtime.index.project_count() as u64,
            last_scan_ms: runtime.last_scan_ms(),
            cache_dir: runtime.cache.root().display().to_string(),
        }
    }

    /// Maps `(agent, session id)` of every pane with a running agent that owns a
    /// native session to that pane's public id.
    fn open_agent_session_panes(&self) -> HashMap<(String, String), String> {
        self.session_snapshot()
            .agents
            .into_iter()
            .filter_map(|agent| {
                let session = agent.agent_session?;
                Some(((session.agent, session.value), agent.pane_id))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{AgentHistorySearchParams, EmptyParams, Method, Request};
    use crate::app::agent_history::tests::{point_at_fixtures, scan_now};
    use crate::app::App;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    fn search(app: &mut App, query: &str, deep: bool) -> serde_json::Value {
        let response = app.handle_api_request(Request {
            id: "test:search".into(),
            method: Method::AgentHistorySearch(AgentHistorySearchParams {
                query: query.into(),
                limit: None,
                deep,
            }),
        });
        serde_json::from_str(&response).expect("json response")
    }

    #[test]
    fn search_groups_by_project_and_ranks_title_first() {
        let mut app = test_app();
        point_at_fixtures(&mut app, "api-search");
        scan_now(&mut app);

        let response = search(&mut app, "telegram", true);
        let result = &response["result"];
        assert_eq!(result["type"], "agent_history_results");
        assert_eq!(result["query"], "telegram");
        assert_eq!(result["deep"], true);
        let groups = result["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["label"], "alpha");
        assert_eq!(groups[0]["project_path"], "/Users/demo/alpha");
        let sessions = groups[0]["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["match_tier"], "title");
        assert_eq!(sessions[0]["title"], "telegram bot deploy");
        assert_eq!(sessions[0]["title_kind"], "custom");
        assert!(sessions[0].get("open_pane_id").is_none());

        let response = search(&mut app, "zoom", true);
        let groups = response["result"]["groups"].as_array().expect("groups");
        assert_eq!(groups[0]["label"], "beta");
        assert_eq!(groups[0]["sessions"][0]["match_tier"], "prompt");

        let response = search(&mut app, "pagination", true);
        let session = &response["result"]["groups"][0]["sessions"][0];
        assert_eq!(session["match_tier"], "text");
        assert_eq!(session["snippet"]["role"], "assistant");

        let response = search(&mut app, "pagination", false);
        assert!(response["result"]["groups"]
            .as_array()
            .expect("groups")
            .is_empty());

        let response = search(&mut app, "", true);
        let total: usize = response["result"]["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .map(|group| group["sessions"].as_array().map_or(0, Vec::len))
            .sum();
        assert_eq!(total, 3);
        let _ = std::fs::remove_dir_all(app.agent_history.cache.root());
    }

    #[test]
    fn messages_return_the_conversation_with_paging() {
        use crate::api::schema::AgentHistoryMessagesParams;
        let mut app = test_app();
        point_at_fixtures(&mut app, "api-messages");
        scan_now(&mut app);
        let fetch = |app: &mut App, offset: Option<u32>, limit: Option<u32>| {
            let response = app.handle_api_request(Request {
                id: "test:messages".into(),
                method: Method::AgentHistoryMessages(AgentHistoryMessagesParams {
                    agent: "claude".into(),
                    session_id: "11111111-1111-4111-8111-111111111111".into(),
                    offset,
                    limit,
                }),
            });
            serde_json::from_str::<serde_json::Value>(&response).expect("json")
        };
        let full = fetch(&mut app, None, None);
        let conversation = &full["result"]["conversation"];
        assert_eq!(full["result"]["type"], "agent_history_messages");
        assert_eq!(conversation["title"], "telegram bot deploy");
        assert_eq!(conversation["total"], 3);
        assert_eq!(conversation["truncated"], false);
        let messages = conversation["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert!(messages[2]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Done.")));

        let page = fetch(&mut app, Some(1), Some(1));
        let conversation = &page["result"]["conversation"];
        assert_eq!(conversation["offset"], 1);
        assert_eq!(conversation["truncated"], true);
        assert_eq!(conversation["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(conversation["messages"][0]["role"], "assistant");

        let missing = app.handle_api_request(Request {
            id: "test:missing".into(),
            method: Method::AgentHistoryMessages(AgentHistoryMessagesParams {
                agent: "claude".into(),
                session_id: "nope".into(),
                offset: None,
                limit: None,
            }),
        });
        let missing: serde_json::Value = serde_json::from_str(&missing).expect("json");
        assert_eq!(missing["error"]["code"], "session_unknown");
        let _ = std::fs::remove_dir_all(app.agent_history.cache.root());
    }

    #[test]
    fn status_and_refresh_report_index_state() {
        let mut app = test_app();
        point_at_fixtures(&mut app, "api-status");
        let response = app.handle_api_request(Request {
            id: "test:status".into(),
            method: Method::AgentHistoryStatus(EmptyParams::default()),
        });
        let status: serde_json::Value = serde_json::from_str(&response).expect("json");
        assert_eq!(status["result"]["type"], "agent_history_status");
        assert_eq!(status["result"]["status"]["sessions"], 0);
        assert_eq!(status["result"]["status"]["indexing"], false);

        let response = app.handle_api_request(Request {
            id: "test:refresh".into(),
            method: Method::AgentHistoryRefresh(EmptyParams::default()),
        });
        let status: serde_json::Value = serde_json::from_str(&response).expect("json");
        assert_eq!(status["result"]["status"]["indexing"], true);
        let event = app.event_rx.blocking_recv().expect("scan event");
        app.handle_internal_event_with_render_impact(event);
        assert_eq!(app.agent_history_status_info().sessions, 3);
        assert_eq!(app.agent_history_status_info().projects, 2);
        let _ = std::fs::remove_dir_all(app.agent_history.cache.root());
    }

    #[test]
    fn disabled_index_rejects_search_and_refresh() {
        let mut app = test_app();
        app.agent_history.enabled = false;
        let response = search(&mut app, "x", true);
        assert_eq!(response["error"]["code"], "agent_history_disabled");
        let response = app.handle_api_request(Request {
            id: "test:refresh".into(),
            method: Method::AgentHistoryRefresh(EmptyParams::default()),
        });
        let value: serde_json::Value = serde_json::from_str(&response).expect("json");
        assert_eq!(value["error"]["code"], "agent_history_disabled");
    }
}
