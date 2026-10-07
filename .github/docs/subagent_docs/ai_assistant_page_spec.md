# AI Assistant page — spec

## Current state

- `agent` and `diagnose` sit in an "AI Assistant" group on the Features page
  (`catalog.toml`, `page = "features"`). `src/ui/pages/features.rs` hides that group
  unless `vexos.features.ai.enable` is on in features.nix.
- The sidebar is built once in `Window::new` from `App::visible_pages()`; `ids` is a
  fixed `Rc<Vec<String>>`. A finished job calls `App::refresh_state` and
  `Window::state_changed`, which rebuilds only the visible page, not the sidebar.
- `vexos-ai` (github:VictoryTek/vexos-ai, `bin/vexos-ai.sh`) keeps its state in user files:
  - `~/.config/vexos/ai/agent`: `claude` | `opencode` (first line)
  - `~/.config/vexos/ai/claude-account`: active label; `main` when it is absent or names no account dir
  - `~/.config/vexos/ai/mode`: `auto`, otherwise manual
  - `~/.config/vexos/ai/threshold`: 1–100, default 95
  - `~/.config/vexos/ai/crash-ignore/<name>`: one empty file per muted program
  - `~/.local/share/vexos/ai/claude-accounts/<label>/`: one dir per extra account (`main` is implicit)
  - `~/.cache/vexos/ai/usage-<label>.json`: `{"ok":true,"session":N,"weekly":N,"sessionReset":..,"weeklyReset":..}`
    or `{"ok":false,"why":"..."}`. N is a percentage (may be fractional).
  Every path honours `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME`.

## Problem

Provide a dedicated, feature-gated "AI Assistant" page: setup, Claude accounts, usage,
switch mode and crash mutes. It uses the new justfile wrappers and does not shell out to `vexos-ai`.

## Contract deviation (must be reported to the user)

`vexos-ai pick` takes no argument and opens zenity whenever `WAYLAND_DISPLAY` is set,
which includes VexPortal's terminal. The GUI therefore cannot pass the user's choice. Per
the user's instruction we do not write `agent` ourselves; the catalog declares
`ai-pick agent=""` (`agent` = `claude` | `opencode`). vexos-nix must accept it in
both `ai-pick` and `vexos-ai pick [claude|opencode]`.

Justfile signatures the catalog (and fixture) expect:

```
ai-pick agent=""
ai-account-add label
ai-account-use target
ai-account-remove label
ai-account-mode mode pct=""
ai-crash-mute name="" state=""
```

## Design

### Catalog
- New `[[page]] id = "ai"`, title "AI Assistant", icon `user-available-symbolic`,
  placed first among catalog pages (sidebar: Overview, AI Assistant, …).
- `agent`, `diagnose` move to `page = "ai"`, group "Assistant".
- Six new terminal actions (`mode = "terminal"`, roles desktop/htpc/server like `agent`):
  `ai-pick` (choice claude/opencode, required), `ai-account-add` (label, `account-label`),
  `ai-account-use` (target, `account-ref`), `ai-account-remove` (label, `account-label`,
  destructive + confirm), `ai-account-mode` (choice manual/auto required, pct `percent` optional),
  `ai-crash-mute` (name `program-name` optional, state choice `off` optional).
- New closed `Format`s in `catalog/src/format.rs`:
  - `account-label`: `[A-Za-z0-9_-]{1,32}`, not `main` (vexos-ai `valid_label`)
  - `account-ref`: the same charset, `main` and `next` allowed (for `use`)
  - `percent`: integer 1–100
  - `program-name`: `[A-Za-z0-9._+-]`, ≤255, not starting with `.` (no `/`, so
    `${name##*/}` is a no-op; no `.`/`..`)
  All reject shell metacharacters; they are added to the existing metacharacter test.
- Fixture `catalog/tests/fixtures/vexos-nix-justfile.json`: add the six recipes with
  the signatures above.

### GUI
- `src/system/ai.rs`: `AiState::read()` (agent, accounts with usage, active, mode,
  threshold, muted list) from user files, plus `is_installed()` via
  `glib::find_program_in_path("vexos-ai")`. Pure parsing helpers are unit-tested.
- `App::ai_enabled()` (moved from features.rs). `visible_pages()` drops `ai` when it is off.
  `pages::build("ai")` falls back to the Overview when it is off, so no route reaches it.
- `Window`: `ids` becomes `Rc<RefCell<Vec<String>>>`. `refresh_sidebar()` recomputes
  entries and, if they changed, repopulates the list box; if the current page vanished it
  shows Overview. Called from `state_changed()`. Any finished job, including
  `feature-enable ai`, therefore adds or removes the entry without a restart.
- `src/ui/pages/ai.rs`:
  - Not installed (`vexos-ai` not on PATH): a "Rebuild to finish installing" row with
    Rebuild Now (when `rebuild` is visible). No other actions are shown.
  - No agent: a prominent "Set up your assistant" group with two rows, Claude Code and
    OpenCode, each with a Choose button. This runs `ai-pick` with the agent preset in the
    terminal. On success the terminal closes and an alert offers "Open <agent>"
    (`agent` action).
  - Agent set: a current-assistant row with Change (a dialog with the same two choices)
    and Open (`agent`). Diagnose a Problem row below.
  - Claude only: an Accounts group, one row per account showing session %, weekly % or
    the reason usage is unknown, "Active" pill or Use button, and Remove (not on `main`).
    Add Account opens the arg dialog for `ai-account-add`. Auto-switch is a ComboRow
    (Manual / Automatic) plus a SpinRow threshold and an Apply button that runs
    `ai-account-mode` with both values.
  - Crash notifications: one row per muted program with Unmute (`ai-crash-mute name off`).
    It shows "No programs are muted." when empty.
  - Each new action is hidden when the host justfile lacks it (existing `app.visible`).
- `terminal_view::open` returns `Option<Rc<Job>>` so the page can follow `ai-pick`.
- features.rs: drop the AI gate.
- README: mention the AI Assistant page.

## Steps → verification
1. Catalog + formats + fixture → `cargo test -p vexportal-catalog` (drift fixture passes).
2. `system/ai.rs` with tests → unit tests.
3. App/window/pages wiring + ai page → `cargo check`/`clippy`.
4. Preflight → `scripts/preflight.sh` exit 0.

Commands: only those in CLAUDE.md Phase 3 (via `nix develop -c`), plus
`nix build .#default` is not required (no packaging/data changes).

## Dependencies
None new. Uses gtk4 0.9 / libadwaita 0.7 (`v1_6`, so `adw::SpinRow` is available), glib, serde_json.

## Risks
- If vexos-nix's recipes don't land with these signatures, the drift test fails on a built host.
  Mitigation: signatures are stated above and in the hand-off.
- Usage cache may be stale (vexos-ai refreshes it every 5 min via its timer for the active account only);
  rows show the cached value as-is.
- Each account/mode/unmute operation opens the terminal dialog (terminal mode is required since the
  daemon's HOME is /root). The page refreshes when the job ends.
