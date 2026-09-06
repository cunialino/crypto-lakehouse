{
  description = "A devShell example";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
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
        python = pkgs.python314.withPackages (ps: [
          ps.py4j
        ]);
        icebergVersion = "1.11.0";
        sparkCompat = "4.1";
        scalaVersion = "2.13";
        hadoopVersion = "3.4.2";
        awsSdkV2Version = "2.29.52";

        extraJars = [
          (pkgs.fetchurl {
            url = "https://repo1.maven.org/maven2/org/apache/iceberg/iceberg-spark-runtime-${sparkCompat}_${scalaVersion}/${icebergVersion}/iceberg-spark-runtime-${sparkCompat}_${scalaVersion}-${icebergVersion}.jar";
            sha256 = "sha256-1upsXQmSiNrrfVqSBhvT19jylkkmMrQjeOXy8OMGYkI=";
          })
          (pkgs.fetchurl {
            url = "https://repo1.maven.org/maven2/org/apache/iceberg/iceberg-aws/${icebergVersion}/iceberg-aws-${icebergVersion}.jar";
            sha256 = "sha256-TP5Ke6Ok+yR6KeYMYUZ4w7eh6bS5H1TgyjQGQZyTOPA=";
          })
          (pkgs.fetchurl {
            url = "https://repo1.maven.org/maven2/org/apache/hadoop/hadoop-aws/${hadoopVersion}/hadoop-aws-${hadoopVersion}.jar";
            sha256 = "sha256-0tJOa3FbVla50ji/E7ZoKgfiAE3Taq10pzrrhUMZ9Nk=";
          })
          (pkgs.fetchurl {
            url = "https://repo1.maven.org/maven2/software/amazon/awssdk/bundle/${awsSdkV2Version}/bundle-${awsSdkV2Version}.jar";
            sha256 = "sha256-YmOPMwRWkMa8XklrVLCy6Jqa04y+2s324Bf+pryF9Vw=";
          })
        ];

        spark = pkgs.stdenv.mkDerivation rec {
          pname = "spark";
          version = "4.1.2";

          src = pkgs.fetchzip {
            url = "https://downloads.apache.org/spark/spark-${version}/spark-${version}-bin-hadoop3.tgz";
            sha256 = "sha256-cI22md2A0Hy0bPqIiD7wO5MKQBxDiEOKAWEOHuBSfls=";
          };

          nativeBuildInputs = [ pkgs.makeWrapper ];

          installPhase = ''
            mkdir -p $out
            cp -r . $out/

            patchShebangs $out/bin

            for jar in ${pkgs.lib.concatStringsSep " " (map toString extraJars)}; do
              cp "$jar" "$out/jars/$(stripHash "$jar")"
            done

            for f in $out/bin/*; do
              if [ -f "$f" ] && [ -x "$f" ]; then
                wrapProgram "$f" \
                --set JAVA_HOME "${pkgs.jdk21}" --set SPARK_HOME "$out" \
                --set PYSPARK_PYTHON "${python}/bin/python3" \
                --set PYSPARK_DRIVER_PYTHON "${python}/bin/python3"
              fi
            done
          '';

          meta = {
            description = "Apache Spark ${version}";
            homepage = "https://spark.apache.org/";
            license = pkgs.lib.licenses.asl20;
          };
        };
      in
      let
        tableMaintenanceScript = pkgs.writeTextFile {
          name = "table_maintenance.py";
          destination = "/bin/table_maintenance.py";
          text = ''
            #!/usr/bin/env python3
            ${builtins.readFile ./deploy/scripts/table_maintenance.py}'';
          executable = true;
        };
        cryptoCollector = pkgs.rustPlatform.buildRustPackage {
          pname = "crypto-collector";
          version = "0.1.0";
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          src = self;
          cargoBuildFlags = [ "-p crypto-collector" ];
          inherit rustToolchain;
          inherit buildInputs;
          inherit nativeBuildInputs;

          PROTOC = "${pkgs.protobuf}/bin/protoc";
        };

        dockerRootfs = pkgs.symlinkJoin {
          name = "docker-rootfs";
          paths = [
            spark
            tableMaintenanceScript
            pkgs.bash
            pkgs.coreutils
            python
          ];
          postBuild = ''
            mkdir -p $out/etc
            echo "root:x:0:0:root:/root:/bin/bash" > $out/etc/passwd
            echo "root:x:0:" > $out/etc/group
          '';
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
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "SSL_CERT_DIR=${pkgs.cacert}/etc/ssl/certs"
            ];
          };
        };
        packages.dockerSpark = pkgs.dockerTools.buildImage {
          name = "spark";
          tag = "s0.1.3";
          copyToRoot = dockerRootfs;
          config = {
            Entrypoint = [
              "${spark}/bin/spark-submit"
              "--driver-memory"
              "4g"
              "/bin/table_maintenance.py"
            ];
            User = "root";
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "SSL_CERT_DIR=${pkgs.cacert}/etc/ssl/certs"
              "SPARK_HOME=${spark}"
              "PYSPARK_PYTHON=${python}/bin/python3"
              "HOME=/tmp"
              "HADOOP_USER_NAME=root"
              "PATH=${pkgs.bash}/bin:${pkgs.coreutils}/bin:${spark}/bin:${python}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            ];
          };
        };

        devShells.default =
          with pkgs;
          mkShell {
            nativeBuildInputs = [
              pkgs.pkg-config
            ];
            env = {
              UV_PYTHON = "${python}";
            };
            buildInputs = [
              rustToolchain
              spark
              pkgs.rust-analyzer
              pkgs.protobuf
              pkgs.openssl
            ];
            shellHook = ''
              export SPARK_LOCAL_IP=127.0.0.1
              export PYTHONPATH="''${PYTHONPATH:+$PYTHONPATH:}${spark}/python:${python}/lib/python3.14/site-packages"
            '';
          };
      }
    );
}
