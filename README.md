# VexPortal

A GTK4 / libadwaita front end for the [vexos-nix](https://github.com/VictoryTek/vexos-nix) justfile.

Shows only the operations that apply to the role a machine is actually built as,
replaces terminal prompts with forms, and runs everything through a polkit-guarded
system daemon so the app itself never holds root.

Built in Rust, distributed as a Nix flake.

## Build

```sh
nix build .#default                      # package, runs tests in the sandbox
nix develop -c cargo test --workspace
```

## Install / Run

`nix run` and `nix profile install` only build and launch the `vexportal` GUI
binary — they do not install the `vexportal-daemon` systemd unit, its D-Bus
activation file, or the polkit actions. Without those, the window opens but
every action fails with "The name is not activatable", because there is no
daemon on the system bus for it to call. These two methods are for browsing
the UI only:

```sh
nix run github:VictoryTek/VexPortal        # from anywhere
nix run .                                  # from a local checkout

nix profile install github:VictoryTek/VexPortal
```

### vexos-nix hosts

If you're on a [vexos-nix](https://github.com/VictoryTek/vexos-nix) host built
from the `desktop`, `htpc`, `stateless`, or `server` role, **skip the steps
above** — vexos-nix's own flake already declares `inputs.vexportal` and
imports `vexportal.nixosModules.default` with `programs.vexportal.enable = true;`
for those roles (see `vexportalModule` in vexos-nix's `flake.nix`). Adding it
again in your system flake declares `programs.vexportal.enable` twice and
`nixos-rebuild` fails with "already declared". Just rebuild:

```sh
sudo nixos-rebuild switch --flake /etc/nixos#$(cat /etc/nixos/vexos-variant)
```

`headless-server` and `vanilla` roles don't get it (no display), and the
manual steps above are the way to add it there if wanted.

Then rebuild the system so the daemon, D-Bus policy, and polkit actions are
actually installed — `programs.vexportal.enable = true;` on its own changes
nothing until this runs:

```sh
# Permanent: installs it and sets it as the boot default.
sudo nixos-rebuild switch --flake .#<hostname>

# Temporary, for testing: activates it right now, but reverts to the
# previous generation on the next reboot — nothing is committed.
sudo nixos-rebuild test --flake .#<hostname>

# To undo a `test` activation immediately instead of waiting for a reboot:
sudo nixos-rebuild switch --rollback
```

After either `switch` or `test`, `vexportal-daemon` is D-Bus-activated on
demand — you don't start it by hand. Launch the GUI as usual
(`vexportal`, or from the app grid) and actions will work.

## Layout

```
catalog/     recipe metadata, argument validation, drift detection
daemon/      the privileged backend (D-Bus, polkit, executor)
src/         the GTK4 application
nix/         package.nix and the NixOS module
data/        polkit actions, D-Bus policy, desktop entry, icon, stylesheet
```

## Status

Wired into vexos-nix for the desktop, htpc, stateless, and server roles — see
`docs/` for the integration plan and remaining work.

## License

MIT
