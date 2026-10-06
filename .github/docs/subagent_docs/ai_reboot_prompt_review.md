# AI Assistant reboot prompt — review

Spec: `.github/docs/subagent_docs/ai_reboot_prompt_spec.md`
Files: `src/system/ai.rs`, `src/app.rs`, `src/ui/pages/ai.rs`, `src/ui/pages/mod.rs`

## Findings
- Spec compliance: complete. `needs_reboot()` compares `sw/bin/vexos-ai` in the current and booted
  systems. The prompt fires once, on the false→true edge, after any finished job (in practice the
  install rebuild), only while `ai` is on. The AI page shows "Reboot to finish installing" until reboot.
- Ordering: `jobs_changed(true)` rebuilds the page first (it shows the reboot row), then the alert
  is presented over it. A VexPortal restart while pending does not re-prompt (the cell starts at the
  current value).
- Reboot Now runs the catalog `reboot` action through `App::run` (daemon, polkit
  `run-recipe`). The catalog confirm is skipped because this dialog carries the same unsaved-work
  warning and uses destructive styling. The page row's button uses the normal path with the confirm.
- Security: no new exec path; reboot still goes through the daemon and polkit.
- Not visually verified: needs a host where a rebuild installs `vexos-ai` (see previous review).

## Build validation
- fmt --check: exit 0 · check: Finished · clippy --all-targets: no warnings
- test --workspace --no-fail-fast: vexportal 42 passed (new
  `a_reboot_is_needed_only_between_install_and_boot`), catalog 38, fixture drift 3, daemon 6.
- `catalog_matches_the_installed_justfile`: FAILED, pre-existing and unrelated (this host's
  `/etc/nixos/justfile` predates vexos-nix f6159bb; see `ai_assistant_page_review.md`).
- `scripts/preflight.sh`: exit 101, solely because of that test.

| Category | Score | Grade |
|----------|-------|-------|
| Specification Compliance | 97% | A+ |
| Best Practices | 94% | A |
| Functionality | 94% | A |
| Code Quality | 94% | A |
| Security | 97% | A+ |
| Performance | 98% | A+ |
| Consistency | 95% | A |
| Build Success | 85% | B (pre-existing host-specific test failure) |

**Overall Grade: A (94%)**

Verdict: **PASS** for the change. Preflight is blocked only by the stale-host drift test.
