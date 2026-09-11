use serde::{Deserialize, Serialize};

use crate::agent_history::{MatchTier, SessionMessage, Snippet, TitleKind};

fn default_true() -> bool {
    true
}

/// Parameters for `agent_history.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistorySearchParams {
    /// Search text. Every whitespace-separated term must match; an empty query lists
    /// recent sessions.
    #[serde(default)]
    pub query: String,
    /// Maximum sessions returned across all projects. Defaults to 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Also search cached transcript text, not only titles and first prompts.
    #[serde(default = "default_true")]
    pub deep: bool,
}

impl Default for AgentHistorySearchParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: None,
            deep: true,
        }
    }
}

fn default_agent() -> String {
    "claude".into()
}

/// Parameters for `agent_history.messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistoryMessagesParams {
    #[serde(default = "default_agent")]
    pub agent: String,
    pub session_id: String,
    /// Skip this many messages from the start of the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// Maximum messages returned. Defaults to 400.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Conversation excerpt returned by `agent_history.messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistoryMessagesInfo {
    pub agent: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub project_path: String,
    /// Messages in the whole conversation.
    pub total: u32,
    pub offset: u32,
    /// Some messages were left out by `limit`, or a message was cut at its
    /// character cap.
    pub truncated: bool,
    pub messages: Vec<SessionMessage>,
}

/// Index status returned by `agent_history.status` and `agent_history.refresh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistoryStatusInfo {
    pub enabled: bool,
    /// Whether transcript text is cached for deep search.
    pub deep_search: bool,
    /// A scan is currently running in the background.
    pub indexing: bool,
    pub sessions: u64,
    pub projects: u64,
    /// Unix milliseconds of the last completed scan; zero when never scanned.
    pub last_scan_ms: i64,
    pub cache_dir: String,
}

/// One past session in a search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistorySessionInfo {
    pub agent: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub title_kind: TitleKind,
    pub first_prompt: String,
    pub project_path: String,
    pub git_branch: String,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub message_count: u32,
    pub score: u32,
    /// Where the query matched; absent for an empty query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_tier: Option<MatchTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Snippet>,
    /// Public pane id currently hosting this session, when it is already open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_pane_id: Option<String>,
}

/// Sessions grouped under one project directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistoryProjectInfo {
    pub project_path: String,
    pub label: String,
    /// Open workspace whose checkout matches this project, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub sessions: Vec<AgentHistorySessionInfo>,
}
