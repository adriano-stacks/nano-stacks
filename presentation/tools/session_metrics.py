#!/usr/bin/env python3
"""Summarize whitelisted token counters without emitting session content."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator


DEFAULT_CUTOFF = "2026-08-08T15:17:53Z"
CLAUDE_ROOT = Path("/home/aldur/.claude/projects/-home-aldur-nano-stacks")
KIMI_ROOT = Path(
    "/home/aldur/.kimi-code/sessions/wd_nano-stacks_f7d444b0891a/"
    "session_7bb7b3bb-9309-44e7-9ecb-b79266e0a9e7"
)
CODEX_ROOT = Path("/home/aldur/.codex/sessions")
REPOSITORY = Path("/home/aldur/nano-stacks")


def instant(value: str | None) -> datetime:
    if not value:
        return datetime.min.replace(tzinfo=timezone.utc)
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("timestamps must include a UTC offset")
    return parsed


def counter(mapping: dict[str, Any], key: str) -> int:
    value = mapping.get(key, 0)
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"invalid token counter {key!r}")
    return value


def records(path: Path) -> Iterator[dict[str, Any]]:
    with path.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(record, dict):
                yield record


def claude(root: Path, cutoff: datetime) -> dict[str, Any]:
    fields = {
        "uncached_input": "input_tokens",
        "cache_creation_input": "cache_creation_input_tokens",
        "cached_input": "cache_read_input_tokens",
        "output": "output_tokens",
    }
    latest: dict[str, tuple[datetime, dict[str, int]]] = {}
    sessions: set[str] = set()
    contributing_files: set[Path] = set()
    raw_usage_rows = 0
    missing_identity_rows = 0
    paths = sorted(root.rglob("*.jsonl"))

    for path in paths:
        for record in records(path):
            timestamp = instant(record.get("timestamp"))
            if timestamp >= cutoff:
                continue
            message = record.get("message")
            if not isinstance(message, dict) or message.get("role") != "assistant":
                continue
            usage = message.get("usage")
            if not isinstance(usage, dict):
                continue
            raw_usage_rows += 1
            contributing_files.add(path)
            identity = record.get("requestId") or message.get("id")
            if not isinstance(identity, str) or not identity:
                missing_identity_rows += 1
                continue
            session = record.get("sessionId") or record.get("session_id")
            if isinstance(session, str) and session:
                sessions.add(session)
            counters = {name: counter(usage, key) for name, key in fields.items()}
            previous = latest.get(identity)
            if previous is None or timestamp >= previous[0]:
                latest[identity] = (timestamp, counters)

    totals = {
        name: sum(values[name] for _, values in latest.values()) for name in fields
    }
    return {
        "log_files_discovered": len(paths),
        "log_files_contributing": len(contributing_files),
        "sessions": len(sessions),
        "model_calls": len(latest),
        **totals,
        "total_recorded_traffic": sum(totals.values()),
        "duplicate_usage_rows_removed": raw_usage_rows - len(latest),
        "missing_identity_rows_ignored": missing_identity_rows,
    }


def kimi(root: Path, cutoff: datetime) -> dict[str, Any]:
    fields = {
        "uncached_input": "inputOther",
        "cache_creation_input": "inputCacheCreation",
        "cached_input": "inputCacheRead",
        "output": "output",
    }
    totals = dict.fromkeys(fields, 0)
    agents: set[str] = set()
    paths = sorted(root.glob("agents/*/wire.jsonl"))
    usage_records = 0

    for path in paths:
        for record in records(path):
            if record.get("type") != "usage.record" or record.get("usageScope") != "turn":
                continue
            timestamp = datetime.fromtimestamp(
                counter(record, "time") / 1000, timezone.utc
            )
            if timestamp >= cutoff:
                continue
            usage = record.get("usage")
            if not isinstance(usage, dict):
                continue
            usage_records += 1
            agents.add(path.parent.name)
            for name, key in fields.items():
                totals[name] += counter(usage, key)

    return {
        "sessions": int(bool(usage_records)),
        "agents": len(agents),
        "model_calls": usage_records,
        **totals,
        "total_recorded_traffic": sum(totals.values()),
    }


def codex(root: Path, repo: Path, cutoff: datetime) -> dict[str, Any]:
    fields = (
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    )
    session_totals: list[dict[str, int]] = []
    sessions = 0
    cumulative_snapshots = 0

    for path in sorted(root.glob("*/*/*/*.jsonl")):
        metadata: dict[str, Any] | None = None
        latest: tuple[datetime, dict[str, int]] | None = None
        for record in records(path):
            timestamp = instant(record.get("timestamp"))
            if record.get("type") == "session_meta" and metadata is None:
                metadata = record
            if timestamp >= cutoff:
                continue
            payload = record.get("payload")
            if not isinstance(payload, dict) or payload.get("type") != "token_count":
                continue
            info = payload.get("info")
            usage = info.get("total_token_usage") if isinstance(info, dict) else None
            if not isinstance(usage, dict):
                continue
            cumulative_snapshots += 1
            counters = {name: counter(usage, name) for name in fields}
            if latest is None or timestamp >= latest[0]:
                latest = (timestamp, counters)

        if metadata is None or instant(metadata.get("timestamp")) >= cutoff:
            continue
        payload = metadata.get("payload")
        cwd = payload.get("cwd") if isinstance(payload, dict) else None
        if not isinstance(cwd, str) or Path(cwd) != repo:
            continue
        sessions += 1
        if latest is not None:
            session_totals.append(latest[1])

    totals = {
        name: sum(usage[name] for usage in session_totals) for name in fields
    }
    if totals["total_tokens"] != totals["input_tokens"] + totals["output_tokens"]:
        raise ValueError("Codex total_tokens is not input_tokens + output_tokens")
    return {
        "sessions": sessions,
        "sessions_with_counters": len(session_totals),
        "sessions_without_counters": sessions - len(session_totals),
        "cumulative_snapshots_read": cumulative_snapshots,
        "input_including_cache": totals["input_tokens"],
        "cached_input_subset": totals["cached_input_tokens"],
        "output_including_reasoning": totals["output_tokens"],
        "reasoning_output_subset": totals["reasoning_output_tokens"],
        "uncached_input": totals["input_tokens"] - totals["cached_input_tokens"],
        "total_recorded_traffic": totals["total_tokens"],
    }


def summary(cutoff: datetime) -> dict[str, Any]:
    result = {
        "cutoff": cutoff.isoformat(),
        "scope": (
            "Whitelisted aggregate usage counters from nano-stacks project logs; "
            "message, prompt, tool, environment and credential bodies are not emitted"
        ),
        "claude": claude(CLAUDE_ROOT, cutoff),
        "kimi": kimi(KIMI_ROOT, cutoff),
        "codex": codex(CODEX_ROOT, REPOSITORY, cutoff),
    }
    result["combined_recorded_traffic"] = sum(
        result[vendor]["total_recorded_traffic"]
        for vendor in ("claude", "kimi", "codex")
    )
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cutoff", default=DEFAULT_CUTOFF)
    args = parser.parse_args()
    print(json.dumps(summary(instant(args.cutoff)), indent=2))


if __name__ == "__main__":
    main()
