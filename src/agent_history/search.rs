//! Ranking and grouping of agent history search results.
//!
//! Matching is case-insensitive substring; every whitespace-separated term must
//! occur. Sessions are ranked by where the match was found — title first, then the
//! first prompt, then transcript text — with a recency bonus, and grouped by project.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::claude::{collapse_whitespace, truncate_chars, MESSAGE_START, ROLE_SEPARATOR};
use super::model::{project_label, SessionRecord, TitleKind};
use super::store::{HistoryCache, Index};

/// Default cap on sessions returned across all groups.
pub const DEFAULT_RESULT_LIMIT: usize = 200;

const TITLE_WEIGHT: u32 = 300;
const PROMPT_WEIGHT: u32 = 200;
const TEXT_WEIGHT: u32 = 100;
const CUSTOM_TITLE_BONUS: u32 = 5;
const WHOLE_WORD_BONUS: u32 = 10;
const RECENCY_MAX_BONUS: u32 = 50;
const RECENCY_WINDOW_MS: i64 = 90 * 86_400_000;
const SNIPPET_CONTEXT_CHARS: usize = 60;

/// Where the best match for a session was found.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MatchTier {
    Title,
    Prompt,
    Text,
}

/// Parsed search input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    raw: String,
    terms: Vec<String>,
}

impl Query {
    pub fn parse(raw: &str) -> Self {
        let terms = raw
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        Self {
            raw: raw.trim().to_string(),
            terms,
        }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// True when every term occurs in the lowercased haystack.
    pub fn matches_lower(&self, haystack_lower: &str) -> bool {
        self.terms
            .iter()
            .all(|term| haystack_lower.contains(term.as_str()))
    }

    fn first_term(&self) -> Option<&str> {
        self.terms.first().map(String::as_str)
    }
}

/// Context around a transcript hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Snippet {
    pub role: String,
    pub text: String,
}

/// One matching session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMatch {
    pub agent: String,
    pub session_id: String,
    /// `None` for an empty query, where every session is listed by recency.
    pub tier: Option<MatchTier>,
    pub score: u32,
    pub snippet: Option<Snippet>,
}

/// Sessions of one project folder, best match first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGroup {
    pub project_path: String,
    pub label: String,
    pub best_score: u32,
    pub last_ts_ms: i64,
    pub sessions: Vec<SessionMatch>,
}

/// Tuning for [`search`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// Also search cached transcript text.
    pub deep: bool,
    /// Maximum sessions across all groups.
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            deep: true,
            limit: DEFAULT_RESULT_LIMIT,
        }
    }
}

/// Metadata search: titles and first prompts only. Cheap enough to run on every keystroke.
pub fn search_metadata(index: &Index, query: &Query, now_ms: i64) -> Vec<SessionMatch> {
    index
        .sessions
        .iter()
        .filter_map(|record| metadata_match(record, query, now_ms))
        .collect()
}

/// Deep search over cached transcript text for sessions not already in `skip`.
pub fn search_text(
    index: &Index,
    cache: &HistoryCache,
    query: &Query,
    now_ms: i64,
    skip: &HashSet<String>,
) -> Vec<SessionMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    index
        .sessions
        .iter()
        .filter(|record| record.text_len > 0 && !skip.contains(&record.session_id))
        .filter_map(|record| {
            let text = cache.read_text(&record.agent, &record.session_id).ok()?;
            text_match(record, &text, query, now_ms)
        })
        .collect()
}

/// Full search: metadata first, then transcript text when `options.deep` is set.
pub fn search(
    index: &Index,
    cache: &HistoryCache,
    query: &Query,
    options: SearchOptions,
    now_ms: i64,
) -> Vec<ProjectGroup> {
    let mut matches = search_metadata(index, query, now_ms);
    if options.deep && !query.is_empty() {
        let skip: HashSet<String> = matches.iter().map(|m| m.session_id.clone()).collect();
        matches.extend(search_text(index, cache, query, now_ms, &skip));
    }
    group_matches(index, matches, options.limit)
}

