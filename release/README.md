# Running the release artifact

The release bundle contains one Nix-built `stacks-node`, its CycloneDX SBOM,
resolved Cargo feature tree, generated configuration schema, and the profiles in
this directory. Verify the signed manifest before installing any file.

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
