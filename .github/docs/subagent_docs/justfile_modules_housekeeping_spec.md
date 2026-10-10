# Spec: housekeeping for the modular vexos-nix justfile

## Current state

- vexos-nix split its justfile into a root `justfile` that imports `just/*.just`
  (commits ce977fa, b77437d). The recipe contract VexPortal uses (recipe names,
  parameter order and defaults, `_feature_names`, `_service_catalog`) is unchanged.
- `catalog/tests/fixtures/vexos-nix-justfile.json` was recorded from the older
  single-file justfile. `scripts/refresh-drift-fixture.sh` already regenerates it with
  the trimming described in `catalog/tests/drift_against_fixture.rs`.
- The `catalog/src/lib.rs` module doc says `just` keeps only the last comment line as
  a recipe's doc, with `backup-plex` as the example. Every public recipe now has an
  explicit `[doc(...)]`, so that example is stale.

## Audit: is the justfile treated as one standalone file?

No. Every consumer passes the path to `just`, which resolves imports relative to the
root justfile:

- `src/just.rs`: `just --justfile /etc/nixos/justfile --working-directory /etc/nixos --dump`
- `src/terminal.rs`: the same `--justfile` / `--working-directory` pair for the built-in and external terminals
- `daemon/src/executor.rs`: `Command::new("just").arg("--justfile").arg(&config.justfile)…`
- `nix/module.nix`: passes `--justfile ${cfg.justfile}` and sets `ProtectSystem = false`, so `/etc/nixos/just/` is visible to the daemon

Nothing copies, reads, or hashes the file. Deploying `/etc/nixos/just/` is vexos-nix's job.
No code change is needed.

## Changes

1. Regenerate the fixture: `scripts/refresh-drift-fixture.sh ../vexos-nix/justfile`.
2. Rewrite the doc-comment rationale in `catalog/src/lib.rs`: `just`'s own docs are
   now one-liners, but they still don't say which role a recipe applies to or how
   risky it is. Don't change any code.
3. Don't change `catalog.toml` unless a test shows real drift.

## Validation

- `nix develop -c cargo test -p vexportal-catalog`: fixture drift tests pass
- Live drift against the checkout: `just --justfile ../vexos-nix/justfile --dump --dump-format json` compared against the catalog (the self-skipping drift test only reads `/etc/nixos/justfile`)
- Phase 3 steps: fmt check, check, clippy, test (workspace), all through `nix develop`
- `nix build .#default` isn't needed: no packaging or `data/` changes

## Risks

- The fixture now records `[arg(pattern=…)]` metadata only if the trim keeps it. The
  trim keeps only `name`, `default` and `kind`, so the patterns aren't recorded and
  nothing else changes.
