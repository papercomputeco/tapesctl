{
  description = "tapesctl — the Tapes client CLI (Rust)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    dagger.url = "github:dagger/nix";
    dagger.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, dagger }:
    {
      overlays.default = final: prev:
        let
          # Build with the same toolchain the devShell and CI pin via
          # rust-toolchain.toml, not whatever Rust nixpkgs-unstable ships.
          rust = (import rust-overlay final prev).rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = final.makeRustPlatform {
            cargo = rust;
            rustc = rust;
          };
        in
        {
          tapesctl = rustPlatform.buildRustPackage {
            pname = "tapesctl";
            version = self.shortRev or "dev";
            src = final.lib.cleanSource self;

            cargoLock = {
              lockFile = ./Cargo.lock;
              # Required for the `[patch.crates-io]` libproc git pin: Nix cannot
              # infer a hash for a git dependency from the lockfile alone. Same
              # crate, same rev, same hash as `platform/paper`.
              outputHashes = {
                "libproc-0.14.11" = "sha256-B4mZIbjn1FOsTJXqyv3DRXAE3FFwT/4Gl+GDP4r9+9M=";
                # The shared harness crate, consumed by git pin since the
                # repo split. Recompute when the pin rev bumps.
                "tapes-harnesses-0.1.0" = "sha256-4ptJDQYrBCq0KyksSFOscwQ6/1Ub5RZrVr7IFwYYUHM=";
              };
            };
            cargoBuildFlags = [ "-p" "tapesctl" ];

            # Workspace tests are exercised in CI (`make test`); keep the package
            # build focused on producing the CLI.
            doCheck = false;
          };
        };
    }
    //
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system; overlays = overlays ++ [ self.overlays.default ]; };
        # Pin the toolchain to rust-toolchain.toml so `nix develop` and bare
        # `cargo` stay in lockstep.
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        packages = {
          default = pkgs.tapesctl;
          tapesctl = pkgs.tapesctl;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rust
            pkgs.gnumake
            pkgs.git
            # Dagger drives the CI/release pipeline (`.dagger/`): lint, test, and
            # the cargo-zigbuild cross-compile of all four release targets.
            dagger.packages.${system}.dagger
          ];

          shellHook = ''
            echo "tapesctl development environment (Rust)"
            echo ""
            echo "Rust version: $(rustc --version)"
            echo ""
            echo "Available make targets:"
            make help
          '';
        };
      }
    );
}
