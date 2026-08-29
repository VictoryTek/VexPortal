# Spec — Clear the catalog/justfile drift banner (`switch` args + bootloader recipes)

## Current state

`/etc/nixos/justfile` (the copy the daemon actually runs) has moved ahead of
`catalog/src/catalog.toml`. `nix develop -c cargo test -p vexportal-catalog --test
drift_against_justfile` reports three catalog defects and one benign host-behind note:

```
the catalog has drifted from /etc/nixos/justfile:
  - `switch` takes [role, variant, flake, de, vmp] in the justfile but the catalog declares [role, variant, flake]
  - `switch-bootloader` is in the justfile but not the catalog (Example: just switch-bootloader limine)
  - `switch-bootloader-cleanup` is in the justfile but not the catalog (cannot be run before a successful reboot has actually happened.)
note: this host's justfile predates 1 catalog entry:
  - `setup-rdp` is not in this host's justfile yet
```

The GUI renders the first three (plus "(and 2 more)") as the banner the user is seeing
at the top of the dashboard. The `setup-rdp` note is a `Drift::Missing`, is not a
catalog defect, and is out of scope.

### Justfile facts established by reading `/etc/nixos/justfile`

- `switch role="" variant="" flake="" de="" vmp=""` (line 117). All five have defaults,
  so all five are optional to `just`.
  - `de` is consumed only when `ROLE = desktop`; valid values `gnome | cosmic | hyprland`
    (line 325-328). Empty + desktop role ⇒ an interactive `read` menu.
  - `vmp` is consumed only when `VARIANT = vm`; valid values `qemu | virtualbox`
    (line 101-103). Empty + vm variant ⇒ an interactive `read` menu.
  - Those menus use bare `read`, **not** `_confirm`, so `VEXOS_ASSUME_YES` does not
    bypass them — under the daemon (`Stdio::null`, `set -euo pipefail`) the recipe
    aborts. This is the same pre-existing behaviour `switch`'s optional `role`/`variant`
    already have, and is why the recipe already carries `needs_upstream = true`.
- `switch-bootloader target="limine"` (line 415). Only `limine` is accepted; refuses
  non-UEFI hosts, refuses `vexos-vanilla-*` variants, and asks one `just _confirm`
  question. Patches `flake.nix`, runs `nixos-rebuild switch`, reorders UEFI BootOrder.
  Leaves the systemd-boot entry in place — non-destructive.
- `switch-bootloader-cleanup` (line 556). No parameters, no prompts. Refuses to run
  unless `BootCurrent` is the Limine entry. Deletes the old NVRAM entries and orphaned
  ESP files — destructive.
- `_confirm` (line 1580) honours `VEXOS_ASSUME_YES`, which
  `daemon/src/config.rs::recipe_environment()` sets to `1`. So `switch-bootloader`'s
  single confirmation auto-answers yes under the daemon; the GUI must carry the warning
  itself via `confirm =`.

## Problem

The catalog is the single source of truth shared by the GUI and the daemon. While it
disagrees with the installed justfile the dashboard shows a drift banner, the two
bootloader recipes are unreachable from the GUI, and `switch`'s argv would be missing
two positional slots the justfile now defines.

## Solution

Catalog-only change. No Rust, no GUI, no daemon, no packaging changes — every consumer
is already data-driven off `catalog.toml`.

1. **`switch`**: append two optional `choice` params, in justfile order, after `flake`.
   - `de` — "Desktop environment", choices `gnome, cosmic, hyprland`.
   - `vmp` — "VM platform", choices `qemu, virtualbox`.
   Both must be optional: `Catalog::check_consistency` rejects a required parameter that
   follows an optional one, and `flake` before them is optional. `Invocation` trims
   trailing empties, so leaving both unset produces exactly today's argv. Help text must
   say which selection each one applies to and that leaving it unchanged means the
   rebuild stops at a prompt.
2. **`switch-bootloader`**: new recipe in the `build-deploy` category.
   - `risk = "medium"`, `confirm` warning, `refresh = ["generation"]` (it rebuilds).
   - `roles`: everything except `vanilla` — the recipe itself refuses `vexos-vanilla-*`.
   - one required `choice` param `target` with the single choice `limine` (first
     positional, so `required = true` is legal).
3. **`switch-bootloader-cleanup`**: new recipe in `build-deploy`,
   `risk = "destructive"`, `confirm` warning, no params, same role list.

Icons reuse names already present in the catalog (`drive-harddisk-symbolic`,
`edit-clear-all-symbolic`) so no new icon-theme dependency is introduced.

## Out of scope (noted, not changed)

- `setup-rdp` missing from this host's justfile — host is behind, not a catalog defect.
- The justfile's interactive GPU menu offers `nvidia-legacy580` while every flake target
  in `/etc/nixos/flake.nix` is `nvidia-legacy535`. The catalog's `legacy535` choice
  matches the real flake outputs and is correct; the mismatch is upstream in vexos-nix.
- `switch`'s pre-existing optional `role`/`variant` prompt trap.

## Dependencies

None added. No external library work, so Context7 is not required for this change.

## Verification

1. `nix develop -c cargo fmt --all -- --check`
2. `nix develop -c cargo check --workspace`
3. `nix develop -c cargo clippy --workspace --all-targets`
4. `nix develop -c cargo test --workspace` — `catalog_matches_the_installed_justfile`
   must now pass on this host (it currently fails), with only the `setup-rdp`
   host-behind note printed.
5. GUI confirmation via `cage` + `grim` that the dashboard banner is gone.

`nix build .#default` is not required: nothing under `nix/`, `data/`, or the packaging
inputs changes.

## Risks and mitigations

- *A user leaves "Desktop environment" unchanged while switching a desktop host.* The
  rebuild stops at the justfile's prompt and exits non-zero without changing the system.
  Mitigated by the param help text and the existing "Needs vexos-nix update" badge.
- *`switch-bootloader` auto-confirms under `VEXOS_ASSUME_YES`.* Mitigated by a `confirm`
  string in the catalog so the GUI asks before the daemon is ever called.
- *Cleanup is destructive.* Marked `risk = "destructive"` (red button + polkit
  destructive tier) and the recipe itself refuses unless the host truly booted Limine.
