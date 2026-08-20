# Spec: Remove update/update-all/deploy from VexPortal's catalog

## Current state analysis

`catalog/src/catalog.toml` declares three recipes in the `build-deploy` category
that fetch/apply upstream changes:

- `update` (catalog.toml:92-100) — updates flake inputs, then rebuilds+switches.
- `update-all` (catalog.toml:102-111) — same, unconditionally, no cache-safety check.
- `deploy` (catalog.toml:113-121) — pulls latest vexos-nix commit, rebuilds, leaves
  nixpkgs pinned.

The user reported a drift banner: the host's justfile now defines `update` with
`role`/`variant` parameters that the catalog does not declare
(`catalog/src/drift.rs` `Drift::Parameters`). Investigating
`docs/vexos-nix-prompt.md` (Task 2) confirms this: `update` gained optional
`role=""`/`variant=""` params, needed only for the stateless-reboot case where
`/etc/nixos/vexos-variant` is absent — mirroring `switch`'s existing params.

Rather than teach the catalog `update`'s new parameters, the user clarified
VexPortal's scope: **VexPortal is strictly for enabling/disabling features,
services, and other justfile-driven system config — not for updates/upgrades.**
A separate tool ("Up") already owns updates/upgrades. The user confirmed via
clarifying question that `update` and `update-all` are update functionality
that should be dropped, and separately confirmed `deploy` should be dropped too
(it pulls new config from upstream, same "pull something new in" category).
`upgrade-analysis`/`rollback`/`rollforward` and `switch`/`build`/`rebuild` were
explicitly kept — rollback/rollforward move between generations already present
on disk (no fetch), and switch/build/rebuild apply/test the *current* pinned
config, so none of those are "update/upgrade" in the sense the user means.

## Problem definition

1. `catalog.toml` must stop declaring `update`, `update-all`, and `deploy` as
   recipes VexPortal surfaces.
2. Simply deleting their `[[recipe]]` blocks would make the drift checker flag
   them as `Drift::Unlisted` the next time it runs against a justfile that still
   defines them (which it does — they're real justfile recipes, just recipes
   VexPortal now deliberately declines to show). The catalog has an existing
   `[[excluded]]` mechanism (`lib.rs` `Excluded { name, reason }`,
   `drift.rs::compare` skips any justfile recipe listed there) built exactly for
   this: a recipe the catalog knows about and deliberately omits.
3. `src/ui/dashboard.rs` hardcodes `update` in `QUICK_ACTIONS`
   (dashboard.rs:11). Once `update` is gone from the catalog, that lookup
   silently no-ops (`app.catalog.recipe(name)` returns `None`, loop `continue`s),
   so nothing breaks, but the dashboard would silently lose a quick-action slot
   and carry a dead reference to a name no longer in the catalog.

## Proposed solution

1. Delete the three `[[recipe]]` blocks (`update`, `update-all`, `deploy`) from
   `catalog/src/catalog.toml`.
2. Add an `[[excluded]]` entry for each of the three, with a `reason` explaining
   updates/upgrades are out of scope (owned by Up), so the drift checker treats
   their presence in the real justfile as expected rather than flagging
   `Unlisted`.
3. Update the `build-deploy` category's `description` (catalog.toml:27) — it
   currently reads "Rebuild this machine, change its role, and pull
   configuration changes," and "pull configuration changes" described `deploy`,
   which is going away. Reword to match what remains (`variant`, `rebuild`,
   `build`, `switch`).
4. Replace `"update"` in `dashboard.rs`'s `QUICK_ACTIONS` (dashboard.rs:11) with
   `"switch"`, the closest remaining build-deploy action, so the dashboard keeps
   four working quick actions instead of one dead lookup.
5. No changes needed to `daemon/`, `validate.rs`, or `format.rs` — none
   reference these recipe names directly; the daemon validates whatever the
   catalog declares.

## Implementation steps

1. Edit `catalog/src/catalog.toml`:
   - Remove the `update`, `update-all`, `deploy` `[[recipe]]` blocks.
   - Add three `[[excluded]]` entries (name + reason) for them.
   - Reword the `build-deploy` category description.
2. Edit `src/ui/dashboard.rs`: change `QUICK_ACTIONS` entry `"update"` →
   `"switch"`.
3. No other files require changes.

## Dependencies

None — no new crates, no external library integration. Context7 not applicable
(internal-only change, no new dependency).

## Configuration changes

None.

## Risks and mitigations

- **Risk:** the existing `catalog/tests/drift_against_justfile.rs` test runs
  against a real `/etc/nixos/justfile` + `just` binary, which is absent in this
  dev/sandboxed environment, so the fix cannot be drift-verified against a real
  host here. **Mitigation:** the unit-level drift tests in `drift.rs` (which run
  against an in-memory `JustDump`, no real justfile needed) cover the
  `excluded` skip path (`ignores_private_guard_recipes` is the closest existing
  precedent) and will be exercised by `cargo test -p vexportal-catalog`; the
  host-only integration test self-skips as documented in CLAUDE.md and that
  skip is expected, not a failure.
- **Risk:** removing recipes a user may already have pinned to a quick action
  or muscle memory. **Mitigation:** none needed beyond this being the user's
  explicit, confirmed instruction.
- **Risk:** `catalog.check_consistency()` requires every category to have at
  least one recipe (`every_category_is_used` test). `build-deploy` keeps
  `variant`, `rebuild`, `build`, `switch` — still non-empty, so this is safe.
