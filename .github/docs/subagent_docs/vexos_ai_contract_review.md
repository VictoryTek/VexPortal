# vexos-ai machine contract — review

Spec: `vexos_ai_contract_spec.md`. Reviewed by the orchestrating agent, not a separate
reviewer.

## Build validation (WSL Ubuntu, `scripts/preflight.sh`, via `nix develop`)

- First run: `cargo fmt --all -- --check` failed on line wrapping in the new tests.
  `nix develop -c cargo fmt --all` fixed it.
- Second run: fmt, check, clippy (no warnings), test — exit 0. The new tests pass:
  `system::ai::tests::{builds_the_state_from_status_json, status_json_that_is_unknown_falls_back,
  an_unchosen_agent_and_missing_usage_are_fine, a_stale_reading_keeps_its_numbers_and_says_why}`
  and `terminal::tests::{only_ai_actions_run_vexos_ai_non_interactively, ai_exit_codes_have_messages}`.
  `catalog_matches_the_recorded_justfile` passes against the regenerated fixture.
- `nix build .#default` not run: no change to packaging, `nix/`, or `data/`.

## Findings

- Fixture: regenerated from vexos-nix ebff3c3. `ai-*`, `agent` and `diagnose` unchanged.
  Only difference: new `SYSTEMD_PAGER` assignment (`export SYSTEMD_PAGER := ""`); no
  catalog drift.
- Security boundary kept: the GUI still builds no shell line; the only new child process
  is `vexos-ai status --json` with fixed argv, stdin null.
- Known limit: `status --json` is read synchronously on page build (files only, no network).
- Not manually verified in a running window (no GUI session here).

**Result: PASS** — Spec compliance A, Build A, Security A.
