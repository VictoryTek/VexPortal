{ lib
, rustPlatform
, pkg-config
, wrapGAppsHook4
, glib
, gtk4
, libadwaita
, dbus
, hicolor-icon-theme
, just
}:

rustPlatform.buildRustPackage {
  pname = "vexportal";
  # The root package inherits its version from [workspace.package], so `.package.version`
  # is `{ workspace = true; }` rather than a string.
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "--workspace" ];

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    # glib-compile-resources, called by build.rs through glib-build-tools.
    glib
    # gtk4-update-icon-cache, called in postInstall.
    gtk4
  ];

  buildInputs = [
    gtk4
    libadwaita
    dbus
    hicolor-icon-theme
  ];

  # The drift test shells out to `just` against /etc/nixos/justfile, which does not
  # exist in the build sandbox; it detects that and skips. Everything else — catalog
  # parsing, argument validation, the ANSI parser, variant parsing — runs here.
  nativeCheckInputs = [ just ];

  # wrapGAppsHook4 bakes XDG_DATA_DIRS from buildInputs into the wrapper but does not
  # add $out/share, so without this GTK cannot find the icon installed below.
  preFixup = ''
    gappsWrapperArgs+=(--prefix XDG_DATA_DIRS : "$out/share")
  '';

  postInstall = ''
    install -Dm644 data/io.github.vexportal.desktop \
      $out/share/applications/io.github.vexportal.desktop
    install -Dm644 data/io.github.vexportal.metainfo.xml \
      $out/share/metainfo/io.github.vexportal.metainfo.xml
    install -Dm644 data/io.github.vexportal.policy \
      $out/share/polkit-1/actions/io.github.vexportal.policy
    install -Dm644 data/icons/hicolor/scalable/apps/io.github.vexportal.svg \
      $out/share/icons/hicolor/scalable/apps/io.github.vexportal.svg
    gtk4-update-icon-cache -qtf $out/share/icons/hicolor

    # The daemon is started by D-Bus activation, not by a user, so it lives in libexec
    # rather than bin.
    mkdir -p $out/libexec
    mv $out/bin/vexportal-daemon $out/libexec/vexportal-daemon

    install -Dm644 data/io.github.vexportal.Daemon.conf \
      $out/share/dbus-1/system.d/io.github.vexportal.Daemon.conf
    install -Dm644 data/io.github.vexportal.Daemon.service \
      $out/share/dbus-1/system-services/io.github.vexportal.Daemon.service
  '';

  meta = {
    description = "A graphical front end for the vexos-nix justfile";
    homepage = "https://github.com/VictoryTek/VexPortal";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "vexportal";
  };
}
