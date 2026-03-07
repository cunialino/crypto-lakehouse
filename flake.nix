{
  description = "A devShell example";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.rust-bin.beta.latest.default;
        buildInputs = [
          pkgs.openssl
        ];
        nativeBuildInputs = [
          pkgs.pkg-config
        ];
      in
      let
        cryptoCollector = pkgs.rustPlatform.buildRustPackage {
          pname = "crypto-collector";
          version = "0.1.0";
          cargoLock = {
            lockFile = ./rustapps/crypto-collector/Cargo.lock;
          };
          src = ./rustapps/crypto-collector;
          cargoPatches = [ ];
          cargoCheckPaths = [
            "src"
            "Cargo.toml"
          ];
          inherit rustToolchain;
          inherit buildInputs;
          inherit nativeBuildInputs;
          PROTOC = "${pkgs.protobuf}/bin/protoc";
        };
      in
      {
        packages.default = cryptoCollector;

        packages.docker = pkgs.dockerTools.buildImage {
          name = "crypto-collector";
          tag = "latest";
          copyToRoot = pkgs.buildEnv {
            name = "docker-rootfs";
            paths = [
              cryptoCollector
              pkgs.openssl
              pkgs.stdenv.cc.cc.lib
            ];
          };
          config = {
            Entrypoint = [ "${cryptoCollector}/bin/crypto-collector" ];
          };
        };

        devShells.default =
          with pkgs;
          mkShell {
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.python3
            ];
            env = {
              UV_PYTHON = "${pkgs.python3}";
            };
            buildInputs = [
              rustToolchain
              pkgs.rust-analyzer
              pkgs.protobuf
              pkgs.openssl
            ];
          };
      }
    );
}
