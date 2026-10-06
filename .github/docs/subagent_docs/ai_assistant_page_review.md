# AI Assistant page — review

Spec: `.github/docs/subagent_docs/ai_assistant_page_spec.md`

## Files reviewed
- `catalog/src/catalog.toml`: new `ai` page (first), `agent`/`diagnose` moved, six `ai-*` terminal actions
- `catalog/src/lib.rs`, `catalog/src/format.rs`: `account-label`, `account-ref`, `percent`, `program-name` formats + tests
- `catalog/tests/fixtures/vexos-nix-justfile.json`: six recipes added (additions only, original escaping preserved)
- `src/system/ai.rs` (new), `src/system/mod.rs`: per-user `vexos-ai` state reader + tests
- `src/app.rs`: `ai_enabled()`, `visible_pages()` gate
- `src/ui/window.rs`: refreshable sidebar (`refresh_sidebar`, `sidebar_entries`, `syncing` guard)
- `src/ui/pages/ai.rs` (new), `src/ui/pages/mod.rs`: the page; `"ai"` route falls back to Overview when off
- `src/ui/pages/features.rs`: AI group gate removed
- `src/ui/terminal_view.rs`: `open` returns `Option<Rc<Job>>`
- `README.md`

## Findings
- Spec compliance: all items implemented. Two deviations, both deliberate and reported to the user:
  1. `ai-pick` takes `agent` (vexos-ai's `pick` cannot take the choice today).
  2. While no assistant is chosen and `ai-pick` is available, the `agent`/`diagnose` rows are hidden,
     because both start `vexos-ai launch`, which falls back to its zenity picker.
- Security: no shell, no direct `vexos-ai` exec. Every change goes through catalog-validated
  `just ai-*` argv in the terminal lane. The new formats reject the shared shell-metacharacter
  probes (test extended). `ai-account-remove` is destructive with a confirm message. GUI reads
  are plain file reads in the user's own home.
- Correctness notes checked: the `RefCell` borrow is bound before `force_close`; sidebar selection
  changes during repopulation are ignored via `syncing`; the activity spinner is reparented
  safely; an `ai` route while the feature is off renders the Overview.
- Refresh: any finished job runs `refresh_state` → `state_changed` → `rebuild_current` →
  `refresh_sidebar`, so turning `ai` on (the `feature-enable` job) adds the entry without a restart.
- Minor/accepted: Claude account usage is the cached reading `vexos-ai` last wrote; each account,
  mode or unmute change opens the terminal dialog (required: these act on the user's HOME).
- GUI not visually verified: on this host `ai` is off, `vexos-ai` is not installed and
  `/etc/nixos/justfile` lacks even `agent`, so the page cannot appear without changing system files.

## Build validation (verbatim summaries)
1. `nix develop -c cargo fmt --all -- --check`: exit 0
2. `nix develop -c cargo check --workspace`: Finished
3. `nix develop -c cargo clippy --workspace --all-targets`: Finished, no warnings
4. `nix develop -c cargo test --workspace --no-fail-fast`:
   - vexportal: 41 passed
   - vexportal-catalog: 38 passed
   - drift_against_fixture: 3 passed
   - vexportal-daemon: 6 passed
   - drift_against_justfile: **FAILED**, `catalog_matches_the_installed_justfile`:
     `` `vpn-selftest` is in the justfile but not the catalog ``
     **Pre-existing, unrelated:** this host's `/etc/nixos/justfile` predates vexos-nix's menu refactor
     (f6159bb) and the AI feature (`agent`, `diagnose` and all menu recipes are reported Missing). At
     HEAD the catalog also has no action whose `command[0]`/`implements` is `vpn-selftest`
     (it uses `vpn selftest` → `_vpn-selftest`), so the same failure occurs without this change. The
     new `ai-*` recipes appear only as benign `Missing` entries. Fix: rebuild this host from
     current vexos-nix; nothing in this change can or should alter it.

| Category | Score | Grade |
|----------|-------|-------|
| Specification Compliance | 95% | A |
| Best Practices | 93% | A |
| Functionality | 92% | A- |
| Code Quality | 93% | A |
| Security | 98% | A+ |
| Performance | 97% | A+ |
| Consistency | 94% | A |
| Build Success | 85% | B (one pre-existing host-specific test failure) |

**Overall Grade: A- (93%)**

Verdict: **PASS** for the change itself. The only failing check is the pre-existing live-host drift
test described above, which no refinement within this task can fix. Phase 6 is expected to hit it.
