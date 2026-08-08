#!/usr/bin/env python3
"""Measure two clean, pinned repository archives."""

from __future__ import annotations

import argparse
import io
import json
import subprocess
import tarfile
import tempfile
from pathlib import Path

from prod_loc import measure


NANO_REVISION = "eac1f89dd277cd2dde93df5ddce97ee88c840e45"
STACKS_CORE_REVISION = "efc34a07a225c4b950ab9404a1652aa5e14affaf"


def archive(repository: Path, revision: str, destination: Path) -> None:
    payload = subprocess.run(
        ["git", "-C", str(repository), "archive", "--format=tar", revision],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:") as bundle:
        bundle.extractall(destination, filter="data")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--nano", type=Path, default=Path("/home/aldur/nano-stacks"))
    parser.add_argument(
        "--stacks-core", type=Path, default=Path("/home/aldur/stacks-core")
    )
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="nano-stacks-loc-") as temporary:
        root = Path(temporary)
        nano = root / "nano-stacks"
        stacks_core = root / "stacks-core"
        nano.mkdir()
        stacks_core.mkdir()
        archive(args.nano.resolve(), NANO_REVISION, nano)
        archive(args.stacks_core.resolve(), STACKS_CORE_REVISION, stacks_core)
        result = {
            "method": "clean git archives measured with tree-sitter-rust",
            "nano_revision": NANO_REVISION,
            "stacks_core_revision": STACKS_CORE_REVISION,
            "nano_stacks": measure("nano-stacks", nano),
            "stacks_core": measure("stacks-core", stacks_core),
        }
        print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
