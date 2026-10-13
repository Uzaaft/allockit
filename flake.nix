{
  description = "allockit: composable allocators for Rust's Allocator trait";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, ... }@inputs:
    let
      inherit (inputs.nixpkgs) lib;

      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      forEachSupportedSystem =
        f:
        lib.genAttrs supportedSystems (
          system:
          let
            pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [ inputs.rust-overlay.overlays.default ];
            };
            toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            craneLib = (inputs.crane.mkLib pkgs).overrideToolchain toolchain;
          in
          f { inherit system pkgs craneLib; }
        );

      workspace =
        craneLib:
        let
          src = craneLib.cleanCargoSource ./.;
          commonArgs = {
            inherit src;
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        {
          inherit src commonArgs cargoArtifacts;
        };
    in
    {
      checks = forEachSupportedSystem (
        { craneLib, ... }:
        let
          ws = workspace craneLib;
          args = ws.commonArgs // {
            inherit (ws) cargoArtifacts;
          };
        in
        {
          clippy = craneLib.cargoClippy (
            args
            // {
              cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
            }
          );
          test = craneLib.cargoTest (args // { cargoTestExtraArgs = "--workspace"; });
          doc = craneLib.cargoDoc (args // { cargoDocExtraArgs = "--workspace --no-deps"; });
          fmt = craneLib.cargoFmt { inherit (ws) src; };
        }
      );

      devShells = forEachSupportedSystem (
        {
          pkgs,
          system,
          craneLib,
        }:
        {
          default = craneLib.devShell {
            checks = self.checks.${system};
            packages = [ self.formatter.${system} ];
          };

          # Miri only ships on nightly. Use with `nix develop .#miri -c cargo miri test`.
          miri = pkgs.mkShell {
            packages = [
              (pkgs.rust-bin.selectLatestNightlyWith (
                toolchain:
                toolchain.minimal.override {
                  extensions = [
                    "miri"
                    "rust-src"
                  ];
                }
              ))
            ];
          };
        }
      );

      formatter = forEachSupportedSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
