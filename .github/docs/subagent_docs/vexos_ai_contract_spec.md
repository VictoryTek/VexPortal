# vexos-ai machine contract — spec

## Current state

- `catalog/tests/fixtures/vexos-nix-justfile.json` was trimmed by hand; its `ai-*` entries
  were written before the recipes existed, so the drift test passed while real hosts
  lacked them.
- `src/system/ai.rs` reads vexos-ai's state files directly. `usage-<label>.json` is parsed
  into `session: f64` / `weekly: f64` / `why: String`.
- AI actions run in the built-in terminal with `VEXPORTAL=1` only; an empty choice lets
  `vexos-ai` open a zenity dialog over VexPortal. Failures read "Stopped with exit code N."
- Docs and a comment point at `pkgs/vexos-ai/vexos-ai.sh` in vexos-nix; the CLI now lives
  in github:VictoryTek/vexos-ai (`bin/vexos-ai.sh`, contract in `docs/contract.md`, 2789611).

## Proposed solution

1. `scripts/refresh-drift-fixture.sh`: `just --dump --dump-format json | jq -aS --indent 1`
   keeping `assignments.*.value` and each recipe's `private`, `doc`, `parameters`
   (`name`, `default`, `kind`) — the trim the test header describes. Defaults to
   `../vexos-nix/justfile`. Regenerated from vexos-nix ebff3c3: the only change is a new
   `SYSTEMD_PAGER` assignment; `ai-*`, `agent` and `diagnose` are unchanged, and there is
   no catalog drift.
2. `terminal::environment(invocation)` adds `VEXOS_AI_NONINTERACTIVE=1` for `ai-*` actions.
   `terminal::ai_exit_message(action, code)` maps 2/3/4/5 (5 on `ai-account-remove` is
   "Close the running Claude session first."); `terminal_view` shows it on failure.
3. `AiState::read()` runs `vexos-ai status --json` (never `--refresh`: it goes to the
   network). Fallback to the file reader when the command fails, `schema` is absent or
   > 1, the JSON does not parse, or no accounts are listed. Unknown keys are ignored.
   `Usage` takes `Option` numbers (null when `ok` is false), `stale`, `why` and `scoped`.
   The account row shows plan, a signed-out notice (`signedIn: false`), and usage; a stale
   reading keeps its numbers and is marked with `why`. Scoped limits are appended to the
   usage line, which already wraps.
4. Stale `pkgs/vexos-ai/vexos-ai.sh` references updated in `ai.rs` and
   `ai_assistant_page_spec.md`.

## Dependencies

None new (`serde`, `serde_json` already present). Build-time tools for the refresh script
only: `just`, `jq`.

## Risks

- `status --json` runs synchronously while the page is built. Plain `status` reads files
  only, so it is fast; a hung CLI would stall the page. Accepted; the page already reads
  files synchronously.
- Out of scope: the `vexportal --page ai` deep link, and calling vexos-ai directly instead
  of through recipes.

## Verification

`scripts/preflight.sh` (fmt, check, clippy, test through `nix develop`).
