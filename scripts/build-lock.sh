#!/usr/bin/env bash
# Serialize cargo across every worktree, because they do not share a target
# directory and four concurrent rustc fleets on a 31 GB machine is an OOM kill.
#
#   scripts/build-lock.sh cargo test -p nano-conformance
#
# Waits for the lock rather than failing, so a caller never has to retry.
exec flock /home/aldur/.nano-build.lock "$@"
