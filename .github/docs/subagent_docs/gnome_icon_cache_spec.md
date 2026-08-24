# Spec: GNOME app grid shows generic placeholder instead of VexPortal icon

## Current state analysis

The icon pipeline inside this repo is correct end to end:

- `data/icons/hicolor/scalable/apps/io.github.vexportal.svg` is a well-formed SVG
  (verified: valid XML, single `<image>` element with an embedded base64 PNG, proper
  closing tags).
- `data/io.github.vexportal.desktop` sets `Icon=io.github.vexportal`, matching the SVG's
  basename.
- `nix/package.nix` `postInstall` installs the SVG to
  `$out/share/icons/hicolor/scalable/apps/io.github.vexportal.svg` and runs
  `gtk4-update-icon-cache -qtf $out/share/icons/hicolor` — this regenerates the
  `icon-theme.cache` **inside the VexPortal package's own store path**.
- `nix/module.nix` puts `cfg.package` in `environment.systemPackages`, which is how the
  desktop file and icon reach the system profile (`/run/current-system/sw/share/...`).

None of that is broken. The gap is downstream of it: `gtk4-update-icon-cache` in
`postInstall` only ever touches the cache file bundled inside VexPortal's own derivation.
It has no way to regenerate the *aggregated* `icon-theme.cache` for
`/run/current-system/sw/share/icons/hicolor` — the merged directory GNOME Shell actually
reads, built by symlinking every `environment.systemPackages` member's `share/icons`
together. Rebuilding that merged cache is a NixOS system-activation concern, controlled
by the option `gtk.iconCache.enable`.

Confirmed via the `nixos` MCP server (`search.nixos.org`, unstable channel):

```
Option: gtk.iconCache.enable
Type: boolean
Description: Whether to build icon theme caches for GTK applications.
Default: config.services.xserver.enable
```

`services.xserver.enable` and `services.desktopManager.gnome.enable` are independent,
confirmed-separate options (both returned by option search) — modern NixOS GNOME setups
commonly enable GNOME purely under Wayland via `services.desktopManager.gnome.enable` +
`services.displayManager.gdm.enable`, **without** `services.xserver.enable = true`. On
any such host, `gtk.iconCache.enable` defaults to `false`, so `nixos-rebuild switch`
never regenerates the aggregated hicolor icon cache — GNOME Shell keeps whatever it last
cached for `io.github.vexportal`, which is the generic placeholder from before the icon
existed.

## Problem definition

VexPortal's own module does not guarantee that the system-wide GTK icon cache is
rebuilt when it installs an icon-bearing package via `environment.systemPackages`. On
any host role where `services.xserver.enable` is false (Wayland-only GNOME), the new
icon is correctly built and shipped, but GNOME Shell's app grid never sees it, because
nothing regenerates `/run/current-system/sw/share/icons/hicolor/icon-theme.cache` on
switch.

This is a real gap in `nix/module.nix`, not a vexos-nix integration mistake — vexos-nix
only needs to import the module and set `programs.vexportal.enable = true`; it has no
reason to know that enabling VexPortal requires also flipping an unrelated GTK option.

## Proposed solution

`nix/module.nix` already forces one cross-cutting system option as a documented
side-effect of enabling VexPortal: `security.polkit.enable = true;` (needed for the
daemon's polkit actions to be usable). Add the same pattern for the icon cache:

```nix
config = lib.mkIf cfg.enable {
  environment.systemPackages = [ cfg.package ];
  services.dbus.packages = [ cfg.package ];
  security.polkit.enable = true;

  # Needed so the system-wide (aggregated) hicolor icon-theme.cache is rebuilt on
  # `nixos-rebuild switch`. `gtk4-update-icon-cache` in package.nix's postInstall only
  # regenerates the cache bundled inside VexPortal's own store path; it cannot touch
  # /run/current-system/sw/share/icons/hicolor, which is what GNOME Shell's app grid
  # actually reads. gtk.iconCache.enable defaults to services.xserver.enable, which is
  # false on Wayland-only GNOME hosts (services.desktopManager.gnome.enable without
  # services.xserver.enable) — leaving the app grid showing a stale/generic icon even
  # though the correct icon ships in the package.
  gtk.iconCache.enable = lib.mkDefault true;

  ...
};
```

Use `lib.mkDefault` (not a hard assignment) so a host that has an explicit opinion about
`gtk.iconCache.enable` (e.g. already forced `true`/`false` for its own reasons) is not
silently overridden — VexPortal only supplies the default that makes its own icon work.

## Implementation steps

1. Edit `nix/module.nix`: add `gtk.iconCache.enable = lib.mkDefault true;` inside the
   `config = lib.mkIf cfg.enable { ... }` block, alongside the existing
   `security.polkit.enable = true;` line, with the comment above explaining why.
2. No changes needed to `nix/package.nix`, `data/`, or catalog/daemon — the icon asset
   and its install path were already correct.

## Dependencies

None — `gtk.iconCache.enable` is a stock NixOS module option (verified present via the
`nixos` MCP `search`/`info` actions against the `unstable` channel); no new package or
external library involved. Context7 lookup is not applicable (no crates.io/npm-style
library API surface here).

## Configuration changes

`nix/module.nix` gains one `lib.mkDefault` line as described above. No option schema
changes, no new `programs.vexportal.*` options.

## Risks and mitigations

- **Risk:** forcing `gtk.iconCache.enable = true` on hosts that don't want it.
  **Mitigation:** `lib.mkDefault` means any explicit host-level setting (`true` or
  `false`) wins over this default; VexPortal only fills in the default when the host
  hasn't opined.
- **Risk:** this doesn't fully fix things if the *running* GNOME Shell session also
  needs a restart to pick up a rebuilt cache (GNOME Shell can cache app icon lookups for
  the life of the session). **Mitigation:** out of scope for a Nix module — note in the
  review/commit message that a full logout/login (or GNOME Shell restart) after
  `nixos-rebuild switch` may still be needed the first time, same as any icon/desktop
  file change.
- **Build validation risk:** this development machine is Windows (`win32`), and `nix`
  is not installed/on `PATH` here (verified: `which nix` → not found). The safe Phase 3
  commands (`nix develop -c cargo check --workspace`, `nix build .#default`, etc.) and
  Phase 6 preflight **cannot be executed on this host** — there is no NixOS/Linux target
  environment available. This must be disclosed rather than fabricating pass results;
  the change itself is a single-line, syntactically simple Nix attribute addition
  following the exact pattern already used one line above it in the same file
  (`security.polkit.enable = true;`), reviewed by inspection instead of by build.
