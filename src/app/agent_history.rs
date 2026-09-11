//! Server-side runtime for the agent history index: background scans of agent
//! transcript directories, scheduled like git status refreshes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use super::App;
use crate::agent_history::{self, HistoryCache, Index, ScanOptions, ScanReport};
use crate::events::AppEvent;

/// Delay before the first scan after server start, so restore work goes first.
const STARTUP_SCAN_DELAY: Duration = Duration::from_secs(3);
/// Interval between periodic incremental rescans.
const PERIODIC_SCAN_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Delay after an agent reports a new session before picking up its transcript.
const SESSION_REPORT_SCAN_DELAY: Duration = Duration::from_secs(20);

pub(crate) struct AgentHistoryRuntime {
    pub(crate) enabled: bool,
    pub(crate) scan_options: ScanOptions,
    pub(crate) cache: HistoryCache,
    /// Claude Code `projects` directory; `None` when the home directory is unknown.
    pub(crate) projects_dir: Option<PathBuf>,
    pub(crate) index: Index,
    pub(crate) in_flight: bool,
    pub(crate) rescan_after_in_flight: bool,
    pub(crate) next_scan_at: Option<Instant>,
    pub(crate) last_report: ScanReport,
}

impl AgentHistoryRuntime {
    pub(crate) fn from_config(config: &crate::config::AgentHistoryConfig) -> Self {
        let cache = HistoryCache::new(agent_history::default_cache_root());
        let projects_dir = match agent_history::claude_projects_dir() {
            Ok(dir) => Some(dir),
            Err(err) => {
                warn!(error = %err, "agent history: cannot resolve Claude Code projects dir");
                None
            }
        };
        let index = if config.enabled {
            cache.load_index()
        } else {
            Index::default()
        };
        Self {
            enabled: config.enabled,
            scan_options: scan_options_from_config(config),
            cache,
            projects_dir,
            index,
            in_flight: false,
            rescan_after_in_flight: false,
            next_scan_at: None,
            last_report: ScanReport::default(),
        }
    }

    /// When the next scan should start, if one is scheduled and none is running.
    pub(crate) fn scan_deadline(&self) -> Option<Instant> {
        if !self.enabled || self.in_flight || self.projects_dir.is_none() {
            return None;
        }
        self.next_scan_at
    }

    /// Schedules a scan no later than `at`; a scan already running is followed by
    /// another one immediately.
    pub(crate) fn schedule_scan(&mut self, at: Instant) {
        if !self.enabled {
            return;
        }
        if self.in_flight {
            self.rescan_after_in_flight = true;
            return;
        }
        self.next_scan_at = Some(match self.next_scan_at {
            Some(current) => current.min(at),
            None => at,
        });
    }

    pub(crate) fn apply_config(
        &mut self,
        config: &crate::config::AgentHistoryConfig,
        now: Instant,
    ) {
        let options = scan_options_from_config(config);
        let was_enabled = self.enabled;
        self.enabled = config.enabled;
        if !self.enabled {
            self.next_scan_at = None;
            self.rescan_after_in_flight = false;
            return;
        }
        let options_changed = options != self.scan_options;
        self.scan_options = options;
        if !was_enabled || options_changed {
            self.schedule_scan(now);
        }
    }

    pub(crate) fn last_scan_ms(&self) -> i64 {
        self.index.scanned_at_ms
    }
}

fn scan_options_from_config(config: &crate::config::AgentHistoryConfig) -> ScanOptions {
    ScanOptions {
        deep_text: config.deep_search,
        max_age_days: config.max_age_days,
    }
}

impl App {
    pub(crate) fn agent_history_scan_deadline(&self) -> Option<Instant> {
        self.agent_history.scan_deadline()
    }

