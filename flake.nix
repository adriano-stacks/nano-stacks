{
  description = "nano-stacks development shell";

  # Pinned to a revision, not to a channel.
  #
  # `nixos-unstable` is a moving branch, and `flake.lock` was ignored -- so every
  # `nix develop` re-resolved it and could hand a different rustc to two runs a day
  # apart. A release report names the compiler that built the artifact, which is
  # worth nothing if a clean checkout picks a different one. The lock file is tracked
  # now and this is the revision it holds.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/b7c2ada94fe99c15b0dbcf4d11fd7850b957a436";

  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
    in {
      devShells = forEachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          # The toolchain comes from the pinned nixpkgs. `rust-toolchain.toml`
          # asks rustup for the same channel, for anyone outside this shell.
          default = pkgs.mkShell {
            packages = [
              pkgs.actionlint
              pkgs.cargo
              pkgs.clippy
              pkgs.curl
              pkgs.openssl
              pkgs.pkg-config
              pkgs.rustc
              pkgs.rustfmt
              pkgs.sqlite
            ];

            # Temporary files go to disk, not to RAM.
            #
            # `/` is a 16 GB tmpfs here, so everything under `/tmp` is memory. A
            # test that opens a `Vm` writes a MARF and a Clarity store into a
            # `tempfile::tempdir`, about 3.7 MB a time, and `Drop` is what removes
            # them -- which a killed process never runs. Two and a half thousand of
            # them had accumulated, 9.3 GB of RAM held by dead tests, and the
            # shortage that leaves is what kills the next process, which leaks the
            # next batch.
            #
            # Pointing `TMPDIR` at the 2 TB disk breaks the cycle: the leak still
            # happens on a kill, and it costs disk instead of the machine.
            shellHook = ''
              export TMPDIR="''${TMPDIR_OVERRIDE:-$HOME/.cache/nano-stacks/tmp}"
              mkdir -p "$TMPDIR"
            '';
          };
        });
    };
}
