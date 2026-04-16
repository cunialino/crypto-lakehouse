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

        # 1. Consolidate shared dependencies to keep build and devShell in sync
        baseBuildInputs = [
          pkgs.openssl
          pkgs.rdkafka.dev # Provides system librdkafka
          pkgs.zlib # Required by rdkafka
          pkgs.cyrus_sasl # Required by rdkafka for auth
        ];

        baseNativeBuildInputs = [
          pkgs.pkg-config
          pkgs.cmake # Required to build the rdkafka-sys bindings
          pkgs.protobuf # Required for prost-build
        ];
      in
      let
        cryptoCollector = pkgs.rustPlatform.buildRustPackage {
          pname = "crypto-collector";
          version = "0.1.0";

          # 1. Nix uses this for vendoring (downloading crates)
          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          # 2. This is the code it will build
          src = ./rustapps/crypto-collector;

          # 3. FIX: Copy the workspace lockfile into the source root
          # so the 'Validating consistency' check passes.
          postPatch = ''
            cp ${./Cargo.lock} Cargo.lock
          '';

          inherit rustToolchain;
          buildInputs = baseBuildInputs;
          nativeBuildInputs = baseNativeBuildInputs;

          # Build environment
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          CARGO_FEATURE_DYNAMIC_LINKING = "1";
        };
      in
      {
        packages.default = cryptoCollector;

        packages.dockerDF = pkgs.dockerTools.buildImage {
          name = "datafusion-cli";
          tag = "0.1.0";
          copyToRoot = pkgs.buildEnv {
            name = "docker-rootfs";
            paths = [
              pkgs.datafusion-cli
            ];
          };
        };

        packages.docker = pkgs.dockerTools.buildImage {
          name = "crypto-collector";
          tag = "latest";
          copyToRoot = pkgs.buildEnv {
            name = "docker-rootfs";
            paths = [
              cryptoCollector
              pkgs.openssl
              pkgs.cacert # 4. CRITICAL: Allows Rust to verify cert-manager TLS certs
              pkgs.stdenv.cc.cc.lib
            ];
          };
          config = {
            Entrypoint = [ "${cryptoCollector}/bin/crypto-collector" ];
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "SSL_CERT_DIR=${pkgs.cacert}/etc/ssl/certs"
            ];
          };
        };

        devShells.default =
          with pkgs;
          mkShell {
            # 5. Append python to the base native inputs
            nativeBuildInputs = baseNativeBuildInputs ++ [
              pkgs.python3
            ];
            # 6. Append Rust tooling to the base inputs
            buildInputs = baseBuildInputs ++ [
              rustToolchain
              pkgs.rust-analyzer
            ];
            # 7. Ensure your shell has the exact same PROTOC path as the builder
            shellHook = ''
              export PROTOC="${pkgs.protobuf}/bin/protoc"
            '';
            env = {
              UV_PYTHON = "${pkgs.python3}";
            };
          };
      }
    );
}
