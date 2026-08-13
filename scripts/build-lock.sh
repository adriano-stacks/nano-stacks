#!/usr/bin/env bash
# Serialize cargo across every worktree, because they do not share a target
# directory and four concurrent rustc fleets on a 31 GB machine is an OOM kill.
#
#   scripts/build-lock.sh cargo test -p nano-conformance
#
# Waits for the lock rather than failing, so a caller never has to retry.
# Keep the lock in `flock` itself, but close its descriptor in the command.
# Otherwise a command that starts a long-lived node, sink, or log follower can
# inherit the descriptor and hold the build queue forever after Cargo exits.
exec flock --close /home/aldur/.nano-build.lock "$@"
