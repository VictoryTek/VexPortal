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
