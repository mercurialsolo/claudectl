{
  description = "Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "claudectl";
          version = "0.16.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          meta = with pkgs.lib; {
            description = "Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you.";
            homepage = "https://github.com/mercurialsolo/claudectl";
            license = licenses.mit;
            mainProgram = "claudectl";
            platforms = platforms.unix;
          };
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
          ];
        };
      }
    );
}
