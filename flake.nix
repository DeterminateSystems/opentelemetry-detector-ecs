{
  description = "An OpenTelemetry resource detector for Amazon ECS";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/DeterminateSystems/nixpkgs-weekly/*";

    fenix.url = "https://flakehub.com/f/nix-community/fenix/*";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { nixpkgs, fenix, ... }:
    let
      inherit (nixpkgs) lib;

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
          in
          f { inherit pkgs toolchain; }
        );
    in
    {
      devShells = forEachSystem (
        { pkgs, toolchain }:
        {
          # A clang stdenv because that is what aws-lc-fips-sys's build, behind
          # the `fips` feature, is tested against.
          default = pkgs.mkShell.override { stdenv = pkgs.clangStdenv; } {
            name = "opentelemetry-detector-ecs";

            packages = [
              toolchain

              # The `fips` feature builds AWS-LC's FIPS module from source.
              pkgs.cmake
              pkgs.go
              pkgs.perl

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

      # Build the crate and run its tests on FIPS-validated crypto, so
      # `nix build .#checks.x86_64-linux.fips` (or aarch64-linux) exercises
      # a Linux build from any machine with a Linux builder.
      checks = forEachSystem (
        { pkgs, ... }:
        {
          fips = pkgs.rustPlatform.buildRustPackage.override { stdenv = pkgs.clangStdenv; } {
            pname = "opentelemetry-detector-ecs-fips";
            version = (lib.importTOML ./Cargo.toml).package.version;

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
              ];
            };

            cargoLock.lockFile = ./Cargo.lock;

            buildFeatures = [ "fips" ];

            # The default build makes a dylib and codesigns it on macOS, and
            # the sandbox has no codesign; the static library needs neither.
            AWS_LC_FIPS_SYS_STATIC = "1";

            nativeBuildInputs = [
              # aws-lc-fips-sys builds AWS-LC's FIPS module from source.
              pkgs.cmake
              pkgs.go
              pkgs.perl
            ];

            # cmake is above only for aws-lc-fips-sys's build script; the
            # crate itself configures with cargo.
            dontUseCmakeConfigure = true;
          };
        }
      );

      formatter = forEachSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
