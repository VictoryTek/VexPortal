# VexPortal

A graphical front end for the [vexos-nix](https://github.com/VictoryTek/vexos-nix) justfile.

VexOS manages itself through a 3000-line justfile: rebuilds, role and GPU switching,
optional feature toggles, server service modules, generation rollback, VPN, binary
caches. VexPortal puts a GTK4 / libadwaita window in front of it that shows only the
operations applying to the role this machine is actually built as, replaces the
recipes' terminal prompts with forms, and runs everything through a polkit-guarded
system daemon so the application itself never holds root.

Built in Rust, distributed as a Nix flake.

## Design

### Role awareness

Everything hangs off `/etc/nixos/vexos-variant`. A `vexos-desktop-nvidia` host gets
Build & Deploy, Upgrades & Rollback, Features, Network & Remote and System; a
`vexos-server-amd` host also gets Server Services, Storage & Backup and Binary Cache.
Categories with nothing to show for the current role are hidden rather than disabled.

Set `VEXPORTAL_VARIANT=vexos-server-amd` to see another role's layout without
building it. It changes only what is listed — the daemon still decides what may run.

### The catalog

`just --dump --dump-format json` can enumerate the justfile, but not well enough to
drive a GUI: it keeps only the *last* comment line as a recipe's doc — which is why
`just --list` documents `backup-plex` as "suitable for moving to a new server" — it has
no notion of roles, and it cannot say whether a recipe is safe to run or will
repartition a disk.

So [`catalog/src/catalog.toml`](catalog/src/catalog.toml) carries titles, real
descriptions, icons, categories, roles, risk levels and argument widgets for all 45
recipes, and is compiled into both binaries. `just --dump` is still read at startup, for
two things it *is* authoritative about:

- the values behind the dynamic dropdowns (`_feature_names`, `_server_service_names`);
- **drift** — a recipe the catalog has not caught up with puts a banner on the window,
  and fails `cargo test`.

`/etc/nixos/justfile` is a copy taken by the last rebuild, so a host that has not
rebuilt since vexos-nix gained a recipe will not have it. VexPortal hides those recipes
and says so, rather than offering a card that would fail.

### Privilege

```
vexportal (user session, GTK4 + libadwaita)
  ├─ catalog.toml        compiled in
  ├─ variant.rs          /etc/nixos/vexos-variant → role + GPU
  ├─ just.rs             just --dump: dropdown values, drift check
  └─ zbus ──▶ io.github.vexportal.Daemon   (system bus, root)
                ├─ validate::build   recipe + every argument checked against the catalog
                ├─ auth.rs           polkit, tiered by the recipe's risk
                ├─ executor.rs       just --justfile /etc/nixos/justfile <recipe> <args…>
                ├─ cancel.rs         SIGTERM → SIGKILL on the process group
                └─ audit.rs          journald
```

The daemon has two methods, `RunRecipe` and `Cancel`. Neither takes a command line.
Arguments are passed to `just` as an argv — never through a shell — and each is checked
against a closed set of formats before `exec`, so a recipe name absent from the catalog
has no path to running at all. The justfile path comes from the systemd unit, not from
the caller. Secrets (the RDP password) travel to the child's stdin rather than argv, so
they never reach `ps`, the journal, or the audit line.

Three polkit tiers match the catalog's `risk` field:

| Risk | Action | Prompt |
| --- | --- | --- |
| `safe` | `io.github.vexportal.run-readonly` | none — `allow_active=yes` |
| `medium` | `io.github.vexportal.run-recipe` | `auth_admin_keep` |
| `destructive` | `io.github.vexportal.run-destructive` | `auth_admin`, never cached |

Making someone authenticate to answer "which GPU variant am I running?" teaches them to
click through prompts, which costs more security than it buys.

Everything the GUI can read without privileges — the variant, the generation, whether a
reboot is pending, the feature list, the flake age — it reads for itself. Asking a root
daemon to `cat` a world-readable file would be attack surface for no benefit.

### Prompts

Recipes that would stop at a `[y/N]` are given `VEXOS_ASSUME_YES=1`, and their answers
are collected up front in a form built from the catalog's parameter list. Until
vexos-nix honours that variable (see below), those recipes see EOF on stdin and take
their default answer, which is "no" for every confirmation in the file — so nothing
hangs, but the trailing "rebuild now?" step does not happen. Cards for those recipes
carry a **Needs vexos-nix update** badge.

Five recipes hold a real conversation — the storage wizards (`create-zfs-pool`,
`create-mergerfs-pool`, `attach-remote-storage`), `enable <service>`, and
`secrets-init`. Form-ifying a live partitioning wizard is not worth the risk, so those
are marked `terminal = true`: the daemon refuses to run them and the GUI opens a
terminal instead.

## Building

```sh
nix build .#default          # package, runs the test suite in the sandbox
nix develop                  # cargo, rustc, clippy, gtk4, libadwaita, just
nix develop -c cargo test --workspace
nix develop -c cargo clippy --workspace --all-targets -- -D warnings
```

The drift test needs a real `/etc/nixos/justfile` and a real `just`, neither of which
exists in the build sandbox; it detects that and skips. Run it on a VexOS host for it to
mean anything.

## Installing on a VexOS host

Not yet wired up. Two changes in vexos-nix, mirroring how `up` and `vexboard` are
already integrated:

1. **`flake.nix`** — add the input beside `up`:

   ```nix
   vexportal = {
     url = "github:VictoryTek/VexPortal";
     inputs.nixpkgs.follows = "nixpkgs";
   };
   ```

   then a module beside `upModule`, wired into the display-bearing roles (desktop,
   htpc, server, stateless) in the `roles` table:

   ```nix
   vexportalModule = {
     imports = [ inputs.vexportal.nixosModules.default ];
     programs.vexportal.enable = true;
   };
   ```

2. **The justfile** — five small, backwards-compatible changes so the form path is
   fully non-interactive. Every recipe still prompts exactly as it does today when run
   by a human without `VEXOS_ASSUME_YES`:

   - a `_confirm` helper that returns yes when `VEXOS_ASSUME_YES=1` and prompts
     otherwise, used for the confirm-only reads in `fix-flake`, `reset-defaults`,
     `restore-plex`, and the trailing "rebuild now?" / "reboot now?" steps in
     `set-hostname`, `switch` and `update`;
   - `update`: accept `role` / `variant` parameters, matching `switch`'s signature;
   - `setup-rdp`: read the password from stdin when stdin is not a TTY;
   - `enable <service>`: parameterize what can be, leaving the rest terminal-only.

## Layout

```
catalog/     recipe metadata, argument validation, drift detection — no GTK, no D-Bus
daemon/      the privileged backend
src/         the GTK4 application
nix/         package.nix and the NixOS module
data/        polkit actions, D-Bus policy, desktop entry, icon, stylesheet
```

`catalog/` is the interesting one: it is the allowlist. `validate::build` is what turns
a set of answers into an argv, and it is the same code in the GUI and the daemon.
