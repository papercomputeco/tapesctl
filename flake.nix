{
  description = "tapesctl development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    dagger.url = "github:dagger/nix";
    dagger.inputs.nixpkgs.follows = "nixpkgs";
    paper-skills.url = "github:papercomputeco/skills";
    paper-skills.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, dagger, paper-skills }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        skills = paper-skills.lib;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.go_1_25
            pkgs.gotools
            pkgs.gnumake
            dagger.packages.${system}.dagger
            pkgs.git
          ];

          shellHook =
            (skills.mkSkillsHook {
              skills = [ "dagger-check" ];
            })
            +
          ''
            echo "tapesctl development environment"
            echo ""
            echo "Go version: $(go version)"
            echo ""
            echo "Available make targets:"
            make help
          '';
        };
      }
    );
}
