# Running the release artifact

The release bundle contains one Nix-built `stacks-node`, its CycloneDX SBOM,
resolved Cargo feature tree, generated configuration schema, and the profiles in
this directory. Verify the signed manifest before installing any file.

## Prepare, qualify, and verify

The signing key stays outside the checkout. Its public key is the trust root an
operator obtains independently. Preparing a candidate runs the pinned RustSec
policy, records the clean Nix closure and copies the public package; it records
only the SHA-256 of `config.toml`, because that file can contain passwords and
private keys.

```sh
nix develop --command cargo xtask release-candidate prepare \
  --output /srv/nano-release/candidate \
  --checkpoint "$NANO_MAINNET_CHECKPOINT" \
  --config "$NANO_RELEASE_CONFIG" \
  --advisory-db "$NANO_RELEASE_ADVISORY_DB" \
  --secret-key "$NANO_RELEASE_SECRET_KEY" \
  --public-key "$NANO_RELEASE_PUBLIC_KEY"

nix develop --command cargo xtask release-report \
  --candidate /srv/nano-release/candidate \
  --public-key "$NANO_RELEASE_PUBLIC_KEY" \
  --capture "$NANO_MAINNET_CAPTURE" --state "$NANO_MAINNET_STATE" \
  > /srv/nano-release/qualification-report.txt

nix develop --command cargo xtask release-candidate finalize \
  --candidate /srv/nano-release/candidate \
  --report /srv/nano-release/qualification-report.txt \
  --secret-key "$NANO_RELEASE_SECRET_KEY" \
  --public-key "$NANO_RELEASE_PUBLIC_KEY"

nix develop --command cargo xtask release-candidate verify \
  --candidate /srv/nano-release/candidate \
  --public-key /path/from/a/trusted/channel/minisign.pub
```

Qualification verifies the preliminary signature and the external config and
checkpoint digests before and after every gate. Finalization refuses a report
that does not say `PASS` and name that exact preliminary manifest, then signs a
second complete checksum inventory containing the report. Any later byte or
extra file makes verification fail.

Before preparation, `scripts/reproducible-release.sh` builds the clean Git
revision in two separate rootless Nix stores, compares their NAR hashes and a
sorted SHA-256 inventory of every packaged file, and removes both stores. A NAR
match includes file contents, modes and paths; the separate inventory makes a
binary or packaged-data difference readable if the comparison ever fails.

## Capacity and shutdown contract

Budget at least 24 GiB of memory, 65,536 file descriptors, and 750 GiB of durable
disk. Keep temporary replay data on that disk rather than a tmpfs. The node seals
each accepted block before publishing progress. `SIGTERM` stops new work, drops
the role stores, and exits after already-sealed state is durable; service managers
allow 180 seconds before escalating. Logs go to the systemd journal or the
container runtime and must be rotated outside the process.

For systemd, install `nano-stacks.service`, replace `/usr/bin/stacks-node` with
the verified Nix-store binary or an immutable installed copy, put the generated
configuration at `/etc/nano-stacks/config.toml`, and keep state under
`/var/lib/nano-stacks`. The profile enforces the memory, descriptor, task,
filesystem, logging, restart, and shutdown limits above.

The flake's `container` output is the runnable OCI image. Run it with 24 GiB of
memory, 65,536 file descriptors, a 750 GiB bounded persistent volume mounted at
`/var/lib/nano-stacks`, a read-only configuration mounted at
`/etc/nano-stacks/config.toml`, and the runtime's log-size/rotation limits. For
example, the equivalent Docker limits are `--memory=24g --ulimit nofile=65536`
and `--storage-opt size=750G` where the storage driver supports it. Stop with
`SIGTERM` and allow at least 180 seconds before `SIGKILL`.
