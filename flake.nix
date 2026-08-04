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

      formatter = forEachSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
