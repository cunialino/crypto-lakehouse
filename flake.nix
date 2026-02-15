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
          pkgs.pkgsStatic.openssl
        ];
        nativeBuildInputs = [
          pkgs.pkg-config
        ];
      in
      {
        packages.default = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
          pname = "crypto-collector";
          version = "0.1.0";
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          src = ./.;
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

        devShells.default =
          with pkgs;
          mkShell {
            nativeBuildInputs = [
              pkgs.pkg-config
            ];
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