    /// Starts a background scan when its deadline has passed. Results arrive as
    /// [`AppEvent::AgentHistoryIndexed`].
    pub(crate) fn start_agent_history_scan_if_due(&mut self, now: Instant) {
        let Some(deadline) = self.agent_history.scan_deadline() else {
            return;
        };
        if now < deadline {
            return;
        }
        let Some(projects_dir) = self.agent_history.projects_dir.clone() else {
            self.agent_history.next_scan_at = None;
            return;
        };

        self.agent_history.in_flight = true;
        self.agent_history.next_scan_at = None;
        let cache = self.agent_history.cache.clone();
        let previous = self.agent_history.index.clone();
        let options = self.agent_history.scan_options.clone();
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let (index, report) = agent_history::scan_claude_projects(
                &projects_dir,
                &cache,
                &previous,
                &options,
                agent_history::now_ms(),
            );
            if let Err(err) = cache.save_index(&index) {
                warn!(error = %err, "agent history: failed to save index");
            }
            let _ = event_tx.blocking_send(AppEvent::AgentHistoryIndexed {
                index: Box::new(index),
                report,
            });
        });
    }

    pub(crate) fn handle_agent_history_indexed(&mut self, index: Index, report: ScanReport) {
        debug!(
            sessions = index.len(),
            parsed = report.parsed,
            reused = report.reused,
            removed = report.removed,
            failed = report.failed,
            "agent history: scan finished"
        );
        self.agent_history.in_flight = false;
        self.agent_history.last_report = report;
        self.agent_history.index = index;
        let now = Instant::now();
        if self.agent_history.rescan_after_in_flight {
            self.agent_history.rescan_after_in_flight = false;
            self.agent_history.schedule_scan(now);
        } else {
            self.agent_history
                .schedule_scan(now + PERIODIC_SCAN_INTERVAL);
        }
    }

    /// Schedules the first scan shortly after the server starts serving, so session
    /// restore and client attach are not delayed by transcript parsing.
    pub(crate) fn schedule_agent_history_startup_scan(&mut self, now: Instant) {
        self.agent_history.schedule_scan(now + STARTUP_SCAN_DELAY);
    }

    /// Requests a scan as soon as possible (explicit refresh).
    pub(crate) fn request_agent_history_scan(&mut self, now: Instant) {
        self.agent_history.schedule_scan(now);
    }

    /// An agent just reported a native session id; pick up its transcript soon.
    pub(crate) fn note_agent_session_reported_for_history(&mut self) {
        let at = Instant::now() + SESSION_REPORT_SCAN_DELAY;
        self.agent_history.schedule_scan(at);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use super::*;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    fn fixture_projects() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent-history/claude/projects")
    }

    fn temp_cache(name: &str) -> HistoryCache {
        let root = std::env::temp_dir().join(format!(
            "herdr-app-agent-history-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        HistoryCache::new(root)
    }

    pub(crate) fn point_at_fixtures(app: &mut App, name: &str) {
        app.agent_history.projects_dir = Some(fixture_projects());
        app.agent_history.cache = temp_cache(name);
        app.agent_history.index = Index::default();
        app.agent_history.enabled = true;
        app.agent_history.next_scan_at = Some(Instant::now());
    }

    /// Runs one scan to completion on the calling thread's event loop.
    pub(crate) fn scan_now(app: &mut App) {
        app.start_agent_history_scan_if_due(Instant::now());
        assert!(app.agent_history.in_flight, "scan should be in flight");
        let event = app.event_rx.blocking_recv().expect("scan completion event");
        assert!(matches!(event, AppEvent::AgentHistoryIndexed { .. }));
        app.handle_internal_event_with_render_impact(event);
        assert!(!app.agent_history.in_flight);
    }

    #[test]
    fn startup_scan_is_scheduled_by_the_server_only_when_enabled() {
        let mut app = test_app();
        assert!(app.agent_history.enabled);
        assert_eq!(app.agent_history_scan_deadline(), None);
        let now = Instant::now();
        app.schedule_agent_history_startup_scan(now);
        assert_eq!(
            app.agent_history_scan_deadline(),
            Some(now + STARTUP_SCAN_DELAY)
        );

        let mut disabled = crate::config::Config::default();
        disabled.agent_history.enabled = false;
        let mut app = App::new(
            &disabled,
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.schedule_agent_history_startup_scan(now);
        assert_eq!(app.agent_history_scan_deadline(), None);
    }

    #[test]
    fn scan_indexes_fixtures_and_schedules_periodic_rescan() {
        let mut app = test_app();
        point_at_fixtures(&mut app, "scan");
        scan_now(&mut app);
        assert_eq!(app.agent_history.index.len(), 3);
        assert_eq!(app.agent_history.last_report.parsed, 3);
        assert!(app.agent_history.last_scan_ms() > 0);
        let next = app.agent_history_scan_deadline().expect("periodic rescan");
        assert!(next > Instant::now() + Duration::from_secs(60));
        let _ = std::fs::remove_dir_all(app.agent_history.cache.root());
    }

    #[test]
    fn refresh_during_scan_queues_another_scan() {
        let mut app = test_app();
        point_at_fixtures(&mut app, "requeue");
        app.start_agent_history_scan_if_due(Instant::now());
        app.request_agent_history_scan(Instant::now());
        assert!(app.agent_history.rescan_after_in_flight);
        let event = app.event_rx.blocking_recv().expect("scan event");
        app.handle_internal_event_with_render_impact(event);
        let next = app.agent_history_scan_deadline().expect("immediate rescan");
        assert!(next <= Instant::now());
        let _ = std::fs::remove_dir_all(app.agent_history.cache.root());
    }

    #[test]
    fn config_reload_toggles_and_reschedules() {
        let mut app = test_app();
        let now = Instant::now();
        let mut config = crate::config::AgentHistoryConfig {
            enabled: false,
            ..Default::default()
        };
        app.agent_history.apply_config(&config, now);
        assert_eq!(app.agent_history_scan_deadline(), None);
        assert!(!app.agent_history.enabled);

        config.enabled = true;
        config.max_age_days = 7;
        app.agent_history.apply_config(&config, now);
        assert!(app.agent_history.enabled);
        assert_eq!(app.agent_history.scan_options.max_age_days, 7);
        assert_eq!(app.agent_history_scan_deadline(), Some(now));
    }

    #[test]
    fn missing_projects_dir_never_schedules() {
        let mut app = test_app();
        app.agent_history.projects_dir = None;
        app.agent_history.next_scan_at = Some(Instant::now());
        assert_eq!(app.agent_history_scan_deadline(), None);
        app.start_agent_history_scan_if_due(Instant::now());
        assert!(!app.agent_history.in_flight);
    }
}
