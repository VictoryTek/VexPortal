# Handoff prompt — run this in ~/Projects/vexos-nix

I've built **VexPortal** (`~/Projects/VexPortal`, also `github:VictoryTek/VexPortal`): a
GTK4/libadwaita GUI for this repo's justfile. It's working — it builds with `nix build`,
renders the real variant/generation/features, and runs recipes through a polkit-guarded
system daemon. What's left is the vexos-nix side. Please do that work here.

Read `~/Projects/VexPortal/README.md` first for the full design. The short version:

- A root daemon (`vexportal-daemon`) runs `just --justfile /etc/nixos/justfile
  --working-directory /etc/nixos <recipe> <args…>`.
- Arguments are passed as **argv, never through a shell**, and are validated against a
  closed set of formats before exec.
- The recipe environment is built from scratch (`env_clear`), containing exactly:
  `PATH=/run/wrappers/bin:/run/current-system/sw/bin:/usr/bin:/bin`,
  **`VEXOS_ASSUME_YES=1`**, `VEXPORTAL=1`, `HOME=/root`, `LANG=C.UTF-8`,
  `NO_COLOR=1`, `TERM=dumb`.
- stdin is `/dev/null`, except for one secret written as a single `<value>\n` then closed.
- There is no TTY. stdout and stderr are pipes, streamed to the GUI line by line.

## Current behaviour (verified empirically, not assumed)

Every recipe sets `set -euo pipefail`, so a `read` that hits EOF behaves in one of two
ways. I tested both against the real loop bodies:

| Shape | Sites | Result today |
| --- | --- | --- |
| `read … \|\| true` | `switch:225` reboot, `set-hostname:846` rebuild, `fix-flake:997` rebuild | Empty answer → the `[y/N]` default (no). Operation completes, optional last step skipped. |
| plain `read` | `reset-defaults:710`, `restore-plex:1350`, `setup-rdp:742,744`, `update:347` | `set -e` fires → recipe exits 1. |

Note the `while [ -z "$ROLE" ]` loops in `switch:148` and `update:347` do **not** spin on
EOF — `set -e` exits the loop body's failing `read`. I confirmed this; no infinite loop.

So nothing hangs and nothing answers on the user's behalf today. The work below is about
making the form path actually complete, not about fixing a hang.

## Task 1 — a `_confirm` helper

Add a helper that returns yes immediately when `VEXOS_ASSUME_YES=1` and prompts
otherwise. Route these through it:

- `switch:225` "Reboot now? [y/N]"
- `set-hostname:846` "Rebuild now to apply fully via NixOS? [y/N]"
- `fix-flake:997` "Rebuild now to apply? [y/N]"
- `reset-defaults:710` "Continue? [y/N]"
- `restore-plex:1350` "Type 'yes' to continue:" — this one is a typed-keyword
  confirmation, so decide deliberately whether `VEXOS_ASSUME_YES` should satisfy it. My
  view: yes, because VexPortal already shows its own destructive confirmation dialog
  before calling, but it's your call and worth a comment either way.

**Hard requirement:** without `VEXOS_ASSUME_YES` set, every one of these must prompt
exactly as it does today. A human running `just reset-defaults` in a terminal sees no
change whatsoever.

## Task 2 — `update` takes parameters

`update` currently prompts for role and variant (`justfile:347,367,386`) when
`/etc/nixos/vexos-variant` is missing — the stateless-reboot case its own comment
mentions. Give it `role=""` and `variant=""` parameters matching `switch`'s existing
signature, used when the variant file is absent.

Accepted values, taken from `switch`: roles `desktop stateless htpc server
headless-server vanilla`; GPU `amd nvidia intel vm`, plus `nvidia-legacy535` which
`switch` accepts directly when passed as an argument.

When you do this, tell me — VexPortal's catalog declares `update` with no parameters
today and its drift test will fail until I add them on my side. That failure is the
feature working, not a problem.

## Task 3 — `setup-rdp` reads the password from stdin

`setup-rdp:741-751` loops asking for a password and a confirmation. VexPortal sends one
line; the confirm read then hits EOF and the recipe exits 1. I verified this: it does
**not** write an empty password, it just fails.

Make it read the password once from stdin when stdin is not a TTY (`[ -t 0 ]`), skipping
the confirmation — there is nothing to typo when the value came from a form. Keep the
current two-read loop verbatim when stdin *is* a TTY.

Do not accept the password via an environment variable or an argument; both are visible
in `/proc` and in `ps`.

## Task 4 — audit `enable <service>`

`enable` has 11 interactive reads (`justfile:2043-2900`) asking for real values: a
Proxmox IP and NIC name, a restic repository path and password file, an Arcane public
URL, a Matrix server name, a Zigbee serial device, ZFS pool creation, disk selection.

VexPortal marks this one `terminal = true` and opens a terminal instead — I don't think a
form should drive disk selection. Have a look and tell me whether any of those services
could reasonably take parameters instead; if some can, say which and I'll surface those
in the GUI as forms. No change needed if the answer is "leave it".

## Task 5 — wire the flake

Mirror how `up` and `vexboard` are already integrated.

In `flake.nix`, beside the `up` input (~line 33):

```nix
vexportal = {
  url = "github:VictoryTek/VexPortal";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Then beside `upModule` (~line 83):

```nix
vexportalModule = {
  imports = [ inputs.vexportal.nixosModules.default ];
  programs.vexportal.enable = true;
};
```

and add it to the display-bearing roles in the `roles` table — desktop, htpc, server,
stateless — the same set `upModule` goes to. Headless-server has no display, so leave it
out for the same reason `up` is left out.

The module installs the app, the polkit actions, the D-Bus policy, and a systemd unit
whose `ExecStart` is the real store path. It has a `programs.vexportal.justfile` option
defaulting to `/etc/nixos/justfile`; that path is set in the unit rather than accepted
over D-Bus, deliberately, so leave it at the default.

## Constraints

- Follow this repo's `CLAUDE.md` — surgical changes, preflight before delivery.
- Every change must be backwards compatible: a human at a terminal sees identical
  behaviour. `VEXOS_ASSUME_YES` is opt-in and nothing else sets it.
- Do not rebuild or switch the system as part of this work. Tell me when it's ready and
  I'll run it.

## How to verify

```sh
just --dump --dump-format json | jq '.recipes.update.parameters'   # Task 2 landed

# Task 1 — same recipe, both ways round:
just fix-flake                    # must still prompt
VEXOS_ASSUME_YES=1 just fix-flake # must not

# Task 3 — exactly what the daemon does:
printf 'testpassword\n' | just setup-rdp   # must succeed, writing testpassword

# Task 5:
nix flake check
nix eval .#nixosConfigurations.vexos-desktop-nvidia.config.programs.vexportal.enable

./scripts/preflight.sh
```

For anything driven by the daemon, the closest reproduction of the real environment is:

```sh
env -i PATH=/run/wrappers/bin:/run/current-system/sw/bin:/usr/bin:/bin \
    VEXOS_ASSUME_YES=1 VEXPORTAL=1 HOME=/root LANG=C.UTF-8 NO_COLOR=1 TERM=dumb \
    just --justfile /etc/nixos/justfile --working-directory /etc/nixos <recipe> < /dev/null
```

Note `/etc/nixos/justfile` is the copy from the last rebuild and currently lags this repo
by ~270 lines, so test against `./justfile` while developing.
