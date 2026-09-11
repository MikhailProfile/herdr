# Agent History Search — implementation plan

Fork branch: `feature/agent-history-search` (based on upstream `master` @ 61ca85d, v0.9.0).
Inspiration: [codbash](https://github.com/vakovalskii/codbash) (Node, browser dashboard). We port the
idea into Herdr's native TUI: one overlay, one server-side index, one keybinding.

## 1. Gap analysis (what upstream has today)

| Need | Upstream status |
|---|---|
| Search past agent sessions by text | **Missing.** Session Navigator (`prefix+g`) searches only *live* workspaces/tabs/panes (`src/client/shell/aggregate_navigation.rs:101-245`), plain substring, no ranking. |
| Group results by project folder | Navigator groups by live workspace only. |
| Prefer title (`/rename`) matches over chat text | Nothing reads Claude transcripts. `~/.claude/projects/**` is never touched (verified). |
| Resume a session from a week ago | Only automatic restore after server restart (`[session] resume_agents_on_restore`), driven by session ids reported by the Claude hook. No user-facing "open session X" action, no API, no CLI. |
| Reusable pieces | `src/agent_resume.rs` (argv plan `claude --resume <id>`), `src/app/agent_resume.rs` (deferred launch through a real shell, alias-safe), `App::open_workspace_idx_for_checkout` (find workspace by cwd), `App::create_workspace_with_options`, `src/integration/env.rs::claude_dir()` (honours `CLAUDE_CONFIG_DIR`). |

Conclusion: build it. Nothing to enable, nothing hidden behind a flag.

## 2. Data source (Claude Code on disk, measured on this machine)

- `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` — 105 resumable sessions in 18 projects (the other 779 `.jsonl` files under `<sessionId>/subagents/` are subagent transcripts and are skipped); 752 MB total, largest 34 MB.
- Records we use (one JSON object per line, `type` field):
  - `custom-title` → `customTitle` — set by `/rename`. **Highest priority for title.** Last one wins.
  - `ai-title` → `aiTitle` — auto title. Second priority.
  - `summary` → `summary` — older builds; third priority.
  - first `user` record with `!isMeta` and string/text content → **first prompt** (skip `<command-name>` lines such as `/clear`, `/init`).
  - `user` / `assistant` → `message.content` text blocks → **body text** for deep search. Skip `tool_result`, `tool_use`, thinking. Skip `isSidechain: true`.
  - any record's `cwd` → project path (fallback: decode directory name, replacing `-` is lossy so prefer `cwd`); `gitBranch`; `timestamp` (ISO) → first/last activity.
- `sessions-index.json` exists only in 9 of 36 project dirs → optional hint, never a source of truth.
- Cost: a naive `grep -il` over the corpus is ~5 s → **interactive deep search needs a cache**, metadata alone is cheap.

## 3. Design

### 3.1 Placement (per AGENTS.md runtime/client guardrail)
- **Server (`App`)** owns the index: file scanning, parsing, caching, searching. Exposed through the socket API so the TUI, the CLI and other agents all get it. Neutral names (`agent_history.*`), no UI words.
- **Client (`src/client/shell`)** owns only the overlay: query string, selection, scroll, rendering. It calls the API and renders the returned rows. No filesystem I/O in the client, none in render/layout paths.

### 3.2 Index module `src/agent_history/` (pure, unit-testable, no App deps)
```
src/agent_history.rs            // mod + pub use
src/agent_history/model.rs      // SessionRecord { agent, session_id, project_path, project_label, title, title_kind: Custom|Ai|Summary|None,
                                //   first_prompt, git_branch, first_ts, last_ts, message_count, transcript_path, text_cache_path }
src/agent_history/claude.rs     // parse one .jsonl → SessionRecord + extracted body text (streaming line reader, serde_json::from_str per line, tolerant)
src/agent_history/store.rs      // Index { sessions: Vec<SessionRecord>, by_id: HashMap }, incremental scan by (path, mtime, len),
                                //   on-disk cache: <herdr config dir>/agent-history/index.json (metadata) + text/<session_id>.txt (body text)
src/agent_history/search.rs     // query → ranked, grouped results (see 3.3)
```
Cache dir: `crate::config::state_dir().join("agent-history")` (`~/.local/state/herdr/agent-history` on macOS/Linux). Metadata index stays in memory
(~1 KB per session); body text lives on disk and is memory-mapped/read only while a deep search runs.

### 3.3 Ranking (the user's rule, made explicit)
Query normalised: trim, lowercase, split on whitespace → all terms must match (AND).
Per session, tier = best of:
1. `Title` — custom title (`/rename`) hit. Also ai-title / summary hit (same tier, custom listed first on ties).
2. `Prompt` — first-prompt hit.
3. `Text` — body-text hit (deep search); carry a ±60-char snippet around the first hit and the role (user/assistant).
Score = tier weight (300 / 200 / 100) + recency bonus (0..50, decays over 90 days) + small bonus for whole-word match.
Grouping: results grouped by `project_path`; project order = best session score in the group, then latest activity;
inside a group: tier, then score, then `last_ts` desc. Empty query = every project expanded, sessions by recency, limit 200.
Fuzzy (trigram, codbash `SEARCH_THRESHOLD = 0.3`) is a phase-6 nicety, not v1.

Deep search is **two-stage** so typing stays snappy: stage 1 (metadata, sync, <1 ms) answers immediately; stage 2 (body text,
worker thread, debounced 250 ms after last keystroke) posts `AppEvent::AgentHistorySearched { query_seq, results }` and the
overlay merges when `query_seq` still matches.

### 3.4 Socket API (`src/api/schema/agent_history.rs`, re-exported from `schema.rs`)
| Method | Params | Result |
|---|---|---|
| `agent_history.search` | `{ query: String, limit?: u32, deep?: bool (default true), agents?: [String] }` | `{ type: "agent_history_results", query, complete: bool, groups: [{ project_path, project_label, workspace_id?: String, sessions: [{ agent, session_id, title, title_kind, first_prompt, git_branch, last_ts, message_count, match: {tier, snippet?, role?}, open_pane_id?: String }] }] }` |
| `agent_history.refresh` | `{}` | `{ type: "agent_history_status", sessions, projects, indexing: bool, last_scan_ts }` |
| `agent_history.status` | `{}` | same as above |
| `agent.resume` | `{ agent: "claude", session_id, cwd?: String, focus: bool (default true), placement: "tab" \| "workspace" }` | `{ type: "agent_resume_result", workspace_id, tab_id, pane_id, reused_workspace: bool, focused_existing: bool }` |
Events: `agent_history.updated { sessions, indexing }` (emitted after each scan; lets the overlay show "indexing… 412/885").

`agent.resume` behaviour:
1. If a live pane already holds this session (`terminal_agent_session_info` + `agent_resume::dedupe_key`) → focus it, return `focused_existing: true`.
2. `cwd` defaults to the session's `project_path`; if that dir no longer exists → error `project_missing`.
3. Workspace = `App::open_workspace_idx_for_checkout(cwd)` or `App::create_workspace_with_options(cwd, focus)`.
4. New tab in that workspace (`Workspace::create_tab`), label = session title (truncated 32), terminal gets
   `with_pending_agent_resume_plan(agent_resume::plan("herdr:claude","claude", AgentSessionRef::id(id)))` and
   `set_persisted_agent_session(..)`. The existing headless driver (`src/server/headless.rs:3375`) launches
   `claude --resume <id>` through the user's shell, so aliases/wrappers keep working and the pane survives restarts.
5. Extend `agent_resume::persisted_session_from_launch_args` to recognise `claude --resume <id>` (today codex-only) so
   `herdr agent start … -- --resume <id>` also self-persists.

Registration checklist (from AGENTS.md): `Method` enum + `#[serde(rename)]`, `api_method_name()` in `src/api/server.rs`,
handler arm in `src/app/api.rs`, `request_changes_ui()` for `agent.resume`, `CLIENT_SHELL_METHODS` in
`src/server/client_commands.rs` (sorted), append digests to `tests/fixtures/endpoint-method-shapes-v1.json`,
regenerate `docs/next/api/herdr-api.schema.json` via `HERDR_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current`,
round-trip tests in `src/api/schema/tests.rs`. No `PROTOCOL_VERSION` bump (additive only).

### 3.5 Background indexing in `App` (`src/app/agent_history.rs`)
Copy the `git_refresh.rs` pattern: `agent_history_in_flight` guard, `std::thread::spawn` for the blocking scan,
results returned by `event_tx.blocking_send(AppEvent::AgentHistoryIndexed { index })`. Triggers:
- server start (after session restore, low priority),
- `agent_history.refresh` / overlay open (rescan only files whose mtime/len changed → a few ms),
- Claude hook `SessionStart` report (`pane.report_agent_session`) → mark due,
- periodic every 10 min while a client is attached.
Never on the render path (`scripts/test_ui_hot_path_architecture.py` enforces this).

### 3.6 Client overlay `ClientShellOverlay::AgentHistory` (clone of Navigator)
Keybinding `keys.history = "prefix+f"` (free; `h/s/r` are taken). Files, mirroring the Navigator touch list:
- `src/config/model.rs` (+overlay field, doc comment `Default: "prefix+f"`), `src/config/keybinds.rs` (3 sites),
  `src/input/keybindings.rs` (`KeybindAction::OpenAgentHistory`), `src/input/keybind_help.rs`, `src/main.rs` DEFAULT_CONFIG,
  `docs/next/website/src/data/config-reference.json`.
- `src/client/shell/state.rs`: `ClientAgentHistoryOverlay { query, search_focused, selected: Option<AgentHistoryTarget>, scroll,
  expanded_projects, results: AgentHistoryResults (last server answer), request_seq, deep_pending }`, `ClientShellOverlayKind::AgentHistory`,
  hit rects `history_popup / history_search / history_rows`.
- `src/client/shell/agent_history.rs` (new): `open_agent_history_overlay`, `route_agent_history_key`, `accept_agent_history_selection`
  (→ `Method::AgentResume`), row derivation from `results` (project rows depth 0 with count + caret, session rows depth 1).
- `src/client/shell/overlay_input.rs`: dispatch branch, `insert_overlay_text` arm; `input.rs::modal_paste_target_active`.
- `src/client/shell/overlays.rs`: `render_agent_history_overlay` (panel, search line + "N sessions · M projects", rule, tree body,
  detail line = `title · project · branch · 3 days ago · snippet`, footer). Add fields to `OverlayRender`, the explicit struct
  literals, and `composition.rs:609-640`.
- `src/client/shell/mouse.rs`: hover-select, click row = open, click caret = toggle project, wheel, outside click closes.
- `src/client/shell/actions.rs::record_binding`: open on `OpenAgentHistory`.
Keys: search mode = type / backspace / ctrl+u / ↑↓ ctrl+n/p / esc leaves search; navigate mode = j k ↑ ↓ ctrl+d ctrl+u
space (expand project) `/` (search) enter (open as tab in project workspace) `w` (open in new workspace) `r` (refresh index) esc.
Row glyphs reuse `navigator_following_siblings` + `▾/▸`; match tier shown as a 1-char badge: `T` title, `P` prompt, `≈` text.

### 3.7 CLI (`src/cli/agent.rs`, spec mirror in `src/cli/spec.rs` + its tests)
```
herdr agent history [<query>] [--limit N] [--no-deep] [--json]
herdr agent resume <session-id> [--agent claude] [--cwd PATH] [--workspace] [--no-focus] [--json]
herdr agent history refresh
```
Also handy for other agents running inside Herdr ("find the session where we discussed X and resume it").

### 3.8 Config
```toml
[agent_history]
enabled = true            # index and expose past agent sessions
agents = ["claude"]       # v1: claude only; codex/pi later
deep_search = true        # index transcript text (disk cache under config dir)
max_age_days = 0          # 0 = unlimited
[keys]
# history = "prefix+f"
```
Diagnostics through `Config::collect_diagnostics()`; add to `DEFAULT_CONFIG`, `config-reference.json`, `configuration.mdx`.

## 4. Phases

| # | Phase | Deliverable | Verify |
|---|---|---|---|
| 0 | Toolchain + fork | rustup (toolchain pinned by `rust-toolchain.toml` = 1.96.1), **zig 0.16.0** (libghostty-vt build), cargo-nextest, just, bun (docs tests). `just build` green on untouched master. | `cargo build --release --locked`, `just lint` |
| 1 ✅ | Index core | `src/agent_history/*` with fixtures in `tests/fixtures/agent-history/` (small synthetic `.jsonl` with custom-title, ai-title, sidechain, tool_result noise, `/clear` first line). Ranking tests: title beats prompt beats text; grouping order; AND terms; incremental rescan by mtime. | `just test-one agent_history` |
| 2 ✅ | Server + API + CLI | `App` indexer thread, `AppEvent`s, `agent_history.search/status/refresh`, `herdr agent history`. Schema artifact regenerated. | `herdr agent history "telegram" --json` returns groups; `herdr api schema` lists methods |
| 3 ✅ | Resume | `agent.resume` + `herdr agent resume`, focus-existing dedupe, `persisted_session_from_launch_args` for claude. | resume a week-old session from CLI, pane survives `herdr server stop`/start |
| 4 ✅ | Overlay | `prefix+f` overlay end-to-end, mouse, help entry, config docs. Compose-buffer tests in `src/client/shell/tests/` modelled on `navigator_*`. | `just ci`, manual run in a named test session (`herdr --session hist-test`) |
| 5 ✅ | Polish | indexing progress line, snippet highlighting, relative dates, `w` open-in-new-workspace, empty-state text, keyboard.mdx / socket-api.mdx / cli-reference.mdx drafts (+ ja/zh-cn parity if we ever upstream). | `just check` |
| 6 | Later | trigram fuzzy (codbash), Codex (`~/.codex/sessions`) and Pi sources, session preview pane (last N messages) before resuming, star/pin. | — |

Estimated size: phases 1–4 ≈ 2.5–3.5k lines of Rust incl. tests, touching ~25 existing files plus ~8 new ones.

## 4a. Phase 1 result (2026-09-11)
Measured on this machine, release build, real `~/.claude/projects` (`cargo test --release --bin herdr agent_history_scale_profile -- --ignored --nocapture`):
cold scan 0.91 s (105 sessions, 8.4 MB text cache), warm rescan 3.5 ms, metadata search 72 µs, deep search 35 ms.
35 unit tests, clippy clean. Module is registered with `#[allow(dead_code, unused_imports)]` until phase 2 wires it in.

## 4b. Phase 2 result (2026-09-11)
- Config `[agent_history] enabled / deep_search / max_age_days` (model, defaults, reload, `DEFAULT_CONFIG`, config-reference.json, configuration.mdx).
- Server runtime `src/app/agent_history.rs`: index loaded from cache at start; background scan thread (git-refresh pattern) 3 s after the server starts serving, 20 s after any `pane.report_agent_session`, every 10 min, and on explicit refresh; results via `AppEvent::AgentHistoryIndexed`; deadline wired into the headless loop.
- Socket API `agent_history.search` / `agent_history.status` / `agent_history.refresh` (`src/api/schema/agent_history.rs`, handlers in `src/app/api/agent_history.rs`), advertised to the TUI client (`CLIENT_SHELL_METHODS` + digests in `tests/fixtures/endpoint-method-shapes-v1.json`), schema artifact regenerated, socket-api.mdx updated. Search runs synchronously on the App thread (35 ms deep on 106 sessions here).
- CLI `herdr agent history [<query>...] [--limit N] [--no-deep] [--json]` plus `refresh|status`, clap spec mirror + tests, cli-reference.mdx.
- Verified end-to-end against the real corpus with an isolated headless server (scratch `XDG_CONFIG_HOME`/`XDG_STATE_HOME`, short `/tmp` socket): 106 sessions / 18 projects indexed 5 s after start; grouped, tiered results with snippets.
- Full suite: 3222 passed; the 2 remaining failures (`live_handoff_keeps_agent_started_pane_after_agent_exits`, `pane_info_and_subscriptions_expose_done_agent_status`) fail identically on pristine upstream master on this Mac (fake `pi` agent timing), so they are environmental.
- Deferred: `agent_history.updated` event (overlay can poll `status`), ja/zh-cn doc translations (only needed for a release).

## 4c. Phase 3 result (2026-09-11)
- `agent.resume` (`src/api/schema/agent_resume.rs`, handler `src/app/api/agent_resume.rs`): dedupe against panes with a *running* agent owning the session (remembered-but-exited sessions do not count), cwd from the index (`workspace_path`, worktree-aware) or explicit `cwd`, `placement: tab|workspace`, tab label = session title, `persisted_agent_session` set so the pane survives restarts, events + session save. Command is typed into a fresh shell exactly like restore (`shell_command_from_argv`), so aliases/wrappers apply.
- `agent_resume::persisted_session_from_launch_args` now recognises `claude --resume <id>` (so `herdr agent start … -- --resume <id>` self-persists); `claude_resume_plan` helper.
- CLI `herdr agent resume <id> [--agent claude] [--cwd PATH] [--workspace] [--no-focus] [--json]`; spec + tests; docs (socket-api.mdx, cli-reference.mdx). Advertised to the TUI (`agent.resume` in `CLIENT_SHELL_METHODS`, digest added, schema regenerated).
- Search results' `open_pane_id` now also requires a running agent (uses `session_snapshot().agents`).
- Verified e2e on an isolated headless server: resume → new workspace for the project → real Claude Code launched with `--resume` and rendered the old transcript; second resume → new tab (no agent detected for the fake); `--workspace` → second workspace; error codes for wrong agent, unknown session, missing project. Note: the pane's login shell rebuilds PATH, so a fake `claude` on the server's PATH is bypassed and the real binary ran (harmless, stopped with the server).

## 4d. Phase 4 + 5 result (2026-09-11)
- Overlay `prefix+f` (`keys.history`): `src/client/shell/agent_history.rs` (state machine, key routing, debounced search 200 ms via `history_search_deadline` + `tick_agent_history_search`, generation-tagged responses so stale answers are dropped, resume on Enter / `w` for a new workspace / space to collapse a project / `r` to rescan), renderer `src/client/shell/agent_history_overlay.rs` (clone of the Navigator geometry: search line, summary, project tree with `T`/`P`/`~` tier badges, `◆` for open workspaces/panes, detail line with snippet or first prompt, error line, footer), mouse support, hit rects, paste target, help entry, `DEFAULT_CONFIG`, config-reference.json, keyboard.mdx, configuration.mdx.
- Client tests `src/client/shell/tests/agent_history.rs` (open → initial request, render, debounce + stale-result drop, resume success/error, collapse + mouse click + outside click); keybinding default test.
- Verified live inside Herdr: fork server + fork client in sibling panes; `prefix+f` listed 106 sessions / 18 projects, "telegram" produced 21 sessions / 7 projects with the Messenger API project first (two title hits, then prompt hits), Enter opened a new workspace + tab titled after the session with Claude Code resumed inside.
- Polish: the temporary `#[allow(dead_code, unused_imports)]` on `mod agent_history` is gone; re-exports trimmed. Full suite 3235 passed (the 2 known environmental failures remain).
- Left for later (not blocking): `agent_history.status` polling in the overlay for an "indexing…" indicator, relative dates, snippet highlighting, Codex/Pi sources, ja/zh-cn doc translations, monolithic `--no-session` mode never rescans (only loads the cached index; same as git refresh upstream).

## 4e. Conversation preview (2026-09-11, follow-up request)
- `agent_history.messages { agent, session_id, offset?, limit? }` → `{ conversation: { title, project_path, total, offset, truncated, messages: [{ role, text }] } }`; text from the cache (`src/agent_history/messages.rs`), falling back to parsing the transcript when text caching is off; 400 messages / 8000 chars per message caps.
- Overlay: space (or `l` / →) on a session opens the preview inside the overlay — `YOU` / `ASSISTANT` cards, word-wrapped, oldest first; j/k, ctrl+d/u, g/G scroll; Enter / `w` resume from the preview; Esc / space / `h` / ← back to the list. Renderer + wrap tests in `src/client/shell/agent_history_overlay.rs`, client tests in `tests/agent_history.rs`.
- CLI `herdr agent history show <session-id> [--offset N] [--limit N] [--json]`.

## 5. Risks / decisions already taken
- **Memory**: metadata only in RAM; body text on disk. 885 sessions ≈ 1 MB RAM. Deep search reads ~150 MB of cached text
  per query on a worker thread (<300 ms on this machine, measured grep baseline 5 s on raw jsonl).
- **Privacy**: text cache lives under the Herdr state dir with `0600` perms; `deep_search = false` disables it.
  Same concern upstream had with `pane_history` — mirror their wording in docs.
- **Claude `--resume` and cwd**: Claude Code resolves the id inside the project dir, so we always launch in `project_path`.
  Worktree sessions: record `worktree-state.originalCwd` when present (codbash does), prefer it for the workspace match.
- **Upstream contribution**: CONTRIBUTING.md auto-closes unsolicited feature PRs; this stays a fork unless a maintainer issue is opened first.
  Keep commits conventional (`feat: …`, no co-author lines) so upstreaming stays possible.
- **`gh` is not installed** here; the GitHub fork (remote `origin`) is created manually or after `brew install gh && gh repo fork`.
