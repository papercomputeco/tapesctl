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
              # Fetch git dependencies by revision instead of by tree hash. The
              # revision in Cargo.lock is the whole identity of the source, so
              # `builtins.fetchGit` needs nothing else — which is why there is no
              # `outputHashes` block here.
              #
              # The tapes crates come from crates.io now and go through ordinary
              # cargo vendoring, but this setting is not theirs to retire: the
              # `[patch.crates-io]` libproc pin is still a git dependency, and
              # any temporary co-development git pin of a tapes crate (the
              # escape hatch in .github/dependabot.yml) rides through here too —
              # without needing a hash recomputed, which is what an
              # `outputHashes` block would demand on every rev change.
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "tapesctl" ];

            # What the built binary reports for `tapesctl version`. The build
            # script would otherwise ask git, and `cleanSource` has already
            # removed the `.git` directory by the time it runs — so without this
            # a Nix-built tapesctl could not say which commit it came from.
            # Empty for a dirty tree, where there is no revision to name.
            TAPESCTL_BUILD_SHA = self.rev or "";

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
