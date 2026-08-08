#!/usr/bin/env python3
"""Count Rust production lines after syntax-aware test removal."""

from __future__ import annotations

import argparse
import json
import re
from collections import defaultdict
from pathlib import Path

import tree_sitter_rust
from tree_sitter import Language, Parser


RUST = Language(tree_sitter_rust.language())
PARSER = Parser(RUST)
TEST_PARTS = {"test", "tests", "bench", "benches", "fuzz", "fuzzing"}
TEST_NAMES = re.compile(r"(^test(s)?|_test(s)?)\.rs$")
TEST_FEATURES = {
    "testing",
    "test-clarity-v1",
    "test-clarity-v2",
    "test-clarity-v3",
    "test-clarity-v4",
}
NANO_PRODUCT = {
    "nano-address",
    "nano-bitcoin",
    "nano-chainstate",
    "nano-codec",
    "nano-crypto",
    "nano-marf",
    "nano-mempool",
    "nano-miner",
    "nano-node",
    "nano-p2p",
    "nano-primitives",
    "nano-rpc",
    "nano-signer",
    "nano-sortition",
    "nano-stackerdb",
    "nano-sync",
    "nano-tui",
    "nano-vm",
    "nano-wasm-cache",
}
STACKS_PRODUCT = {
    "clarity",
    "clarity-types",
    "libsigner",
    "libstackerdb",
    "pox-locking",
    "stacks-codec",
    "stacks-common",
    "stacks-node",
    "stacks-profiler",
    "stacks-profiler-macros",
    "stacks-signer",
    "stackslib",
    "stx-genesis",
}


def is_test_path(path: Path) -> bool:
    return bool(TEST_PARTS.intersection(part.lower() for part in path.parts)) or bool(
        TEST_NAMES.search(path.name.lower())
    )


def is_test_attribute(text: str) -> bool:
    compact = re.sub(r"\s+", "", text)
    if compact in {"#[test]", "#[bench]", "#[rstest]"}:
        return True
    if compact.startswith("#[") and compact.endswith("::test]"):
        return True
    if not compact.startswith("#[cfg(") or not compact.endswith(")]"):
        return False
    condition = compact[6:-2]
    return cfg_without_tests(condition) is False


def cfg_without_tests(expression: str) -> bool | None:
    """Evaluate cfg with test-only atoms false and all other atoms unknown."""
    expression = expression.strip()
    if expression == "test":
        return False
    feature = re.fullmatch(r'feature="([^"]+)"', expression)
    if feature:
        return False if feature.group(1) in TEST_FEATURES else None
    call = re.fullmatch(r"(all|any|not)\((.*)\)", expression)
    if not call:
        return None
    operator, body = call.groups()
    arguments = split_cfg_arguments(body)
    values = [cfg_without_tests(argument) for argument in arguments]
    if operator == "not":
        return None if len(values) != 1 or values[0] is None else not values[0]
    if operator == "all":
        if False in values:
            return False
        return True if values and all(value is True for value in values) else None
    if True in values:
        return True
    return False if values and all(value is False for value in values) else None


def split_cfg_arguments(value: str) -> list[str]:
    arguments = []
    depth = 0
    start = 0
    for index, character in enumerate(value):
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        elif character == "," and depth == 0:
            arguments.append(value[start:index])
            start = index + 1
    arguments.append(value[start:])
    return [argument for argument in arguments if argument]


def walk(node):
    yield node
    for child in node.children:
        yield from walk(child)


def syntax_ranges(root, source: bytes) -> tuple[list[tuple[int, int]], list[tuple[int, int]]]:
    tests: list[tuple[int, int]] = []
    comments: list[tuple[int, int]] = []
    for node in walk(root):
        if node.type in {"line_comment", "block_comment"}:
            comments.append((node.start_byte, node.end_byte))
        if node.type != "attribute_item":
            continue
        if not is_test_attribute(source[node.start_byte : node.end_byte].decode("utf-8")):
            continue
        target = node.next_named_sibling
        while target is not None and target.type in {
            "attribute_item",
            "line_comment",
            "block_comment",
        }:
            target = target.next_named_sibling
        if target is not None:
            tests.append((node.start_byte, target.end_byte))
    return tests, comments


def mark(mask: bytearray, ranges: list[tuple[int, int]]) -> None:
    for start, end in ranges:
        mask[start:end] = b"\x01" * (end - start)


def line_counts(source: bytes, test_ranges, comment_ranges, whole_file_test: bool) -> tuple[int, int]:
    test_mask = bytearray(len(source))
    comment_mask = bytearray(len(source))
    if whole_file_test:
        test_mask[:] = b"\x01" * len(source)
    else:
        mark(test_mask, test_ranges)
    mark(comment_mask, comment_ranges)

    production = 0
    discarded = 0
    offset = 0
    for line in source.splitlines(keepends=True):
        line_range = range(offset, offset + len(line))
        production_here = False
        test_here = False
        for index in line_range:
            if source[index] in b" \t\r\n" or comment_mask[index]:
                continue
            if test_mask[index]:
                test_here = True
            else:
                production_here = True
        production += production_here
        discarded += test_here
        offset += len(line)
    return production, discarded


def scope(project: str, relative: Path) -> tuple[str, bool]:
    if project == "nano-stacks":
        if len(relative.parts) >= 2 and relative.parts[0] == "crates":
            crate = relative.parts[1]
            return crate, crate in NANO_PRODUCT
        if relative.parts[:3] == ("vendor", "clarity-wasm", "clar2wasm"):
            is_diagnostic_binary = relative.parts[3:5] == ("src", "bin")
            return "clarity-wasm", not is_diagnostic_binary
        return relative.parts[0], False
    group = relative.parts[0]
    return group, group in STACKS_PRODUCT


def measure(project: str, root: Path) -> dict:
    groups = defaultdict(lambda: {"files": 0, "production": 0, "tests": 0})
    parse_errors = []
    ignored = 0
    for path in sorted(root.rglob("*.rs")):
        relative = path.relative_to(root)
        if {".git", "target"}.intersection(relative.parts):
            continue
        group, included = scope(project, relative)
        if not included:
            ignored += 1
            continue
        source = path.read_bytes()
        tree = PARSER.parse(source)
        whole_file_test = is_test_path(relative)
        if tree.root_node.has_error and not whole_file_test:
            parse_errors.append(str(relative))
        tests, comments = syntax_ranges(tree.root_node, source)
        production, discarded = line_counts(source, tests, comments, whole_file_test)
        groups[group]["files"] += 1
        groups[group]["production"] += production
        groups[group]["tests"] += discarded

    total = {
        key: sum(group[key] for group in groups.values())
        for key in ("files", "production", "tests")
    }
    return {
        "project": project,
        "root": str(root),
        "method": (
            "tree-sitter-rust; test paths and syntax-only cfg/test items removed; "
            "blank and comment-only lines removed"
        ),
        "total": total,
        "groups": dict(sorted(groups.items())),
        "ignored_rust_files_outside_product_scope": ignored,
        "parse_errors": parse_errors,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("project", choices=("nano-stacks", "stacks-core"))
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    print(json.dumps(measure(args.project, args.root.resolve()), indent=2))


if __name__ == "__main__":
    main()
