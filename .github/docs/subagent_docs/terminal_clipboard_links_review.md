# Built-in terminal: clipboard, context menu, clickable links — review

Spec: `terminal_clipboard_links_spec.md`. Files: `Cargo.toml`, `src/terminal.rs`,
`src/ui/terminal_view.rs`.

## Build validation (WSL, `nix develop -c …`)
1. `cargo fmt --all -- --check` — **exit 1**. rustfmt wants three lines in `src/terminal.rs`
   wrapped: the `held` mask in `clipboard_key` (line 78), three `assert_eq!`s in
   `clipboard_shortcuts`, one in `only_web_links_are_opened`. Diff is whitespace-only.
2. `cargo check --workspace` — exit 0 (vte4 `v0_76` APIs resolve against VTE 0.84.1).
3. `cargo clippy --workspace --all-targets` — exit 0. One warning, **pre-existing and outside
   this change**: `clippy::nonminimal_bool` at `src/system/ai.rs:223` (new lint in clippy
   1.95). Not touched here (surgical-changes rule); reported to the user.
4. `cargo test --workspace` — exit 0. vexportal 51 passed, incl. new
   `terminal::tests::clipboard_shortcuts`, `plain_control_keys_still_reach_the_program`,
   `only_web_links_are_opened`; catalog 38; drift fixture 3; drift against justfile 2 (ran, a
   justfile is present in this WSL); daemon 6.
5. `nix build` — not required (no packaging/data change).

## Findings
- **CRITICAL** — formatting check fails (above). Fix: `nix develop -c cargo fmt --all`.
- Spec compliance: all designed pieces present: capture-phase key controller, swallowed
  empty-selection copy, VTE-owned context menu with `term.*` actions and selection-tracking
  Copy, OSC 8 + regex links, Ctrl+click via capture-phase `GestureClick`, `UriLauncher` with toast
  on failure, status label text.
- Security: argv/exec path and envv unchanged; only `http(s)` links are opened (OSC 8 can
  carry `file:` etc.); paste goes through VTE (bracketed paste honoured).
- Ownership: closures stored on the terminal hold weak refs to it and to the copy action; the
  link gesture holds a `Window` clone, matching the existing `connect_close_attempt` pattern.
- Not verifiable here: actual GUI behaviour (no display in WSL). Must be confirmed on the
  host: Ctrl+Shift+V into Claude Code's code prompt, right-click menu, Ctrl+click on the
  sign-in URL.
- Problem 2 root cause still open (diagnostic not run) — by design of the spec.

| Category | Score | Grade |
|----------|-------|-------|
| Specification Compliance | 100% | A |
| Best Practices | 95% | A |
| Functionality | 90% | A- |
| Code Quality | 92% | A- |
| Security | 100% | A |
| Performance | 100% | A |
| Consistency | 85% | B |
| Build Success | 75% | C |

**Overall Grade: B+ (92%)** — **NEEDS_REFINEMENT** (fmt check failure).
