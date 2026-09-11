//! Index of past coding-agent sessions found on disk (Claude Code transcripts today).
//!
//! The module is pure: it reads transcript files and a cache directory, and produces
//! [`SessionRecord`]s plus ranked, project-grouped search results. Wiring into the
//! server, the socket API, and the TUI overlay lives elsewhere.

mod claude;
mod messages;
mod model;
mod search;
mod store;
mod timestamp;

pub use messages::{session_messages, truncate_message, SessionMessage, MAX_PREVIEW_MESSAGE_CHARS};
pub use model::TitleKind;
pub use search::{search, MatchTier, Query, SearchOptions, Snippet, DEFAULT_RESULT_LIMIT};
pub use store::{scan_claude_projects, HistoryCache, Index, ScanOptions, ScanReport};
pub use timestamp::{format_date_ms, now_ms};

/// Default cache location: `<state dir>/agent-history`.
pub fn default_cache_root() -> std::path::PathBuf {
    crate::config::state_dir().join("agent-history")
}

/// Claude Code transcript root: `<claude config dir>/projects`.
pub fn claude_projects_dir() -> std::io::Result<std::path::PathBuf> {
    crate::integration::claude_dir().map(|dir| dir.join("projects"))
}

/// Scale profile against a real Claude Code `projects` directory. Ignored by default;
/// run with:
/// `cargo test --release --bin herdr agent_history_scale_profile -- --ignored --nocapture`
/// Optional env: `HERDR_AGENT_HISTORY_PROJECTS` (dir), `HERDR_AGENT_HISTORY_QUERY` (text).
#[cfg(test)]
mod profile {
    use std::time::Instant;

    use super::*;

    #[test]
    #[ignore]
    fn agent_history_scale_profile() {
        let projects = std::env::var_os("HERDR_AGENT_HISTORY_PROJECTS")
            .map(std::path::PathBuf::from)
            .or_else(|| claude_projects_dir().ok())
            .expect("projects dir");
        let query_text =
            std::env::var("HERDR_AGENT_HISTORY_QUERY").unwrap_or_else(|_| "telegram".into());
        let root = std::env::temp_dir().join(format!(
            "herdr-agent-history-profile-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let cache = HistoryCache::new(&root);
        let options = ScanOptions::default();

        let started = Instant::now();
        let (index, report) =
            scan_claude_projects(&projects, &cache, &Index::default(), &options, now_ms());
        let cold = started.elapsed();
        cache.save_index(&index).expect("save index");
        let text_bytes: u64 = index.sessions.iter().map(|record| record.text_len).sum();
        println!(
            "cold scan: {cold:?} sessions={} projects={} parsed={} failed={} text_cache={:.1} MB",
            index.len(),
            index.project_count(),
            report.parsed,
            report.failed,
            text_bytes as f64 / 1_048_576.0
        );

        let started = Instant::now();
        let loaded = cache.load_index();
        let (index, report) = scan_claude_projects(&projects, &cache, &loaded, &options, now_ms());
        println!(
            "warm rescan: {:?} reused={} parsed={}",
            started.elapsed(),
            report.reused,
            report.parsed
        );

        let query = Query::parse(&query_text);
        let started = Instant::now();
        let metadata = search::search_metadata(&index, &query, now_ms());
        println!(
            "metadata search {query_text:?}: {:?} matches={}",
            started.elapsed(),
            metadata.len()
        );

        let started = Instant::now();
        let groups = search(&index, &cache, &query, SearchOptions::default(), now_ms());
        let total: usize = groups.iter().map(|group| group.sessions.len()).sum();
        println!(
            "deep search {query_text:?}: {:?} groups={} sessions={}",
            started.elapsed(),
            groups.len(),
            total
        );
        for group in groups.iter().take(5) {
            println!("  {} ({} sessions)", group.label, group.sessions.len());
            for session in group.sessions.iter().take(3) {
                let record = index
                    .get(&session.agent, &session.session_id)
                    .expect("record");
                println!(
                    "    [{:?}] {} — {}",
                    session.tier,
                    record.display_title(),
                    session
                        .snippet
                        .as_ref()
                        .map(|snippet| snippet.text.as_str())
                        .unwrap_or("")
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
