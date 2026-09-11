//! On-disk cache and incremental scanning for the agent history index.
//!
//! Layout under the cache root:
//! - `index.json` — every [`SessionRecord`] (small; kept in memory by the server).
//! - `text/<agent>/<session id>.txt` — cached body text used by deep search.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::claude::parse_transcript;
use super::model::{is_session_file_name, project_path_from_dir_name, SessionRecord, CLAUDE_AGENT};
use super::timestamp::system_time_ms;

const INDEX_VERSION: u32 = 1;
const INDEX_FILE: &str = "index.json";
const TEXT_DIR: &str = "text";

/// All known sessions plus when they were last scanned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub version: u32,
    pub scanned_at_ms: i64,
    pub sessions: Vec<SessionRecord>,
}

impl Index {
    pub fn get(&self, agent: &str, session_id: &str) -> Option<&SessionRecord> {
        self.sessions
            .iter()
            .find(|record| record.agent == agent && record.session_id == session_id)
    }

    // Kept alongside `len` for the usual collection convention; only tests call it.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Distinct project paths in the index.
    pub fn project_count(&self) -> usize {
        self.sessions
            .iter()
            .map(|record| record.project_path.as_str())
            .collect::<HashSet<_>>()
            .len()
    }

    fn by_transcript_path(&self) -> HashMap<&Path, &SessionRecord> {
        self.sessions
            .iter()
            .map(|record| (record.transcript_path.as_path(), record))
            .collect()
    }
}

/// Scan tuning; mirrors the `[agent_history]` config section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// Cache body text for deep search.
    pub deep_text: bool,
    /// Skip transcripts last modified more than this many days ago; zero disables.
    pub max_age_days: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            deep_text: true,
            max_age_days: 0,
        }
    }
}

/// Counters describing one scan pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub parsed: usize,
    pub reused: usize,
    pub removed: usize,
    pub failed: usize,
    pub skipped_old: usize,
}

/// Cache directory handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryCache {
    root: PathBuf,
}

impl HistoryCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    fn text_path(&self, agent: &str, session_id: &str) -> PathBuf {
        self.root
            .join(TEXT_DIR)
            .join(agent)
            .join(format!("{session_id}.txt"))
    }

    /// Loads the persisted index; a missing, unreadable, or incompatible file yields
    /// an empty index so the next scan rebuilds it.
    pub fn load_index(&self) -> Index {
        let path = self.index_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Index::default(),
            Err(err) => {
                warn!(path = %path.display(), error = %err, "agent history index unreadable");
                return Index::default();
            }
        };
        match serde_json::from_slice::<Index>(&bytes) {
            Ok(index) if index.version == INDEX_VERSION => index,
            Ok(index) => {
                debug!(
                    version = index.version,
                    "agent history index version mismatch; rebuilding"
                );
                Index::default()
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "agent history index corrupt; rebuilding");
                Index::default()
            }
        }
    }

    pub fn save_index(&self, index: &Index) -> io::Result<()> {
        let bytes = serde_json::to_vec(index).map_err(io::Error::other)?;
        write_private_atomic(&self.index_path(), &bytes)
    }

    pub fn write_text(&self, agent: &str, session_id: &str, text: &str) -> io::Result<()> {
        write_private_atomic(&self.text_path(agent, session_id), text.as_bytes())
    }

    pub fn read_text(&self, agent: &str, session_id: &str) -> io::Result<String> {
        fs::read_to_string(self.text_path(agent, session_id))
    }

    pub fn has_text(&self, agent: &str, session_id: &str) -> bool {
        self.text_path(agent, session_id).is_file()
    }

    pub fn remove_text(&self, agent: &str, session_id: &str) {
        let path = self.text_path(agent, session_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => warn!(path = %path.display(), error = %err, "failed to remove cached text"),
        }
    }
}

