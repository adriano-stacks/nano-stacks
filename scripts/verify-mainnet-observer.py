#!/usr/bin/env python3
"""Compare one nano new_block event with an independent mainnet oracle."""

import argparse
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path


class OracleUnavailable(Exception):
    pass


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "nano-stacks-release-gate"})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.read()
    except (OSError, urllib.error.URLError) as error:
        raise OracleUnavailable(f"oracle request failed for {url}: {error}") from error


def fetch_json(url: str) -> dict:
    return json.loads(fetch(url))


def fetch_transaction(base: str, txid: str) -> dict:
    url = f"{base}/extended/v1/tx/{txid}"
    document = fetch_json(f"{url}?event_limit=100")
    events = document["events"]
    while len(events) < document["event_count"]:
        page = fetch_json(f"{url}?event_limit=100&event_offset={len(events)}")["events"]
        if not page:
            raise ValueError(f"{txid} oracle event pagination stopped at {len(events)}")
        events.extend(page)
    document["events"] = events
    return document


def require_equal(actual, expected, label: str) -> None:
    if actual != expected:
        raise ValueError(f"{label}: nano {actual!r}, oracle {expected!r}")


def clarity_hex(value) -> str:
    if isinstance(value, str):
        return value.removeprefix("0x")
    return value["hex"].removeprefix("0x")


def local_event(event: dict) -> dict:
    kind = event["type"]
    if kind == "contract_event":
        body = event[kind]
        return {
            "kind": "contract_log",
            "contract": body["contract_identifier"],
            "topic": body["topic"],
            "value": clarity_hex(body["raw_value"]),
        }
    if kind == "stx_lock_event":
        body = event[kind]
        return {
            "kind": "stx_lock",
            "locked_address": body["locked_address"],
            "locked_amount": body["locked_amount"],
            "unlock_height": body["unlock_height"],
        }

    family, operation, _ = kind.split("_", 2)
    body = event[kind]
    normalized = {"kind": family, "operation": operation}
    fields = {
        "ft": ("asset_identifier", "sender", "recipient", "amount"),
        "nft": ("asset_identifier", "sender", "recipient", "raw_value"),
        "stx": ("sender", "recipient", "amount", "memo"),
    }[family]
    for field in fields:
        value = body.get(field)
        if value in (None, ""):
            continue
        name = {"asset_identifier": "asset", "raw_value": "value"}.get(field, field)
        if field in {"raw_value", "memo"}:
            value = clarity_hex(value)
        normalized[name] = value
    return normalized


def oracle_event(event: dict) -> dict:
    kind = event["event_type"]
    if kind == "smart_contract_log":
        body = event["contract_log"]
        return {
            "kind": "contract_log",
            "contract": body["contract_id"],
            "topic": body["topic"],
            "value": clarity_hex(body["value"]),
        }
    if kind == "stx_lock":
        body = event["stx_lock_event"]
        return {
            "kind": "stx_lock",
            "locked_address": body["locked_address"],
            "locked_amount": body["locked_amount"],
            "unlock_height": body["unlock_height"],
        }

    families = {
        "fungible_token_asset": "ft",
        "non_fungible_token_asset": "nft",
        "stx_asset": "stx",
    }
    family = families[kind]
    body = event["asset"]
    normalized = {"kind": family, "operation": body["asset_event_type"]}
    fields = {
        "ft": ("asset_id", "sender", "recipient", "amount"),
        "nft": ("asset_id", "sender", "recipient", "value"),
        "stx": ("sender", "recipient", "amount", "memo"),
    }[family]
    for field in fields:
        value = body.get(field)
        if value in (None, ""):
            continue
        name = {"asset_id": "asset"}.get(field, field)
        if field in {"value", "memo"}:
            value = clarity_hex(value)
        normalized[name] = value
    return normalized


def transaction_cost(document: dict) -> dict:
    return {
        "read_count": document["execution_cost_read_count"],
        "read_length": document["execution_cost_read_length"],
        "runtime": document["execution_cost_runtime"],
        "write_count": document["execution_cost_write_count"],
        "write_length": document["execution_cost_write_length"],
    }


def verify_transaction(base: str, block: dict, transaction: dict, events: list[dict]) -> None:
    txid = transaction["txid"]
    oracle = fetch_transaction(base, txid)
    require_equal(oracle["canonical"], True, f"{txid} canonical")
    require_equal(oracle["block_hash"], block["block_hash"], f"{txid} block hash")
    require_equal(oracle["block_height"], block["block_height"], f"{txid} block height")
    require_equal(oracle["tx_index"], transaction["tx_index"], f"{txid} index")
    require_equal(oracle["tx_status"], transaction["status"], f"{txid} status")
    require_equal(oracle["tx_result"]["hex"], transaction["raw_result"], f"{txid} result")
    require_equal(transaction["execution_cost"], transaction_cost(oracle), f"{txid} cost")

    local = [local_event(event) for event in events if event["txid"] == txid]
    remote = [oracle_event(event) for event in oracle["events"]]
    require_equal(oracle["event_count"], len(local), f"{txid} event count")
    require_equal(local, remote, f"{txid} events")


def verify(event_path: Path, base: str, expected_root: str) -> dict:
    block = json.loads(event_path.read_bytes())
    base = base.rstrip("/")
    oracle = fetch_json(f"{base}/extended/v2/blocks/{block['block_height']}")
    block_fields = {
        "height": "block_height",
        "hash": "block_hash",
        "index_block_hash": "index_block_hash",
        "burn_block_height": "burn_block_height",
        "parent_block_hash": "parent_block_hash",
        "parent_index_block_hash": "parent_index_block_hash",
    }
    require_equal(oracle["canonical"], True, "block canonical")
    for oracle_name, local_name in block_fields.items():
        require_equal(block[local_name], oracle[oracle_name], f"block {local_name}")
    require_equal(len(block["transactions"]), oracle["tx_count"], "block transaction count")
    require_equal(block["anchored_cost"], transaction_cost(oracle), "block cost")

    block_id = block["index_block_hash"].removeprefix("0x")
    raw = fetch(f"{base}/v3/blocks/{block_id}")
    if len(raw) < 133:
        raise ValueError(f"raw block has only {len(raw)} bytes")
    header_root = raw[101:133].hex()
    require_equal(expected_root.removeprefix("0x"), header_root, "verified state root")

    transactions = sorted(block["transactions"], key=lambda transaction: transaction["tx_index"])
    for transaction in transactions:
        verify_transaction(base, block, transaction, block["events"])

    return {
        "type": "oracle",
        "block_height": block["block_height"],
        "block_hash": block["block_hash"],
        "index_block_hash": block["index_block_hash"],
        "state_index_root": header_root,
        "transactions": len(transactions),
        "events": len(block["events"]),
        "oracle": base,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("event", type=Path)
    parser.add_argument("oracle")
    parser.add_argument("state_index_root")
    args = parser.parse_args()
    try:
        print(json.dumps(verify(args.event, args.oracle, args.state_index_root), sort_keys=True))
    except OracleUnavailable as error:
        print(error, file=sys.stderr)
        return 75
    except Exception as error:  # The release gate must turn every mismatch into failure.
        print(f"oracle verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
