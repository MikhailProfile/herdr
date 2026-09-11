//! Parser for Claude Code transcripts (`~/.claude/projects/<dir>/<session>.jsonl`).
//!
//! Each line is one JSON record. Only a handful of record types matter here; every
//! other record and every unknown field is skipped without allocation.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use serde::Deserialize;

use super::model::{SessionRecord, TitleKind, CLAUDE_AGENT};
use super::timestamp::{parse_iso_ms, system_time_ms};

/// Upper bound on cached body text per session.
pub const MAX_BODY_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const MAX_FIRST_PROMPT_CHARS: usize = 200;
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Marks the start of one message inside cached body text.
pub(super) const MESSAGE_START: char = '\u{1e}';
/// Separates the role prefix from the message text.
pub(super) const ROLE_SEPARATOR: char = '\u{1f}';

/// Result of parsing one transcript: metadata plus searchable body text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTranscript {
    pub record: SessionRecord,
    /// Concatenated user and assistant text, one message per
    /// `MESSAGE_START role ROLE_SEPARATOR text \n` run. Empty when nothing was said.
    pub body_text: String,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(rename = "gitBranch", default)]
    git_branch: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    #[serde(rename = "customTitle", default)]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle", default)]
    ai_title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(rename = "worktreeSession", default)]
    worktree_session: Option<WorktreeSession>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Content,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Content {
    #[default]
    Missing,
    Text(String),
    Blocks(Vec<Block>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct WorktreeSession {
    #[serde(rename = "originalCwd", default)]
    original_cwd: Option<String>,
}

/// Parses the transcript at `path`. `project_hint` is used as the project path when
/// no record carries a working directory.
pub fn parse_transcript(
    path: &Path,
    session_id: &str,
    project_hint: Option<&str>,
) -> io::Result<ParsedTranscript> {
    let metadata = std::fs::metadata(path)?;
    let file_mtime_ms = metadata.modified().map(system_time_ms).unwrap_or(0);
    let mut reader = BufReader::with_capacity(256 * 1024, File::open(path)?);
    let mut buffer = Vec::new();
    let mut state = ParseState::default();

    loop {
        buffer.clear();
        let read = (&mut reader)
            .take(MAX_LINE_BYTES as u64)
            .read_until(b'\n', &mut buffer)?;
        if read == 0 {
            break;
        }
        let Ok(line) = serde_json::from_slice::<Line>(&buffer) else {
            continue;
        };
        state.absorb(line);
    }

    let (title, title_kind) = state.title();
    let project_path = state
        .project_path
        .or_else(|| project_hint.map(str::to_string))
        .unwrap_or_default();
    let first_ts_ms = state.first_ts_ms.unwrap_or(file_mtime_ms);
    let last_ts_ms = state.last_ts_ms.unwrap_or(file_mtime_ms);
    let text_len = state.body_text.len() as u64;

    Ok(ParsedTranscript {
        record: SessionRecord {
            agent: CLAUDE_AGENT.into(),
            session_id: session_id.into(),
            project_path,
            original_cwd: state.original_cwd,
            git_branch: state.git_branch.unwrap_or_default(),
            title,
            title_kind,
            first_prompt: state.first_prompt.unwrap_or_default(),
            first_ts_ms,
            last_ts_ms,
            message_count: state.message_count,
            transcript_path: path.to_path_buf(),
            file_len: metadata.len(),
            file_mtime_ms,
            text_len,
        },
        body_text: state.body_text,
    })
}

#[derive(Default)]
struct ParseState {
    project_path: Option<String>,
    original_cwd: Option<String>,
    git_branch: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    summary: Option<String>,
    first_prompt: Option<String>,
    first_ts_ms: Option<i64>,
    last_ts_ms: Option<i64>,
    message_count: u32,
    body_text: String,
    body_truncated: bool,
}

impl ParseState {
    fn absorb(&mut self, line: Line) {
        if self.project_path.is_none() {
            self.project_path = line.cwd.filter(|cwd| !cwd.is_empty());
        }
        if self.git_branch.is_none() {
            self.git_branch = line.git_branch.filter(|branch| !branch.is_empty());
        }
        if let Some(timestamp) = line.timestamp.as_deref().and_then(parse_iso_ms) {
            self.first_ts_ms = Some(self.first_ts_ms.map_or(timestamp, |ts| ts.min(timestamp)));
            self.last_ts_ms = Some(self.last_ts_ms.map_or(timestamp, |ts| ts.max(timestamp)));
        }

        match line.kind.as_str() {
            "custom-title" => {
                self.custom_title = clean_title(line.custom_title).or(self.custom_title.take())
            }
            "ai-title" => self.ai_title = clean_title(line.ai_title).or(self.ai_title.take()),
            "summary" => self.summary = clean_title(line.summary).or(self.summary.take()),
            "worktree-state" => {
                if self.original_cwd.is_none() {
                    self.original_cwd = line
                        .worktree_session
                        .and_then(|session| session.original_cwd)
                        .filter(|cwd| !cwd.is_empty());
                }
            }
            "user" | "assistant" if !line.is_sidechain => {
                self.message_count = self.message_count.saturating_add(1);
                let Some(message) = line.message else {
                    return;
                };
                let text = message_text(message.content);
                if text.is_empty() {
                    return;
                }
                let is_user = line.kind == "user";
                if is_user && (line.is_meta || is_command_echo(&text)) {
                    return;
                }
                if is_user && self.first_prompt.is_none() {
                    self.first_prompt = Some(truncate_chars(
                        &collapse_whitespace(&text),
                        MAX_FIRST_PROMPT_CHARS,
                    ));
                }
                self.push_message(if is_user { "user" } else { "assistant" }, &text);
            }
            _ => {}
        }
    }

    fn push_message(&mut self, role: &str, text: &str) {
        if self.body_truncated {
            return;
        }
        let needed = text.len() + role.len() + 3;
        if self.body_text.len() + needed > MAX_BODY_TEXT_BYTES {
            self.body_truncated = true;
            return;
        }
        self.body_text.push(MESSAGE_START);
        self.body_text.push_str(role);
        self.body_text.push(ROLE_SEPARATOR);
        self.body_text.push_str(text);
        self.body_text.push('\n');
    }

    fn title(&self) -> (Option<String>, TitleKind) {
        if let Some(title) = &self.custom_title {
            return (Some(title.clone()), TitleKind::Custom);
        }
        if let Some(title) = &self.ai_title {
            return (Some(title.clone()), TitleKind::Ai);
        }
        if let Some(title) = &self.summary {
            return (Some(title.clone()), TitleKind::Summary);
        }
        (None, TitleKind::None)
    }
}

fn message_text(content: Content) -> String {
    match content {
        Content::Text(text) => text.trim().to_string(),
        Content::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if block.kind != "text" {
                    continue;
                }
                let Some(text) = block.text else {
                    continue;
                };
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            out
        }
        Content::Missing | Content::Other(_) => String::new(),
    }
}

/// Slash-command echoes and local command output are stored as user records but are
/// not prompts the user typed.
fn is_command_echo(text: &str) -> bool {
    const PREFIXES: [&str; 4] = [
        "<command-name>",
        "<command-message>",
        "<local-command-",
        "<system-reminder>",
    ];
    PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

fn clean_title(value: Option<String>) -> Option<String> {
    let value = value?;
    let cleaned = collapse_whitespace(&value);
    if cleaned.is_empty() {
        return None;
    }
    Some(truncate_chars(&cleaned, MAX_TITLE_CHARS))
}

pub(super) fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

pub(super) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.char_indices();
    match chars.nth(max_chars) {
        Some((index, _)) => format!("{}…", &value[..index]),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-history/claude/projects")
            .join(name)
    }

    fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-agent-history-{}-{name}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, content).expect("write temp transcript");
        path
    }

    #[test]
    fn custom_title_wins_and_first_prompt_skips_command_echo() {
        let path = fixture("-Users-demo-alpha/11111111-1111-4111-8111-111111111111.jsonl");
        let parsed = parse_transcript(&path, "11111111-1111-4111-8111-111111111111", None)
            .expect("parse fixture");
        let record = &parsed.record;
        assert_eq!(record.agent, "claude");
        assert_eq!(record.project_path, "/Users/demo/alpha");
        assert_eq!(record.git_branch, "main");
        assert_eq!(record.title.as_deref(), Some("telegram bot deploy"));
        assert_eq!(record.title_kind, TitleKind::Custom);
        assert_eq!(
            record.first_prompt,
            "please add a telegram notification when the deploy finishes"
        );
        // /clear echo, prompt, assistant, tool_result, final assistant; sidechain excluded.
        assert_eq!(record.message_count, 5);
        assert_eq!(record.first_ts_ms, 1_770_357_573_827);
        assert_eq!(record.last_ts_ms, 1_770_357_700_000);
        assert!(parsed
            .body_text
            .contains("\u{1e}user\u{1f}please add a telegram"));
        assert!(parsed
            .body_text
            .contains("\u{1e}assistant\u{1f}I added a webhook"));
        assert!(!parsed
            .body_text
            .contains("tool output that must not be indexed"));
        assert!(!parsed.body_text.contains("sidechain text"));
        assert!(!parsed.body_text.contains("<command-name>"));
        assert_eq!(record.text_len, parsed.body_text.len() as u64);
    }

    #[test]
    fn ai_title_then_summary_then_none() {
        let path = fixture("-Users-demo-alpha/22222222-2222-4222-8222-222222222222.jsonl");
        let parsed =
            parse_transcript(&path, "22222222-2222-4222-8222-222222222222", None).expect("parse");
        assert_eq!(
            parsed.record.title.as_deref(),
            Some("Review pull request 42")
        );
        assert_eq!(parsed.record.title_kind, TitleKind::Ai);

        let summary_only = write_temp(
            "summary",
            r#"{"type":"summary","summary":"Old style summary","leafUuid":"x"}
{"type":"user","cwd":"/p","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}
"#,
        );
        let parsed = parse_transcript(&summary_only, "s", None).expect("parse");
        let _ = std::fs::remove_file(&summary_only);
        assert_eq!(parsed.record.title.as_deref(), Some("Old style summary"));
        assert_eq!(parsed.record.title_kind, TitleKind::Summary);

        let untitled = write_temp(
            "untitled",
            r#"{"type":"user","cwd":"/p","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"  hello   world "}}
"#,
        );
        let parsed = parse_transcript(&untitled, "u", None).expect("parse");
        let _ = std::fs::remove_file(&untitled);
        assert_eq!(parsed.record.title, None);
        assert_eq!(parsed.record.title_kind, TitleKind::None);
        assert_eq!(parsed.record.first_prompt, "hello world");
        assert_eq!(parsed.record.display_title(), "hello world");
    }

    #[test]
    fn falls_back_to_project_hint_and_worktree_original_cwd() {
        let path = write_temp(
            "hint",
            r#"{"type":"worktree-state","worktreeSession":{"originalCwd":"/main/checkout"}}
{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}
"#,
        );
        let parsed = parse_transcript(&path, "h", Some("/hinted/path")).expect("parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(parsed.record.project_path, "/hinted/path");
        assert_eq!(
            parsed.record.original_cwd.as_deref(),
            Some("/main/checkout")
        );
        assert_eq!(parsed.record.workspace_path(), "/main/checkout");
        assert_eq!(parsed.record.message_count, 1);
    }

    #[test]
    fn tolerates_broken_lines_and_caps_body_text() {
        let big = "x".repeat(MAX_BODY_TEXT_BYTES);
        let content = format!(
            "not json at all\n{{\"type\":\"user\",\"message\":{{\"content\":\"first\"}}}}\n{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{big}\"}}]}}}}\n{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"after cap\"}}]}}}}\n"
        );
        let path = write_temp("cap", &content);
        let parsed = parse_transcript(&path, "c", None).expect("parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(parsed.record.message_count, 3);
        assert!(parsed.body_text.contains("first"));
        assert!(!parsed.body_text.contains("after cap"));
        assert!(parsed.body_text.len() <= MAX_BODY_TEXT_BYTES);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let err = parse_transcript(Path::new("/definitely/missing.jsonl"), "m", None)
            .expect_err("missing file");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn helpers_collapse_and_truncate() {
        assert_eq!(collapse_whitespace("  a \n\t b  "), "a b");
        assert_eq!(truncate_chars("héllo", 3), "hél…");
        assert_eq!(truncate_chars("hi", 3), "hi");
    }
}
