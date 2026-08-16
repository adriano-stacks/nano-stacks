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

  outputs = { self, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
    in rec {
      devShells = forEachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          # The toolchain comes from the pinned nixpkgs. `rust-toolchain.toml`
          # asks rustup for the same channel, for anyone outside this shell.
          default = pkgs.mkShell {
            packages = [
              pkgs.actionlint
              pkgs.cargo
              pkgs.cargo-audit
              pkgs.cargo-cyclonedx
              pkgs.clippy
              pkgs.curl
              pkgs.jq
              pkgs.minisign
              pkgs.openssl
              pkgs.pkg-config
              pkgs.rustc
              pkgs.rustfmt
              pkgs.shellcheck
              pkgs.sqlite
              pkgs.zstd
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
              if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
                git config --local core.hooksPath .githooks
              fi
            '';
          };
        });

      packages = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          sourceRevision = self.rev or (self.dirtyRev or "dirty");
          stacksCoreVersions = pkgs.fetchurl {
            url = "https://raw.githubusercontent.com/stacks-network/stacks-core/efc34a07a225c4b950ab9404a1652aa5e14affaf/versions.toml";
            hash = "sha256-7mmEKZcNtkl13XfqevGUN8oPW+xHuTkkDiFdegbVRrY=";
          };
        in rec {
          stacks-node = pkgs.rustPlatform.buildRustPackage {
            pname = "nano-stacks";
            version = "0.1.0-${builtins.substring 0 12 sourceRevision}";
            src = self;
            cargoHash = "sha256-d1wUTZliCixSmbo0uMQKyiH9RKLxNdnCg1wp3U3ZtXg=";
            cargoBuildFlags = [ "-p" "nano-node" "--bin" "stacks-node" ];
            doCheck = false;

            nativeBuildInputs = [
              pkgs.cargo-cyclonedx
              pkgs.jq
              pkgs.pkg-config
            ];
            buildInputs = [ pkgs.openssl ];

            NANO_SOURCE_REVISION = sourceRevision;
            SOURCE_DATE_EPOCH = "1";

            # stacks-common's build script reads this repository-root file. Cargo's
            # standard git vendoring retains the crate but omits that sibling.
            postPatch = ''
              install -m644 ${stacksCoreVersions} \
                "$cargoDepsCopy/source-git-0/versions.toml"
            '';

            postBuild = ''
              cargo cyclonedx \
                --manifest-path crates/nano-node/Cargo.toml \
                --format json \
                --spec-version 1.5 \
                --all \
                --override-filename nano-stacks-sbom \
                --quiet
              cargo tree --locked --offline \
                -p nano-node \
                --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} \
                --edges normal,build,features \
                --charset ascii \
                > nano-stacks-dependencies.txt
            '';

            postInstall = ''
              install -Dm644 crates/nano-node/nano-stacks-sbom.json \
                "$out/share/nano-stacks/sbom.cdx.json"
              install -Dm644 nano-stacks-dependencies.txt \
                "$out/share/nano-stacks/dependencies.txt"
              install -Dm644 release/systemd/nano-stacks.service \
                "$out/share/nano-stacks/systemd/nano-stacks.service"
              install -Dm644 release/container/Containerfile \
                "$out/share/nano-stacks/container/Containerfile"
              install -Dm644 release/README.md \
                "$out/share/doc/nano-stacks/README.md"
              "$out/bin/stacks-node" config-schema \
                > "$out/share/nano-stacks/config.schema.json"
              "$out/bin/stacks-node" build-identity \
                > "$out/share/nano-stacks/build-identity.json"
            '';
          };

          container = pkgs.dockerTools.buildLayeredImage {
            name = "nano-stacks";
            tag = builtins.substring 0 12 sourceRevision;
            contents = [ stacks-node pkgs.cacert ];
            config = {
              Entrypoint = [ "${stacks-node}/bin/stacks-node" ];
              Cmd = [ "start" "--config" "/etc/nano-stacks/config.toml" ];
              Volumes = { "/var/lib/nano-stacks" = { }; };
              WorkingDir = "/var/lib/nano-stacks";
              StopSignal = "SIGTERM";
              Labels = {
                "org.opencontainers.image.revision" = sourceRevision;
                "org.opencontainers.image.source" = self.sourceInfo.url or "nano-stacks";
              };
            };
          };

          default = stacks-node;
        });
    };
}
