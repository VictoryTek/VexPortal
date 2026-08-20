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
it depends on:

```nix
{
  inputs.vexportal.url = "github:VictoryTek/VexPortal";

  # in your system configuration:
  imports = [ inputs.vexportal.nixosModules.default ];
}
```

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
