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

Run it directly without installing anything:

```sh
nix run github:VictoryTek/VexPortal        # from anywhere
nix run .                                  # from a local checkout
```

Install it into your user profile:

```sh
nix profile install github:VictoryTek/VexPortal
```

Or add it to a NixOS system via the flake's module, alongside the polkit
actions, D-Bus policy, and the `vexportal-daemon` it depends on:

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
