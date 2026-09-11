# herdr fork: agent history search

This is [Herdr](https://herdr.dev) (Apache-2.0, upstream at `herdrdev/herdr`) plus one feature:
**search and resume past Claude Code sessions from inside Herdr.**

- `prefix+f` opens the history overlay. Type to search; results are grouped by project and
  ranked with title matches first (`T`, including `/rename` titles), then first-prompt matches (`P`),
  then transcript text (`~`, with a snippet). Enter resumes the session as a new tab in the matching
  workspace, `w` opens it in a new workspace, space or `→` previews the conversation, `r` rescans.
- CLI: `herdr agent history [query]`, `herdr agent history show <session-id>`, `herdr agent resume <session-id>`.
- Socket API: `agent_history.search`, `agent_history.messages`, `agent_history.status`,
  `agent_history.refresh`, `agent.resume` — usable by agents running inside Herdr.
- Config: `[agent_history] enabled / deep_search / max_age_days`, keybinding `keys.history`.

Details: `PLAN-agent-history-search.md`.

## Install (macOS, prebuilt binary)

Download the asset for your CPU from the latest release of this repository
(`herdr-macos-aarch64` for Apple Silicon, `herdr-macos-x86_64` for Intel), then:

```bash
mkdir -p ~/.local/bin
mv ~/Downloads/herdr-macos-aarch64 ~/.local/bin/herdr
chmod +x ~/.local/bin/herdr
xattr -d com.apple.quarantine ~/.local/bin/herdr 2>/dev/null || true   # unsigned binary
```

Make sure `~/.local/bin` comes before `/opt/homebrew/bin` in your `PATH` (or uninstall the
Homebrew `herdr`). Then:

```bash
herdr integration install claude   # hooks that report Claude session ids, needed for resume
herdr                              # starts the server on demand and attaches
```

Sessions are indexed from `~/.claude/projects` a few seconds after the server starts and
rescanned in the background; transcript text is cached under `~/.local/state/herdr/agent-history`
(owner-only files; set `deep_search = false` to keep only titles and first prompts).

## Rules that save you an afternoon

- **Start Herdr from a terminal, never as a launchd service** (`brew services start herdr`).
  macOS attributes privacy permissions to the terminal app; a launchd-started server has none,
  and every pane it spawns gets "Operation not permitted" on `~/Documents`.
- **Never run `herdr update`**: it downloads the upstream binary over this fork.
- If you replace the Homebrew binary in place instead of using `~/.local/bin`, run `brew pin herdr`.

## Build from source

Requires Rust (pinned by `rust-toolchain.toml`), Zig 0.16.0, and about three minutes:

```bash
git clone <this repository> herdr && cd herdr
cargo build --release --locked
./target/release/herdr --version
```
