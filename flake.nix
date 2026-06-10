{
  description = "herdr — terminal workspace manager for AI coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    guardrails.url = "github:gerchowl/guardrails";
  };

  outputs =
    { self, nixpkgs, guardrails }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          herdr = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit herdr;
          default = herdr;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/herdr";
          meta.description = "Run Herdr";
        };
      });

      checks = forAllSystems (system: {
        herdr = self.packages.${system}.default;
        default = self.checks.${system}.herdr;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          guardrailsToolbelt = guardrails.lib.${system}.toolbelt;
        in
        {
          default = pkgs.mkShell {
            name = "herdr-dev";
            packages =
              (with pkgs; [
                cargo
                cargo-nextest
                clippy
                cmake
                just
                ninja
                pkg-config
                rustc
                rustfmt
                zig_0_15
              ])
              ++ guardrailsToolbelt;

            env = {
              LIBGHOSTTY_VT_OPTIMIZE = "Debug";
              LIBGHOSTTY_VT_SIMD = "true";
            };

            shellHook = ''
              if [ -f .pre-commit-config.yaml ] && [ -d .git ]; then
                prek install >/dev/null 2>&1 || true
              fi
            '';
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);

      overlays.default = final: _prev: {
        herdr = final.callPackage ./nix/package.nix { };
      };
    };
}
