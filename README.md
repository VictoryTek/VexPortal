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

To actually run recipes, add it to a NixOS system via the flake's module,
which wires up the polkit actions, D-Bus policy, and the `vexportal-daemon`
it depends on. In your system's `flake.nix`:

```nix
{
  inputs.vexportal.url = "github:VictoryTek/VexPortal";

  outputs = { self, nixpkgs, vexportal, ... }: {
    nixosConfigurations.<hostname> = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        vexportal.nixosModules.default
        { programs.vexportal.enable = true; }
        ./configuration.nix
      ];
    };
  };
}
```

After adding the input, lock it so `flake.lock` picks up the new
`vexportal` entry — flakes refuse to build against an input that isn't
locked yet:

```sh
nix flake lock --update-input vexportal
```

### vexos-nix hosts

If your `/etc/nixos/flake.nix` is the [vexos-nix](https://github.com/VictoryTek/vexos-nix)
template wrapper (`inputs.vexos-nix`, an `outputs = { self, vexos-nix, nixpkgs }:`
lambda, and a shared `hardwareModule` line in every variant's `modules` list),
this `sed` wires VexPortal into all variants in one shot instead of hand-editing:

```sh
sudo sed -i \
  -e '/vexos-nix\.url = "github:VictoryTek\/vexos-nix";/a\    vexportal.url = "github:VictoryTek/VexPortal";' \
  -e 's/outputs = { self, vexos-nix, nixpkgs }:/outputs = { self, vexos-nix, nixpkgs, vexportal }:/' \
  -e '/^          hardwareModule$/a\          vexportal.nixosModules.default\n          { programs.vexportal.enable = true; }' \
  /etc/nixos/flake.nix

nix flake lock --update-input vexportal --flake /etc/nixos
sudo nixos-rebuild switch --flake /etc/nixos#$(cat /etc/nixos/vexos-variant)
```

This edits `/etc/nixos/flake.nix` in place, so diff or back it up first if you
want to review the change before rebuilding. It's also local-only: a fresh
`curl` of `etc-nixos-flake.nix` from vexos-nix will overwrite it.

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

Not yet wired into vexos-nix — see `docs/` for the integration plan.

## License

MIT
