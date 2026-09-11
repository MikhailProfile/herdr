//! Conversation messages of one indexed session, for previews.

use std::io;

use serde::{Deserialize, Serialize};

use super::claude::{parse_transcript, MESSAGE_START, ROLE_SEPARATOR};
use super::model::SessionRecord;
use super::store::HistoryCache;

/// Upper bound on characters kept per message in a preview.
pub const MAX_PREVIEW_MESSAGE_CHARS: usize = 8000;

/// One user or assistant message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionMessage {
    /// `user` or `assistant`.
    pub role: String,
    pub text: String,
}

/// Splits cached body text (see [`super::claude`]) back into messages.
pub fn split_messages(body: &str) -> Vec<SessionMessage> {
    body.split(MESSAGE_START)
        .filter(|chunk| !chunk.is_empty())
        .filter_map(|chunk| {
            let (role, text) = chunk.split_once(ROLE_SEPARATOR)?;
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                return None;
            }
            Some(SessionMessage {
                role: role.to_string(),
                text: text.to_string(),
            })
        })
        .collect()
}

/// All messages of `record`, from the text cache when present, otherwise by
/// re-reading the transcript.
pub fn session_messages(
    record: &SessionRecord,
    cache: &HistoryCache,
) -> io::Result<Vec<SessionMessage>> {
    if record.text_len > 0 {
        if let Ok(text) = cache.read_text(&record.agent, &record.session_id) {
            return Ok(split_messages(&text));
        }
    }
    let parsed = parse_transcript(
        &record.transcript_path,
        &record.session_id,
        Some(&record.project_path),
    )?;
    Ok(split_messages(&parsed.body_text))
}

/// Truncates a message to `max_chars`, marking the cut.
pub fn truncate_message(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => (format!("{}…", &text[..index]), true),
        None => (text.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_history::model::{TitleKind, CLAUDE_AGENT};
    use std::path::{Path, PathBuf};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-history/claude/projects")
            .join(name)
    }

    #[test]
    fn splits_cached_body_into_messages() {
        let body = format!(
            "{MESSAGE_START}user{ROLE_SEPARATOR}hello\nthere\n{MESSAGE_START}assistant{ROLE_SEPARATOR}hi\n{MESSAGE_START}user{ROLE_SEPARATOR}\n"
        );
        let messages = split_messages(&body);
        assert_eq!(
            messages,
            vec![
                SessionMessage {
                    role: "user".into(),
                    text: "hello\nthere".into()
                },
                SessionMessage {
                    role: "assistant".into(),
                    text: "hi".into()
                },
            ]
        );
        assert!(split_messages("").is_empty());
        assert!(split_messages("garbage without markers").is_empty());
    }

    #[test]
    fn falls_back_to_the_transcript_when_no_text_is_cached() {
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-history-messages-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let cache = HistoryCache::new(&root);
        let record = SessionRecord {
            agent: CLAUDE_AGENT.into(),
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            project_path: "/Users/demo/alpha".into(),
            original_cwd: None,
            git_branch: String::new(),
            title: None,
            title_kind: TitleKind::None,
            first_prompt: String::new(),
            first_ts_ms: 0,
            last_ts_ms: 0,
            message_count: 0,
            transcript_path: fixture(
                "-Users-demo-alpha/11111111-1111-4111-8111-111111111111.jsonl",
            ),
            file_len: 0,
            file_mtime_ms: 0,
            text_len: 0,
        };
        let messages = session_messages(&record, &cache).expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert!(messages[0].text.starts_with("please add a telegram"));
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[2].text,
            "Done. The bot posts on success and failure."
        );

        // A cached copy wins when the record says text is cached.
        cache
            .write_text(
                CLAUDE_AGENT,
                &record.session_id,
                &format!("{MESSAGE_START}user{ROLE_SEPARATOR}cached\n"),
            )
            .expect("write");
        let cached = SessionRecord {
            text_len: 1,
            ..record
        };
        let messages = session_messages(&cached, &cache).expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "cached");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate_message("short", 10), ("short".into(), false));
        assert_eq!(truncate_message("héllo wörld", 5), ("héllo…".into(), true));
    }
}
