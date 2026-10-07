#!/usr/bin/env bash
# Regenerate catalog/tests/fixtures/vexos-nix-justfile.json from a real vexos-nix
# justfile, as described in the header of catalog/tests/drift_against_fixture.rs.
#
#   scripts/refresh-drift-fixture.sh [path/to/justfile]
#
# Defaults to ../vexos-nix/justfile. Needs `just` and `jq` on PATH. Afterwards,
# `git diff` on the fixture shows what changed in the justfile since the last sync;
# any drift the diff exposes is then reported by `cargo test -p vexportal-catalog`.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

justfile="${1:-../vexos-nix/justfile}"
fixture="catalog/tests/fixtures/vexos-nix-justfile.json"

[ -f "$justfile" ] || { echo "no justfile at $justfile" >&2; exit 1; }

just --justfile "$justfile" --dump --dump-format json |
    jq -aS --indent 1 '{
        assignments: (.assignments | map_values({value})),
        recipes: (.recipes | map_values({
            private,
            doc,
            parameters: (.parameters | map({name, default, kind}))
        }))
    }' >"$fixture.tmp"
mv "$fixture.tmp" "$fixture"

echo "wrote $fixture from $justfile"
