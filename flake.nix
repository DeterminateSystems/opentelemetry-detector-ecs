{
  description = "An OpenTelemetry resource detector for Amazon ECS";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/DeterminateSystems/nixpkgs-weekly/*";

    fenix.url = "https://flakehub.com/f/nix-community/fenix/*";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
  };

  # The flake supplies the tools. The Justfile decides what to run with them.
  outputs =
    { nixpkgs, fenix, ... }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
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