/// Scans `projects_dir` (Claude Code's `projects` folder) and returns a fresh index,
/// reusing unchanged entries from `previous` and their cached text.
pub fn scan_claude_projects(
    projects_dir: &Path,
    cache: &HistoryCache,
    previous: &Index,
    options: &ScanOptions,
    now_ms: i64,
) -> (Index, ScanReport) {
    let mut report = ScanReport::default();
    let previous_by_path = previous.by_transcript_path();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut sessions = Vec::with_capacity(previous.sessions.len());
    let min_mtime_ms = if options.max_age_days == 0 {
        i64::MIN
    } else {
        now_ms - i64::from(options.max_age_days) * 86_400_000
    };

    for (dir_name, dir_path) in project_dirs(projects_dir) {
        let project_hint = project_path_from_dir_name(&dir_name);
        for (session_id, path) in session_files(&dir_path) {
            let Ok(metadata) = fs::metadata(&path) else {
                report.failed += 1;
                continue;
            };
            let file_mtime_ms = metadata.modified().map(system_time_ms).unwrap_or(0);
            if file_mtime_ms < min_mtime_ms {
                report.skipped_old += 1;
                continue;
            }
            seen.insert(path.clone());

            if let Some(existing) = previous_by_path.get(path.as_path()) {
                let unchanged =
                    existing.file_len == metadata.len() && existing.file_mtime_ms == file_mtime_ms;
                let text_ok = !options.deep_text
                    || existing.text_len == 0
                    || cache.has_text(&existing.agent, &existing.session_id);
                if unchanged && text_ok {
                    report.reused += 1;
                    sessions.push((*existing).clone());
                    continue;
                }
            }

            match parse_transcript(&path, &session_id, project_hint.as_deref()) {
                Ok(mut parsed) => {
                    if options.deep_text && !parsed.body_text.is_empty() {
                        if let Err(err) =
                            cache.write_text(CLAUDE_AGENT, &session_id, &parsed.body_text)
                        {
                            warn!(session = %session_id, error = %err, "failed to cache session text");
                            parsed.record.text_len = 0;
                        }
                    } else {
                        parsed.record.text_len = 0;
                        cache.remove_text(CLAUDE_AGENT, &session_id);
                    }
                    report.parsed += 1;
                    sessions.push(parsed.record);
                }
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "failed to parse transcript");
                    report.failed += 1;
                }
            }
        }
    }

    for record in &previous.sessions {
        if !seen.contains(&record.transcript_path) {
            report.removed += 1;
            cache.remove_text(&record.agent, &record.session_id);
        }
    }

    sessions.sort_by(|a, b| {
        b.last_ts_ms
            .cmp(&a.last_ts_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    (
        Index {
            version: INDEX_VERSION,
            scanned_at_ms: now_ms,
            sessions,
        },
        report,
    )
}

fn project_dirs(projects_dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(projects_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect();
    dirs.sort();
    dirs
}

fn session_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            let name = entry.file_name();
            let session_id = is_session_file_name(name.to_str()?)?.to_string();
            Some((session_id, entry.path()))
        })
        .collect();
    files.sort();
    files
}

