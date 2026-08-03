{
  description = "An OpenTelemetry resource detector for Amazon ECS";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/DeterminateSystems/nixpkgs-weekly/*";

    fenix.url = "https://flakehub.com/f/nix-community/fenix/*";
    fenix.inputs.nixpkgs.follows = "nixpkgs";

    crane.url = "https://flakehub.com/f/ipetkov/crane/*";
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      crane,
    }:
    let
      inherit (nixpkgs) lib;

      # The systems DeterminateCI maps to a runner by default.
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];

      forEachSystem =
        f:
        lib.genAttrs systems (
          system:
          let
            pkgs = nixpkgs.legacyPackages.${system};

            toolchain =
              with fenix.packages.${system};
              combine [
                stable.cargo
                stable.clippy
                stable.rust-src
                stable.rustc
                stable.rustfmt
              ];

            craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

            # Crane keeps only the Rust sources by default, and the tests read
            # their fixtures with `include_str!`.
            src = lib.cleanSourceWith {
              src = ./.;
              filter = path: type: lib.hasSuffix ".json" path || craneLib.filterCargoSources path type;
            };

            common = {
              inherit src;
              strictDeps = true;
            };

            cargoArtifacts = craneLib.buildDepsOnly common;
          in
          f {
            inherit
              pkgs
              toolchain
              craneLib
              common
              cargoArtifacts
              ;
          }
        );
    in
    {
      checks = forEachSystem (
        {
          pkgs,
          craneLib,
          common,
          cargoArtifacts,
          ...
        }:
        {
          build = craneLib.cargoBuild (common // { inherit cargoArtifacts; });

          clippy = craneLib.cargoClippy (
            common
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          doc = craneLib.cargoDoc (
            common
            // {
              inherit cargoArtifacts;
              env.RUSTDOCFLAGS = "--deny warnings";
            }
          );

          rustfmt = craneLib.cargoFmt { inherit (common) src; };

          test = craneLib.cargoTest (common // { inherit cargoArtifacts; });

          # These three read the whole tree, and each reports paths relative to
          # the working directory, so run them from inside it.
          editorconfig = pkgs.runCommand "check-editorconfig" { } ''
            cd ${self}
            ${lib.getExe pkgs.eclint} .
            touch $out
          '';

          nixfmt = pkgs.runCommand "check-nixfmt" { } ''
            cd ${self}
            find . -name '*.nix' -print0 \
              | xargs -0 ${lib.getExe pkgs.nixfmt} --check
            touch $out
          '';

          typos = pkgs.runCommand "check-typos" { } ''
            cd ${self}
            ${lib.getExe pkgs.typos} .
            touch $out
          '';
        }
      );

      devShells = forEachSystem (
        { pkgs, toolchain, ... }:
        {
          default = pkgs.mkShell {
            name = "opentelemetry-detector-ecs";

            packages = [
              toolchain

              pkgs.cargo-audit
              pkgs.cargo-machete
              pkgs.cargo-outdated
              pkgs.eclint
              pkgs.jq
              pkgs.just
              pkgs.nixfmt
              pkgs.rust-analyzer
              pkgs.typos
            ];

            env.RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
          };
        }
      );

      formatter = forEachSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
