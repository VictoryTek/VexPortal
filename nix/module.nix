# NixOS module for VexPortal.
#
# Wired into vexos-nix the same way `up` is: a flake input, then this module on the
# roles that have a display. Unlike a hand-written unit file, ExecStart here points at
# the daemon's real store path, so it survives every rebuild without a fixed
# /usr/libexec path that does not exist on NixOS.
self:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.vexportal;
in
{
  options.programs.vexportal = {
    enable = lib.mkEnableOption "VexPortal, the graphical front end for the vexos-nix justfile";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "vexportal.packages.\${system}.default";
      description = "The VexPortal package to install.";
    };

    justfile = lib.mkOption {
      type = lib.types.path;
      default = "/etc/nixos/justfile";
      description = ''
        The justfile the daemon runs recipes from. This is the only justfile it will
        ever execute, and it is set here rather than accepted over D-Bus so an
        unprivileged caller cannot redirect the daemon at a justfile of its own.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # Provides the bus name policy and the activation file under /share/dbus-1.
    services.dbus.packages = [ cfg.package ];

    # The polkit actions come from $out/share/polkit-1/actions via systemPackages.
    security.polkit.enable = true;

    systemd.services.vexportal-daemon = {
      description = "VexPortal privileged backend";
      documentation = [ "https://github.com/VictoryTek/VexPortal" ];

      serviceConfig = {
        Type = "dbus";
        BusName = "io.github.vexportal.Daemon";
        ExecStart = "${cfg.package}/libexec/vexportal-daemon --justfile ${cfg.justfile}";
        User = "root";

        # Recipes run nixos-rebuild, nix and sudo, so the usual filesystem and
        # privilege restrictions cannot apply to the daemon itself. What is left is
        # what does not interfere with that.
        NoNewPrivileges = false;
        ProtectSystem = false;
        ProtectHome = false;
        PrivateTmp = false;
        RestrictRealtime = true;
        RestrictSUIDSGID = false;

        Restart = "on-failure";
        RestartSec = 5;
      };

      # D-Bus activated: started on the first call and exits again when idle.
      wantedBy = [ ];
    };
  };
}
