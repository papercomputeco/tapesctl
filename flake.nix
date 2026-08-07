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
                # The shared harness crate and the generated cassette-surface
                # machinery split out of this repo under PCC-1104. They are two
                # packages in ONE repository, and Cargo.toml now pins both at the
                # same revision — so these two hashes are the hash of that single
                # revision's tree, and are expected to be equal. A future diff
                # where they differ means the two pins have drifted apart again.
                #
                # Recompute when the pins in Cargo.toml re-point to the
                # merged main revision:
                #   nix shell nixpkgs#nix-prefetch-git -c nix-prefetch-git \
                #     https://github.com/papercomputeco/tapes-harnesses --rev <sha>
                # `nix build` fails on a stale value even though the cargo-native
                # targets never notice, which is how two earlier pin bumps landed
                # without a recompute.
                "tapes-harnesses-0.1.0" = "sha256-bDlgmlpRC3c3MEsJx5ulWUaLbgDX4mMh1E3Q8sn6IvY=";
                "tapes-cassette-client-0.1.0" = "sha256-bDlgmlpRC3c3MEsJx5ulWUaLbgDX4mMh1E3Q8sn6IvY=";
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
