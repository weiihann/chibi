#!/usr/bin/env python3
"""Compares a run's recorded account history with the public oracle's logs.

Usage: history-check.py RUN_DIR ADDRESS [ORACLE_URL]

Over every block range chibi says it covered for ADDRESS's logs, every log the oracle has
that names ADDRESS (as emitter, or as topic 1, 2 or 3) must be in chibi's history, and
chibi must hold no such log the oracle lacks. Exits 1 on any difference. Reads chibi.db
read-only, so it works beside a running node. The oracle is a test input only. The default
is Tenderly's public Sepolia gateway: publicnode refuses eth_getLogs without a contract
address, and these queries filter by topic alone.
"""
import json
import sqlite3
import sys
import urllib.request
from pathlib import Path

LOGS_CAPABILITY = 2
CHUNK = 2000


def oracle_logs(url, address, first, last):
    padded = "0x" + "0" * 24 + address[2:].lower()
    filters = [{"address": address}] + [
        {"topics": [None] * position + [padded]} for position in (1, 2, 3)
    ]
    found = set()
    start = first
    while start <= last:
        end = min(start + CHUNK - 1, last)
        for extra in filters:
            params = {"fromBlock": hex(start), "toBlock": hex(end), **extra}
            body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "eth_getLogs", "params": [params]}).encode()
            request = urllib.request.Request(url, body, {"content-type": "application/json", "user-agent": "curl/8"})
            reply = json.load(urllib.request.urlopen(request, timeout=120))
            if "error" in reply:
                raise SystemExit(f"oracle error for {start}-{end}: {reply['error']}")
            for log in reply["result"]:
                found.add((int(log["blockNumber"], 16), int(log["logIndex"], 16)))
        start = end + 1
    return found


def chibi_logs(db, account, first, last):
    padded = bytes(12) + account
    rows = db.execute(
        "SELECT number, log_index FROM history_log WHERE number BETWEEN ? AND ? "
        "AND (address = ? OR topic1 = ? OR topic2 = ? OR topic3 = ?)",
        (first, last, account, padded, padded, padded),
    )
    return {(number, index) for number, index in rows}


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    run_dir, address = Path(sys.argv[1]), sys.argv[2]
    oracle = sys.argv[3] if len(sys.argv) > 3 else "https://sepolia.gateway.tenderly.co"
    account = bytes.fromhex(address[2:])
    databases = list((run_dir / "home").glob(".local/share/chibi/*/chibi.db"))
    if len(databases) != 1:
        raise SystemExit(f"expected one chibi.db under {run_dir}/home, found {len(databases)}")
    db = sqlite3.connect(f"file:{databases[0]}?mode=ro", uri=True)
    ranges = db.execute(
        "SELECT first, last FROM history_coverage WHERE account = ? AND capability = ? ORDER BY first",
        (account, LOGS_CAPABILITY),
    ).fetchall()
    if not ranges:
        raise SystemExit(f"no logs coverage for {address} yet")
    failed = False
    for first, last in ranges:
        theirs = oracle_logs(oracle, address, first, last)
        ours = chibi_logs(db, account, first, last)
        missing, extra = sorted(theirs - ours), sorted(ours - theirs)
        print(f"blocks {first}-{last}: oracle {len(theirs)} logs, chibi {len(ours)}, missing {len(missing)}, extra {len(extra)}")
        for block, index in missing[:20]:
            print(f"  missing block {block} log {index}")
        for block, index in extra[:20]:
            print(f"  extra block {block} log {index}")
        failed = failed or bool(missing or extra)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
