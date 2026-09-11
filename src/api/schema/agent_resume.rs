use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_agent() -> String {
    "claude".into()
}

/// Where a resumed session should be opened.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentResumePlacement {
    /// New tab in the workspace whose checkout matches the project; a new
    /// workspace is created when none matches.
    #[default]
    Tab,
    /// Always a new workspace for the project directory.
    Workspace,
}

/// Parameters for `agent.resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentResumeParams {
    /// Agent kind; only `claude` is supported today.
    #[serde(default = "default_agent")]
    pub agent: String,
    /// Native session id reported by the agent (Claude Code session UUID).
    pub session_id: String,
    /// Directory to resume in. Defaults to the project directory recorded in the
    /// agent history index; required for sessions the index does not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Focus the resumed pane. Default: true.
    #[serde(default = "default_true")]
    pub focus: bool,
    #[serde(default)]
    pub placement: AgentResumePlacement,
}
