# AI Assistant reboot prompt — spec

## Request
After the rebuild that installs the AI Assistant, VexPortal prompts to reboot (user's choice: after
the install rebuild, not when the switch is flipped). vexos-nix adds the same advice to its recipe.

## Current state
- `vexos-ai` is in `environment.systemPackages` (`modules/ai.nix`). Its user services
  (`vexos-ai-crash-watch`, `-theme-watch`, `-welcome`, the usage timer) are wanted by
  `graphical-session.target`/`timers.target`, so they only start in a session begun after the
  rebuild. VexOS auto-logs in, so in practice a reboot is needed.
- Every finished job runs `App::refresh_state` and then rebuilds the visible page.
- The AI page shows "Rebuild to finish installing" while `vexos-ai` is not on PATH.
- The Overview already shows a generic "Reboot to finish updating" row when
  `/run/booted-system` ≠ `/run/current-system`.

## Design
- `system::ai::needs_reboot()`: `/run/current-system/sw/bin/vexos-ai` exists and
  `/run/booted-system/sw/bin/vexos-ai` does not. This reads the system profiles, so it is correct
  across VexPortal restarts and clears itself on reboot.
- `App` gains `ai_reboot: Cell<bool>`, initialised from `needs_reboot()` at startup. In `track`'s
  finished-job path, after `refresh_state`: if it flips false → true while `ai_enabled()`, call
  `pages::offer_ai_reboot`. Starting VexPortal on an already pending machine does not re-prompt; the page row covers that case.
- `pages::offer_ai_reboot` (in `pages/ai.rs`): an `adw::AlertDialog`, "Reboot to finish setting up
  the AI Assistant?". The body names what starts after a reboot and warns about unsaved work.
  Responses: Later / Reboot Now (destructive appearance). Reboot Now runs the catalog `reboot`
  action via `App::run` + `job_view::present`. The dialog already carries the reboot warning, so the
  catalog confirm is not shown a second time. It is only offered when `app.visible("reboot")`.
- AI page: while `needs_reboot()`, the "Rebuild to finish installing" state becomes "Reboot to
  finish installing" with a Reboot button (which goes through the normal `actions::activate`, with
  its confirm), and no other actions, mirroring the not-installed state.

## Files
`src/system/ai.rs`, `src/app.rs`, `src/ui/pages/ai.rs`, `src/ui/pages/mod.rs`.
No catalog, daemon or packaging change, so no `nix build` is needed.

## Verification
Phase 3 commands via `nix develop -c`; a unit test for the path predicate (`needs_reboot_in` with
temp dirs). The live-host drift test still fails for the pre-existing stale-justfile reason
(see `ai_assistant_page_review.md`).