/// Groups matches by project, orders groups and sessions, and applies `limit`.
pub fn group_matches(index: &Index, matches: Vec<SessionMatch>, limit: usize) -> Vec<ProjectGroup> {
    let records: HashMap<(&str, &str), &SessionRecord> = index
        .sessions
        .iter()
        .map(|record| ((record.agent.as_str(), record.session_id.as_str()), record))
        .collect();

    let mut groups: Vec<ProjectGroup> = Vec::new();
    let mut group_index: HashMap<&str, usize> = HashMap::new();
    let mut last_ts: HashMap<(&str, &str), i64> = HashMap::new();

    for session in matches {
        let Some(record) = records.get(&(session.agent.as_str(), session.session_id.as_str()))
        else {
            continue;
        };
        last_ts.insert(
            (record.agent.as_str(), record.session_id.as_str()),
            record.last_ts_ms,
        );
        let project_path = record.project_path.as_str();
        let position = match group_index.get(project_path) {
            Some(position) => *position,
            None => {
                groups.push(ProjectGroup {
                    project_path: project_path.to_string(),
                    label: project_label(project_path),
                    best_score: 0,
                    last_ts_ms: i64::MIN,
                    sessions: Vec::new(),
                });
                let position = groups.len() - 1;
                group_index.insert(project_path, position);
                position
            }
        };
        let group = &mut groups[position];
        group.best_score = group.best_score.max(session.score);
        group.last_ts_ms = group.last_ts_ms.max(record.last_ts_ms);
        group.sessions.push(session);
    }

    for group in &mut groups {
        group.sessions.sort_by(|a, b| {
            let a_ts = last_ts
                .get(&(a.agent.as_str(), a.session_id.as_str()))
                .copied()
                .unwrap_or(0);
            let b_ts = last_ts
                .get(&(b.agent.as_str(), b.session_id.as_str()))
                .copied()
                .unwrap_or(0);
            b.score
                .cmp(&a.score)
                .then_with(|| b_ts.cmp(&a_ts))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
    }
    groups.sort_by(|a, b| {
        b.best_score
            .cmp(&a.best_score)
            .then_with(|| b.last_ts_ms.cmp(&a.last_ts_ms))
            .then_with(|| a.project_path.cmp(&b.project_path))
    });

    let mut remaining = limit;
    groups.retain_mut(|group| {
        if remaining == 0 {
            return false;
        }
        group.sessions.truncate(remaining);
        remaining -= group.sessions.len();
        !group.sessions.is_empty()
    });
    groups
}

fn metadata_match(record: &SessionRecord, query: &Query, now_ms: i64) -> Option<SessionMatch> {
    let recency = recency_bonus(record.last_ts_ms, now_ms);
    if query.is_empty() {
        return Some(SessionMatch {
            agent: record.agent.clone(),
            session_id: record.session_id.clone(),
            tier: None,
            score: recency,
            snippet: None,
        });
    }

    let title_lower = record.title.as_deref().map(str::to_lowercase);
    if let Some(title_lower) = title_lower.filter(|title| query.matches_lower(title)) {
        let mut score = TITLE_WEIGHT + recency + whole_word_bonus(&title_lower, query);
        if record.title_kind == TitleKind::Custom {
            score += CUSTOM_TITLE_BONUS;
        }
        return Some(SessionMatch {
            agent: record.agent.clone(),
            session_id: record.session_id.clone(),
            tier: Some(MatchTier::Title),
            score,
            snippet: None,
        });
    }

    let prompt_lower = record.first_prompt.to_lowercase();
    if !prompt_lower.is_empty() && query.matches_lower(&prompt_lower) {
        return Some(SessionMatch {
            agent: record.agent.clone(),
            session_id: record.session_id.clone(),
            tier: Some(MatchTier::Prompt),
            score: PROMPT_WEIGHT + recency + whole_word_bonus(&prompt_lower, query),
            snippet: None,
        });
    }
    None
}

fn text_match(
    record: &SessionRecord,
    text: &str,
    query: &Query,
    now_ms: i64,
) -> Option<SessionMatch> {
    let lower = text.to_lowercase();
    if !query.matches_lower(&lower) {
        return None;
    }
    let snippet = query
        .first_term()
        .and_then(|term| snippet_for(text, &lower, term));
    Some(SessionMatch {
        agent: record.agent.clone(),
        session_id: record.session_id.clone(),
        tier: Some(MatchTier::Text),
        score: TEXT_WEIGHT
            + recency_bonus(record.last_ts_ms, now_ms)
            + whole_word_bonus(&lower, query),
        snippet,
    })
}

/// Linear decay from `RECENCY_MAX_BONUS` (now) to zero (`RECENCY_WINDOW_MS` ago or older).
fn recency_bonus(last_ts_ms: i64, now_ms: i64) -> u32 {
    let age = (now_ms - last_ts_ms).max(0);
    if age >= RECENCY_WINDOW_MS {
        return 0;
    }
    let remaining = RECENCY_WINDOW_MS - age;
    ((i64::from(RECENCY_MAX_BONUS) * remaining) / RECENCY_WINDOW_MS) as u32
}

fn whole_word_bonus(haystack_lower: &str, query: &Query) -> u32 {
    match query.first_term() {
        Some(term) if contains_whole_word(haystack_lower, term) => WHOLE_WORD_BONUS,
        _ => 0,
    }
}

fn contains_whole_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(needle) {
        let begin = start + offset;
        let end = begin + needle.len();
        let before_ok = haystack[..begin]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}

/// Builds a snippet around the first occurrence of `term`, restricted to the message
/// that contains it. `lower` is the lowercased `text`; when lowercasing changed byte
/// lengths the snippet is taken from the lowercased copy instead.
fn snippet_for(text: &str, lower: &str, term: &str) -> Option<Snippet> {
    let hit = lower.find(term)?;
    let (message_lower, message_offset) = message_around(lower, hit);
    let source = if text.len() == lower.len() {
        &text[message_offset..message_offset + message_lower.len()]
    } else {
        message_lower
    };
    let (role, body_lower) = split_role(message_lower);
    let (_, body) = split_role(source);
    // Byte offsets below come from the lowercased copy; only trust them on the
    // original when both bodies have identical length.
    let body = if body.len() == body_lower.len() {
        body
    } else {
        body_lower
    };
    let hit_in_body = body_lower.find(term)?;

    let start = body[..hit_in_body]
        .char_indices()
        .rev()
        .nth(SNIPPET_CONTEXT_CHARS - 1)
        .map_or(0, |(index, _)| index);
    let end = body[hit_in_body + term.len()..]
        .char_indices()
        .nth(SNIPPET_CONTEXT_CHARS)
        .map_or(body.len(), |(index, _)| hit_in_body + term.len() + index);

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.push_str(&collapse_whitespace(&body[start..end]));
    if end < body.len() {
        snippet.push('…');
    }
    Some(Snippet {
        role: role.to_string(),
        text: truncate_chars(
            &snippet,
            SNIPPET_CONTEXT_CHARS * 2 + term.chars().count() + 2,
        ),
    })
}

/// Returns the message containing byte offset `hit` and its start offset.
fn message_around(text: &str, hit: usize) -> (&str, usize) {
    let start = text[..hit].rfind(MESSAGE_START).unwrap_or(0);
    let end = text[hit..]
        .find(MESSAGE_START)
        .map_or(text.len(), |offset| hit + offset);
    (&text[start..end], start)
}

fn split_role(message: &str) -> (&str, &str) {
    let message = message.strip_prefix(MESSAGE_START).unwrap_or(message);
    match message.split_once(ROLE_SEPARATOR) {
        Some((role, body)) => (role, body.trim_end_matches('\n')),
        None => ("", message.trim_end_matches('\n')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_history::model::CLAUDE_AGENT;
    use std::path::PathBuf;

    const NOW: i64 = 1_800_000_000_000;
    const DAY: i64 = 86_400_000;

    fn record(
        id: &str,
        project: &str,
        title: Option<&str>,
        kind: TitleKind,
        prompt: &str,
        age_days: i64,
    ) -> SessionRecord {
        SessionRecord {
            agent: CLAUDE_AGENT.into(),
            session_id: id.into(),
            project_path: project.into(),
            original_cwd: None,
            git_branch: String::new(),
            title: title.map(str::to_string),
            title_kind: kind,
            first_prompt: prompt.into(),
            first_ts_ms: NOW - age_days * DAY,
            last_ts_ms: NOW - age_days * DAY,
            message_count: 2,
            transcript_path: PathBuf::from(format!("/t/{id}.jsonl")),
            file_len: 1,
            file_mtime_ms: 0,
            text_len: 0,
        }
    }

    fn index(sessions: Vec<SessionRecord>) -> Index {
        Index {
            version: 1,
            scanned_at_ms: NOW,
            sessions,
        }
    }

    struct TempCache(HistoryCache);

    impl TempCache {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-agent-history-search-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            Self(HistoryCache::new(root))
        }
    }

    impl Drop for TempCache {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.root());
        }
    }

    fn body(messages: &[(&str, &str)]) -> String {
        let mut out = String::new();
        for (role, text) in messages {
            out.push(MESSAGE_START);
            out.push_str(role);
            out.push(ROLE_SEPARATOR);
            out.push_str(text);
            out.push('\n');
        }
        out
    }

    #[test]
    fn query_parsing_lowercases_and_requires_all_terms() {
        let query = Query::parse("  Telegram  Bot ");
        assert_eq!(query.raw(), "Telegram  Bot");
        assert!(!query.is_empty());
        assert!(query.matches_lower("deploy the telegram bot now"));
        assert!(!query.matches_lower("telegram only"));
        assert!(Query::parse("   ").is_empty());
    }

    #[test]
    fn title_beats_prompt_beats_text_regardless_of_recency() {
        let temp = TempCache::new("tiers");
        let cache = &temp.0;
        let mut text_hit = record(
            "text",
            "/p",
            Some("unrelated"),
            TitleKind::Ai,
            "nothing here",
            0,
        );
        text_hit.text_len = 1;
        cache
            .write_text(
                CLAUDE_AGENT,
                "text",
                &body(&[("assistant", "we discussed telegram hooks")]),
            )
            .expect("write text");
        let prompt_hit = record(
            "prompt",
            "/p",
            Some("other"),
            TitleKind::Ai,
            "set up telegram alerts",
            10,
        );
        let title_hit = record(
            "title",
            "/p",
            Some("telegram bot"),
            TitleKind::Custom,
            "unrelated",
            80,
        );
        let idx = index(vec![text_hit, prompt_hit, title_hit]);

        let groups = search(
            &idx,
            cache,
            &Query::parse("telegram"),
            SearchOptions::default(),
            NOW,
        );
        assert_eq!(groups.len(), 1);
        let order: Vec<(&str, Option<MatchTier>)> = groups[0]
            .sessions
            .iter()
            .map(|m| (m.session_id.as_str(), m.tier))
            .collect();
        assert_eq!(
            order,
            vec![
                ("title", Some(MatchTier::Title)),
                ("prompt", Some(MatchTier::Prompt)),
                ("text", Some(MatchTier::Text)),
            ]
        );
        let snippet = groups[0].sessions[2].snippet.as_ref().expect("snippet");
        assert_eq!(snippet.role, "assistant");
        assert_eq!(snippet.text, "we discussed telegram hooks");
    }

    #[test]
    fn custom_title_outranks_ai_title_on_ties_and_recency_breaks_further_ties() {
        let ai = record("ai", "/p", Some("telegram"), TitleKind::Ai, "", 0);
        let custom = record("custom", "/p", Some("telegram"), TitleKind::Custom, "", 0);
        let older_custom = record("older", "/p", Some("telegram"), TitleKind::Custom, "", 30);
        let idx = index(vec![ai, older_custom, custom]);
        let groups = group_matches(
            &idx,
            search_metadata(&idx, &Query::parse("telegram"), NOW),
            10,
        );
        let order: Vec<&str> = groups[0]
            .sessions
            .iter()
            .map(|m| m.session_id.as_str())
            .collect();
        // Same age: the /rename title wins. A month of age outweighs that small bonus.
        assert_eq!(order, vec!["custom", "ai", "older"]);
    }

    #[test]
    fn groups_are_ordered_by_best_match_then_recency() {
        let weak_recent = record("a", "/recent", Some("x"), TitleKind::Ai, "telegram", 0);
        let strong_old = record("b", "/old", Some("telegram"), TitleKind::Ai, "", 60);
        let strong_older = record("c", "/older", Some("telegram"), TitleKind::Ai, "", 85);
        let idx = index(vec![weak_recent, strong_older, strong_old]);
        let groups = group_matches(
            &idx,
            search_metadata(&idx, &Query::parse("telegram"), NOW),
            10,
        );
        let order: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(order, vec!["old", "older", "recent"]);
        assert_eq!(groups[0].project_path, "/old");
    }

    #[test]
    fn empty_query_lists_everything_by_recency_with_no_tier() {
        let idx = index(vec![
            record("old", "/p", None, TitleKind::None, "one", 50),
            record("new", "/q", None, TitleKind::None, "two", 1),
        ]);
        let temp = TempCache::new("empty");
        let groups = search(
            &idx,
            &temp.0,
            &Query::parse(""),
            SearchOptions::default(),
            NOW,
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].project_path, "/q");
        assert!(groups
            .iter()
            .flat_map(|g| &g.sessions)
            .all(|m| m.tier.is_none()));
    }

    #[test]
    fn all_terms_must_match_within_one_field() {
        let idx = index(vec![
            record("split", "/p", Some("telegram"), TitleKind::Ai, "deploy", 0),
            record("both", "/p", Some("telegram deploy"), TitleKind::Ai, "", 0),
        ]);
        let matches = search_metadata(&idx, &Query::parse("telegram deploy"), NOW);
        let ids: Vec<&str> = matches.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(ids, vec!["both"]);
    }

    #[test]
    fn limit_truncates_across_groups_in_rank_order() {
        let idx = index(vec![
            record("a1", "/a", Some("q"), TitleKind::Custom, "", 0),
            record("a2", "/a", Some("q"), TitleKind::Ai, "", 0),
            record("b1", "/b", Some("q"), TitleKind::Ai, "", 5),
        ]);
        let groups = group_matches(&idx, search_metadata(&idx, &Query::parse("q"), NOW), 2);
        assert_eq!(groups.len(), 1);
        let ids: Vec<&str> = groups[0]
            .sessions
            .iter()
            .map(|m| m.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a1", "a2"]);
    }

    #[test]
    fn deep_search_skips_sessions_already_matched_and_respects_deep_flag() {
        let temp = TempCache::new("deepflag");
        let cache = &temp.0;
        let mut both = record("both", "/p", Some("telegram"), TitleKind::Ai, "", 0);
        both.text_len = 1;
        cache
            .write_text(CLAUDE_AGENT, "both", &body(&[("user", "telegram again")]))
            .expect("write");
        let mut only_text = record("only", "/p", None, TitleKind::None, "hi", 0);
        only_text.text_len = 1;
        cache
            .write_text(CLAUDE_AGENT, "only", &body(&[("user", "Telegram in text")]))
            .expect("write");
        let idx = index(vec![both, only_text]);

        let shallow = search(
            &idx,
            cache,
            &Query::parse("telegram"),
            SearchOptions {
                deep: false,
                limit: 10,
            },
            NOW,
        );
        let ids: Vec<&str> = shallow[0]
            .sessions
            .iter()
            .map(|m| m.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["both"]);

        let deep = search(
            &idx,
            cache,
            &Query::parse("telegram"),
            SearchOptions::default(),
            NOW,
        );
        let ids: Vec<(&str, Option<MatchTier>)> = deep[0]
            .sessions
            .iter()
            .map(|m| (m.session_id.as_str(), m.tier))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("both", Some(MatchTier::Title)),
                ("only", Some(MatchTier::Text))
            ]
        );
        assert_eq!(
            deep[0].sessions[1]
                .snippet
                .as_ref()
                .map(|s| s.text.as_str()),
            Some("Telegram in text")
        );
    }

    #[test]
    fn snippets_are_windowed_and_case_preserving() {
        let long = format!("{} Telegram {}", "before ".repeat(30), "after ".repeat(30));
        let text = body(&[("user", "intro"), ("assistant", &long)]);
        let lower = text.to_lowercase();
        let snippet = snippet_for(&text, &lower, "telegram").expect("snippet");
        assert_eq!(snippet.role, "assistant");
        assert!(snippet.text.starts_with('…'));
        assert!(snippet.text.ends_with('…'));
        assert!(snippet.text.contains("Telegram"));
        assert!(snippet.text.chars().count() <= SNIPPET_CONTEXT_CHARS * 2 + 12);
    }

    #[test]
    fn cyrillic_queries_match_case_insensitively() {
        let temp = TempCache::new("cyrillic");
        let idx = index(vec![record(
            "ru",
            "/p",
            Some("Телеграм бот"),
            TitleKind::Custom,
            "",
            0,
        )]);
        let groups = search(
            &idx,
            &temp.0,
            &Query::parse("телеграм"),
            SearchOptions::default(),
            NOW,
        );
        assert_eq!(groups[0].sessions[0].tier, Some(MatchTier::Title));
    }

    #[test]
    fn scoring_helpers() {
        assert_eq!(recency_bonus(NOW, NOW), RECENCY_MAX_BONUS);
        assert_eq!(recency_bonus(NOW - RECENCY_WINDOW_MS, NOW), 0);
        assert_eq!(recency_bonus(NOW + DAY, NOW), RECENCY_MAX_BONUS);
        assert!(recency_bonus(NOW - 45 * DAY, NOW) < RECENCY_MAX_BONUS);
        assert!(contains_whole_word("deploy telegram bot", "telegram"));
        assert!(!contains_whole_word("telegrams", "telegram"));
        assert!(contains_whole_word("(telegram)", "telegram"));
    }
}
