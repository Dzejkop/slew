{
  description = "slew — a suspendable Lua 5.4 interpreter with embedder-controlled fuel budgets";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];

      forAllSystems = nixpkgs.lib.genAttrs systems;

      packageFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          lib = pkgs.lib;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "slew";
          version = "0.1.0";

          # Flake source is the git tree, so untracked `target/`/`result/`
          # never enter the store.
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.pkg-config ];

          meta = {
            description = "A suspendable Lua 5.4 interpreter with strict, embedder-controlled execution budgets";
            homepage = "https://github.com/Dzejkop/slew";
            license = with lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "slew";
          };
        };
    in
    {
      packages = forAllSystems (system: {
        default = packageFor system;
        slew = packageFor system;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = nixpkgs.lib.getExe (packageFor system);
        };
      });

      devShells = forAllSystems (system: {
        default = nixpkgs.legacyPackages.${system}.mkShell {
          inputsFrom = [ (packageFor system) ];

          packages = with nixpkgs.legacyPackages.${system}; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            pkg-config
          ];

          RUST_BACKTRACE = "1";
        };
      });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt-rfc-style);
    };
}
