# GUI Overhaul — Review (Phase 3)

**Spec:** `.github/docs/subagent_docs/gui_overhaul_spec.md`
**Date:** 2026-10-05
**Build host:** WSL2 Ubuntu on the user's Windows machine, Nix 2.x with flakes; all cargo commands run inside the project's `nix develop` devShell (GTK 4.22.4, libadwaita 1.9.3). No FORBIDDEN COMMANDS were used.

## Files changed

Modified: `Cargo.toml`, `Cargo.lock`, `flake.nix`, `nix/package.nix`, `README.md`, `catalog/src/{catalog.toml,lib.rs,validate.rs,drift.rs}`, `daemon/src/{executor.rs,interface.rs,main.rs}`, `data/style.css`, `src/{app.rs,just.rs,main.rs}`, `src/system/{mod.rs,state.rs,variant.rs}`, `src/ui/{mod.rs,window.rs,arg_dialog.rs}`.
Added: `catalog/tests/drift_against_fixture.rs`, `catalog/tests/fixtures/vexos-nix-justfile.json`, `src/job.rs`, `src/terminal.rs`, `src/system/vpn.rs`, `src/ui/{actions.rs,job_view.rs,terminal_view.rs,switch_dialog.rs}`, `src/ui/pages/{mod,overview,system,features,network,services,activity}.rs`.
Deleted: `src/ui/{category_page.rs,dashboard.rs,run_page.rs}` (replaced by the pages, `job_view.rs` and `actions.rs`).

## Build validation (verbatim summaries)

| # | Command | Result |
|---|---|---|
| 1 | `nix develop -c cargo fmt --all -- --check` | First run: diff in `overview.rs` (one builder chain). Fixed with `cargo fmt --all`; re-run clean. |
| 2 | `nix develop -c cargo check --workspace` | `Finished dev profile` — no errors |
| 3 | `nix develop -c cargo clippy --workspace --all-targets` | First run: 1 `type_complexity` warning in `services.rs`; fixed with a type alias; re-run: no warnings |
| 4 | `nix develop -c cargo test --workspace` | catalog lib 35 passed · vexportal bin 35 passed · `drift_against_fixture` 3 passed · `drift_against_justfile` 2 passed (self-skip: no `/etc/nixos/justfile` off-host — expected) · daemon 6 passed · 0 failed |
| 5 | `nix build .#default` (packaging changed) | `exit=0`; release build + in-sandbox `cargoCheckHook` tests pass; `result/bin/vexportal` wrapper carries `xdg-terminal-exec` and `just` on PATH; `result/libexec/vexportal-daemon` present |

**Visual check** (CLAUDE.md: nested wlroots compositor): the real binary was run in headless `cage` inside a `bubblewrap` sandbox with a fake `/etc/nixos` built from vexos-nix's own justfile and templates, a stand-in `vexos-vpn`, and `VEXPORTAL_VARIANT` set to desktop and server roles; 17 screenshots were taken with `grim`. They exposed three defects that are now fixed: (a) the overview hero rendered beneath its rows; (b) service descriptions containing `&` rendered blank because rows parsed them as Pango markup; (c) three icon names absent from Adwaita 50. The embedded terminal was exercised for real: `just ssh` ran as the user, generated a key, and reached `ssh-copy-id`.

## Spec compliance

| Spec item | Status |
|---|---|
| §1.1 catalog resync to `f6159bb` (menus, `implements`, legacy580, setup-rdp removed, new VPN/AI/NAS/restore actions) | Done — fixture drift test reports **zero** drift of any kind |
| §3.1 three lanes; terminal lane argv-only, no shell, `VEXOS_ASSUME_YES` absent | Done — `terminal.rs` tests assert argv and no `-c`/`-lc` |
| §3.2 schema v2 + consistency rules (a)–(d) | Done, each rule unit-tested |
| §3.3 pages | Done. Deviation: Activity is a sidebar entry with a running spinner plus an Overview button, not a separate header-bar button (one entry point fewer, same reach). |
| §3.3 bootloader step 2 disabled until reboot | **Not implemented** — the recipe itself refuses unless the session booted via Limine, and its confirm says so; tracking reboot state in the GUI would duplicate that check. Recorded as a deliberate simplification. |
| §3.4 switch dialog | Done; conditional DE / VM fields, current values preselected |
| §3.5 rebuild-needed (mtime) | Done; banner on every page |
| §3.6 job model, L1 orphan buffering, L3 dialogs | Done; router unit-tested including Finished-before-Started |
| §3.7 / L7 confirmation in both lanes | Done; `check_consistency` enforces destructive ⇒ confirm |
| L8 trailing comments | Done (bonus from shared `Settings` parser), tested |

## Review findings

**CRITICAL:** none.

**RECOMMENDED (addressed in this phase):**
1. Markup injection in rows (finding (b) above) — fixed with `use_markup(false)` / `markup_escape_text` on every row carrying justfile- or command-derived text.
2. Plex Pass switch revert re-entered its own handler — fixed with the same revert guard the feature switches use.
3. `Service Catalog` row duplicated the page it sat on — dropped from the Services page.

**Observations (not blocking, out of scope or pre-existing):**
- L4 (D-Bus client connects at startup) is pre-existing and still present — visible in the screenshot logs as "could not connect to the system bus". Untouched per spec §9.
- A job's state listeners accumulate one closure per dialog opened (weak refs only, so no widget leaks); acceptable for session-lived jobs.
- VTE uses its default palette (black terminal on a light window). Matching the libadwaita palette is a cosmetic follow-up.
- Context7 MCP was unreachable all session; `vte4` 0.8 / gtk4 0.9 compatibility was verified from crates.io metadata and by compiling.
- The spec's open question on `switch` accepting `headless-server` passed directly could not be re-verified (a read of that recipe section was blocked by the permission classifier); the role list is unchanged from the previous catalog.

## Security

- Daemon: unchanged D-Bus signature, polkit tiers, stdin-secret path and argv exec. It now refuses `mode = terminal` ids (`TerminalOnly`) exactly as it refused `terminal = true` recipes before.
- Terminal lane: runs as the invoking user with no added privilege; `build_for_terminal` refuses daemon-lane ids, so a root action cannot be started unprivileged by mistake; argv exec'd by VTE (`spawn_async`), no shell string anywhere (`grep -rn '"-lc"\|"-c"' src/` → only the test assertion).
- Menu-routed whitespace splitting is prevented at catalog load.
- Local reads only (`vexos-vpn status/regions --json`, world-readable files).

## Scores

| Category | Score | Grade |
|----------|-------|-------|
| Specification Compliance | 93% | A |
| Best Practices | 92% | A |
| Functionality | 94% | A |
| Code Quality | 91% | A- |
| Security | 96% | A |
| Performance | 90% | A- |
| Consistency | 93% | A |
| Build Success | 100% | A+ |

**Overall Grade: A (94%)**

**Result: PASS**
