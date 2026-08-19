{
  description = "VexPortal — a graphical front end for the vexos-nix justfile";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages.default = pkgs.callPackage ./nix/package.nix { };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            pkg-config
            just          # the drift test shells out to it
            polkit        # pkcheck, for testing authorization by hand
          ];

          buildInputs = with pkgs; [
            gtk4
            libadwaita
            glib
            dbus
          ];
        };

        checks.default = self.packages.${system}.default;
      }) // {

      overlays.default = final: prev: {
        vexportal = final.callPackage ./nix/package.nix { };
      };

      # Installs the application, the polkit actions, the D-Bus policy, and the
      # system daemon the GUI talks to. Consumed by vexos-nix alongside `up`.
      nixosModules.default = import ./nix/module.nix self;
      nixosModules.vexportal = self.nixosModules.default;
    };
}
