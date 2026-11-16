# UNTESTED (Nix is not installed where this was written). Build from source:
#   nix build ./packaging/nix   (the flake reads the repository two directories up)
# For `nix run github:OWNER/anchor`, move this file to the repository root and set src = ./.;
# then `nix flake lock` and commit flake.lock.
{
  description = "Anchor: local-first context engine and sync daemon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "anchor";
          version = "0.1.0";
          src = ../..;
          cargoLock.lockFile = ../../Cargo.lock;
          nativeBuildInputs = [ pkgs.installShellFiles ];
          # The test suite binds sockets and spawns processes; run it in CI instead.
          doCheck = false;
          postInstall = ''
            installShellCompletion --cmd anchor \
              --bash <($out/bin/anchor completions bash) \
              --zsh <($out/bin/anchor completions zsh) \
              --fish <($out/bin/anchor completions fish)
            $out/bin/anchor man > anchor.1
            installManPage anchor.1
          '';
          meta = {
            description = "Local-first context engine and sync daemon that grounds AI models in your codebases";
            homepage = "https://github.com/OWNER/anchor";
            # TODO(owner): set the real license, e.g. pkgs.lib.licenses.mit
            license = pkgs.lib.licenses.unfree;
            mainProgram = "anchor";
          };
        };
      });
    };
}
