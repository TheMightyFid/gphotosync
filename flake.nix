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
        # Image is always Linux. On macOS cross-compile to aarch64-linux so no Linux builder is needed.
        lpkgs = if pkgs.stdenv.isDarwin then pkgs.pkgsCross.aarch64-multiplatform else pkgs;
        mkApp = p: p.rustPlatform.buildRustPackage {
          pname = "gphotosync";
          version = "0.1.0";
          src = p.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
        };
        mkImage = lp: let app = mkApp lp; in lp.dockerTools.buildLayeredImage {
          name = "gphotosync";
          tag = "latest";
          contents = [ app lp.cacert ];
          config = {
            User = "1000:1000";
            Entrypoint = [ "${app}/bin/gphotosync" ];
            Env = [
              "SSL_CERT_FILE=${lp.cacert}/etc/ssl/certs/ca-bundle.crt"
              "GPS_DATA=/data"
              "GPS_COOKIES=/config/cookies.txt"
            ];
            Volumes = { "/data" = { }; "/config" = { }; };
          };
        };
        # Images are always Linux; on macOS cross-compile so no Linux builder is needed.
        native = if pkgs.stdenv.isLinux then pkgs else pkgs.pkgsCross.aarch64-multiplatform;
        amd64 = if system == "x86_64-linux" then pkgs else pkgs.pkgsCross.gnu64;
      in {
        packages.default = mkApp pkgs;
        packages.docker = mkImage native;
        # x86_64 image, e.g. for Unraid.
        packages.docker-amd64 = mkImage amd64;
        devShells.default = pkgs.mkShell { packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt ]; };
      });
}
