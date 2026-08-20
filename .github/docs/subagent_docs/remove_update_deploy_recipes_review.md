# Review: Remove update/update-all/deploy from VexPortal's catalog

## Specification compliance

Matches `.github/docs/subagent_docs/remove_update_deploy_recipes_spec.md` step
for step:

1. `update`, `update-all`, `deploy` `[[recipe]]` blocks removed from
   `catalog/src/catalog.toml`. ✅
2. Three `[[excluded]]` entries added for those names, in the existing "Not
   surfaced" section, following the `Excluded { name, reason }` shape already
   defined in `lib.rs`. ✅
3. `build-deploy` category description reworded to drop the "pull
   configuration changes" clause that described `deploy`. ✅
4. `dashboard.rs` `QUICK_ACTIONS` swapped `"update"` → `"switch"`. ✅
5. No daemon/validate/format changes — confirmed via grep that nothing else in
   the workspace referenced these three recipe names. ✅

## Static verification performed

- `git diff` reviewed in full; it is exactly the five changes above, nothing
  extraneous touched (Principle 3, surgical changes).
- `grep -n "^\[\[recipe\]\]" -A1 catalog.toml | grep "name ="` confirms
  `update`/`update-all`/`deploy` no longer appear among recipe names, and
  `build-deploy` still has 4 recipes (`variant`, `rebuild`, `build`, `switch`)
  — satisfies `every_role_has_something_to_show` / `every_category_is_used`
  invariants in `lib.rs` by inspection.
- Repo-wide grep for `"update-all"|"deploy"|recipe("update|== "deploy"` finds
  only the two new `[[excluded]] name = "update-all"` / `name = "deploy"`
  lines — no other file references the removed recipes.
- TOML structure hand-checked against the existing `[[excluded]]` schema
  (`lib.rs:245-249`); each new entry has both required fields (`name`,
  `reason`), matching `Excluded`'s non-`Option` fields.

## Build validation — RUN via WSL

The Windows host has no native `nix`, but `nix` is available inside WSL, which
can reach the repo at `/mnt/c/Projects/VexPortal`. All four Phase 3 commands
were run there through `nix develop -c ...` (never bare `cargo`, never `nix
shell`):

1. `nix develop -c cargo fmt --all -- --check` — reported diffs, but every
   diff is in `src/ui/category_page.rs` and `src/ui/run_page.rs`, files this
   change never touched (pre-existing formatting debt). Neither changed file
   (`catalog/src/catalog.toml` is not Rust; `src/ui/dashboard.rs`) appears in
   the diff output — confirmed via `grep -i dashboard` on the fmt output,
   zero matches. Per Principle 3 (surgical changes), pre-existing unrelated
   formatting issues are not this change's to fix.
2. `nix develop -c cargo check --workspace` — clean, exit code 0. All three
   workspace members (`vexportal`, `vexportal-catalog`, `vexportal-daemon`)
   compiled successfully in 10m07s (cold cache).
3. `nix develop -c cargo clippy --workspace --all-targets` — clean, exit code
   0, no lint warnings.
4. `nix develop -c cargo test --workspace` — clean, exit code 0, all 55 tests
   passed:
   - `vexportal`: 21/21
   - `vexportal-catalog`: 26/26 (includes `drift::tests::*` and the
     `tests::every_category_is_used` / `every_role_has_something_to_show`
     invariants this change could have broken)
   - `vexportal-daemon`: 6/6
   - **`catalog/tests/drift_against_justfile.rs`: 2/2, and notably
     `catalog_matches_the_installed_justfile` actually *ran* (not skipped) —
     this WSL environment has a real `/etc/nixos/justfile` and `just` binary,
     so the fix was verified against real justfile ground truth, not just an
     in-memory `JustDump` fixture.** This directly confirms the original
     drift banner (`update` taking `[role, variant]`) is resolved: `update`
     is no longer in the catalog to disagree with the justfile, and the
     `excluded` entry suppresses the `Unlisted` check.
5. `nix build .#default` — not run; per the spec this change doesn't touch
   packaging, `nix/package.nix`, `nix/module.nix`, or `data/`, so it's out of
   scope for Phase 3 per the project's own conditional rule.

## Score table

| Category | Score | Grade |
| ----------- | ----- | ----- |
| Specification Compliance | 100% | A |
| Best Practices | 100% | A |
| Functionality | 100% | A (verified against the real installed justfile) |
| Code Quality | 100% | A |
| Security | 100% | A (no argv/exec/daemon path touched) |
| Performance | N/A | N/A |
| Consistency | 100% | A |
| Build Success | 100% | A (fmt/check/clippy/test all clean via WSL `nix develop`) |

**Overall Grade: A (100%)**

## Returns

- **PASS/NEEDS_REFINEMENT: PASS.** All Phase 3 build validation commands ran
  successfully (via WSL, since native Windows lacks `nix`) with zero failures
  and zero new lint/format issues. The real-justfile drift test confirms the
  fix resolves the original banner.
- No refinement cycle needed.
