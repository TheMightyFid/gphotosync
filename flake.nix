{
  description = "gphotosync - resumable Google Photos backup";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        # ponytail: image is always Linux; on macOS this needs a linux builder (nix.linux-builder).
        linuxSystem = builtins.replaceStrings [ "darwin" ] [ "linux" ] system;
        lpkgs = nixpkgs.legacyPackages.${linuxSystem};
        mkApp = p: p.rustPlatform.buildRustPackage {
          pname = "gphotosync";
          version = "0.1.0";
          src = p.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
        };
        app = mkApp lpkgs;
      in {
        packages.default = mkApp pkgs;
        packages.docker = lpkgs.dockerTools.buildLayeredImage {
          name = "gphotosync";
          tag = "latest";
          contents = [ app lpkgs.cacert lpkgs.tzdata ];
          config = {
            Entrypoint = [ "${app}/bin/gphotosync" ];
            Env = [
              "SSL_CERT_FILE=${lpkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "GPS_DATA=/data"
              "GPS_COOKIES=/config/cookies.txt"
            ];
            Volumes = { "/data" = { }; "/config" = { }; };
          };
        };
        devShells.default = pkgs.mkShell { packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt ]; };
      });
}