/// Writes `bytes` to `path` through a temporary sibling and rename, owner-only on Unix.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options.open(&temp_path).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_data()
    });
    if let Err(err) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPHA_ONE: &str = "11111111-1111-4111-8111-111111111111";
    const ALPHA_TWO: &str = "22222222-2222-4222-8222-222222222222";
    const BETA_ONE: &str = "33333333-3333-4333-8333-333333333333";

    fn fixture_projects() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent-history/claude/projects")
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-agent-history-store-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn copy_fixture_projects(into: &Path) {
        fn copy_tree(from: &Path, to: &Path) {
            fs::create_dir_all(to).expect("mkdir");
            for entry in fs::read_dir(from).expect("read fixture dir") {
                let entry = entry.expect("entry");
                let target = to.join(entry.file_name());
                if entry.file_type().expect("file type").is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    fs::copy(entry.path(), &target).expect("copy fixture");
                }
            }
        }
        copy_tree(&fixture_projects(), into);
    }

    fn ids(index: &Index) -> Vec<&str> {
        let mut ids: Vec<&str> = index
            .sessions
            .iter()
            .map(|record| record.session_id.as_str())
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn scans_fixture_projects_and_ignores_non_session_files() {
        let temp = TempDir::new("scan");
        let cache = HistoryCache::new(temp.path().join("cache"));
        let (index, report) = scan_claude_projects(
            &fixture_projects(),
            &cache,
            &Index::default(),
            &ScanOptions::default(),
            1_800_000_000_000,
        );
        assert_eq!(ids(&index), vec![ALPHA_ONE, ALPHA_TWO, BETA_ONE]);
        assert_eq!(report.parsed, 3);
        assert_eq!(report.reused, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(index.project_count(), 2);
        assert_eq!(index.version, INDEX_VERSION);
        // Sorted by last activity, newest first.
        assert!(index
            .sessions
            .windows(2)
            .all(|pair| pair[0].last_ts_ms >= pair[1].last_ts_ms));
        assert!(cache.has_text(CLAUDE_AGENT, ALPHA_ONE));
        let text = cache
            .read_text(CLAUDE_AGENT, BETA_ONE)
            .expect("cached text");
        assert!(text.contains("zoom recording"));
        let beta = index.get(CLAUDE_AGENT, BETA_ONE).expect("beta record");
        assert_eq!(beta.text_len, text.len() as u64);
    }

    #[test]
    fn rescan_reuses_unchanged_reparses_modified_and_removes_deleted() {
        let temp = TempDir::new("rescan");
        let projects = temp.path().join("projects");
        copy_fixture_projects(&projects);
        let cache = HistoryCache::new(temp.path().join("cache"));
        let options = ScanOptions::default();

        let (first, _) = scan_claude_projects(
            &projects,
            &cache,
            &Index::default(),
            &options,
            1_800_000_000_000,
        );
        cache.save_index(&first).expect("save index");
        let loaded = cache.load_index();
        assert_eq!(loaded, first);

        let (second, report) =
            scan_claude_projects(&projects, &cache, &loaded, &options, 1_800_000_000_001);
        assert_eq!(report.reused, 3);
        assert_eq!(report.parsed, 0);
        assert_eq!(ids(&second), ids(&first));

        // Modify one transcript: append a rename record with a new title.
        let alpha_two = projects
            .join("-Users-demo-alpha")
            .join(format!("{ALPHA_TWO}.jsonl"));
        let mut content = fs::read_to_string(&alpha_two).expect("read");
        content.push_str(
            "{\"type\":\"custom-title\",\"customTitle\":\"renamed later\",\"sessionId\":\"x\"}\n",
        );
        fs::write(&alpha_two, content).expect("modify");
        // Delete another transcript entirely.
        fs::remove_file(
            projects
                .join("-Users-demo-beta")
                .join(format!("{BETA_ONE}.jsonl")),
        )
        .expect("delete");

        let (third, report) =
            scan_claude_projects(&projects, &cache, &second, &options, 1_800_000_000_002);
        assert_eq!(report.parsed, 1);
        assert_eq!(report.reused, 1);
        assert_eq!(report.removed, 1);
        assert_eq!(ids(&third), vec![ALPHA_ONE, ALPHA_TWO]);
        assert_eq!(
            third
                .get(CLAUDE_AGENT, ALPHA_TWO)
                .and_then(|record| record.title.as_deref()),
            Some("renamed later")
        );
        assert!(!cache.has_text(CLAUDE_AGENT, BETA_ONE));
    }

    #[test]
    fn missing_text_cache_forces_reparse_when_deep_search_is_on() {
        let temp = TempDir::new("textmissing");
        let cache = HistoryCache::new(temp.path().join("cache"));
        let options = ScanOptions::default();
        let (first, _) = scan_claude_projects(
            &fixture_projects(),
            &cache,
            &Index::default(),
            &options,
            1_800_000_000_000,
        );
        cache.remove_text(CLAUDE_AGENT, ALPHA_ONE);
        let (_, report) = scan_claude_projects(
            &fixture_projects(),
            &cache,
            &first,
            &options,
            1_800_000_000_001,
        );
        assert_eq!(report.parsed, 1);
        assert_eq!(report.reused, 2);
        assert!(cache.has_text(CLAUDE_AGENT, ALPHA_ONE));
    }

    #[test]
    fn deep_text_off_skips_text_cache_and_reuses_metadata() {
        let temp = TempDir::new("shallow");
        let cache = HistoryCache::new(temp.path().join("cache"));
        let options = ScanOptions {
            deep_text: false,
            max_age_days: 0,
        };
        let (index, _) = scan_claude_projects(
            &fixture_projects(),
            &cache,
            &Index::default(),
            &options,
            1_800_000_000_000,
        );
        assert!(index.sessions.iter().all(|record| record.text_len == 0));
        assert!(!cache.has_text(CLAUDE_AGENT, ALPHA_ONE));
        let (_, report) = scan_claude_projects(
            &fixture_projects(),
            &cache,
            &index,
            &options,
            1_800_000_000_001,
        );
        assert_eq!(report.reused, 3);
    }

    #[test]
    fn max_age_filters_old_transcripts_by_mtime() {
        let temp = TempDir::new("age");
        let cache = HistoryCache::new(temp.path().join("cache"));
        let options = ScanOptions {
            deep_text: true,
            max_age_days: 1,
        };
        // Fixture files were written long before "now" in the far future.
        let far_future_ms = 4_000_000_000_000;
        let (index, report) = scan_claude_projects(
            &fixture_projects(),
            &cache,
            &Index::default(),
            &options,
            far_future_ms,
        );
        assert!(index.is_empty());
        assert_eq!(report.skipped_old, 3);
    }

    #[test]
    fn corrupt_or_missing_index_loads_as_empty() {
        let temp = TempDir::new("corrupt");
        let cache = HistoryCache::new(temp.path().join("cache"));
        assert!(cache.load_index().is_empty());
        fs::create_dir_all(cache.root()).expect("mkdir");
        fs::write(cache.root().join(INDEX_FILE), b"{not json").expect("write");
        assert!(cache.load_index().is_empty());
        let old = Index {
            version: 0,
            scanned_at_ms: 1,
            sessions: Vec::new(),
        };
        cache.save_index(&old).expect("save");
        assert!(cache.load_index().is_empty());
    }

    #[test]
    fn missing_projects_dir_yields_empty_index() {
        let temp = TempDir::new("noprojects");
        let cache = HistoryCache::new(temp.path().join("cache"));
        let (index, report) = scan_claude_projects(
            &temp.path().join("does-not-exist"),
            &cache,
            &Index::default(),
            &ScanOptions::default(),
            1,
        );
        assert!(index.is_empty());
        assert_eq!(report, ScanReport::default());
    }

    #[cfg(unix)]
    #[test]
    fn cache_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new("perms");
        let cache = HistoryCache::new(temp.path().join("cache"));
        cache
            .write_text(CLAUDE_AGENT, "abc", "secret")
            .expect("write");
        cache.save_index(&Index::default()).expect("save");
        for path in [cache.text_path(CLAUDE_AGENT, "abc"), cache.index_path()] {
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
    }
}
