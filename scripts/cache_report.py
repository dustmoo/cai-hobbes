#!/usr/bin/env python3
"""Prompt-cache report: is 5m + keep-alive better than the 1h TTL?

Reads Hobbes' usage_log.json (read-only) and prints cache hit rates, an
idle-gap histogram, keep-alive spend vs the writes it saved, and a
recommendation. Standard library only.

    scripts/cache_report.py [path/to/usage_log.json]

Default path: ~/Library/Application Support/com.hobbes.app/usage_log.json
(macOS) or ~/.config/com.hobbes.app/usage_log.json.
"""
import json
import os
import sys

BUCKETS = [("0-5m", 0, 300), ("5-15m", 300, 900), ("15-30m", 900, 1800),
           ("30-60m", 1800, 3600), ("60m+", 3600, float("inf"))]
# Multipliers on the base input price (Anthropic prompt caching).
READ, WRITE_5M, WRITE_1H = 0.10, 1.25, 2.00


def default_path():
    mac = os.path.expanduser("~/Library/Application Support/com.hobbes.app/usage_log.json")
    return mac if os.path.exists(mac) else os.path.expanduser("~/.config/com.hobbes.app/usage_log.json")


def pct(n, d):
    return "n/a" if not d else f"{100.0 * n / d:.1f}%"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else default_path()
    with open(path) as f:
        entries = json.load(f).get("entries", [])

    turns = [e for e in entries if e.get("kind", "turn") == "turn" and e.get("cache")]
    pings = [e for e in entries if e.get("kind") == "keepalive"]
    print(f"{path}: {len(entries)} entries, {len(turns)} turns with cache telemetry, {len(pings)} keep-alive pings\n")
    if not turns and not pings:
        print("No cache telemetry yet. Use Hobbes with a Claude connector for a few days, then re-run.")
        return

    # Hit rate: turn-first request vs the rest of the turn.
    first = [t["cache"] for t in turns if t["cache"].get("first_cache_read") is not None]
    first_hits = sum(1 for c in first if (c["first_cache_read"] or 0) > (c.get("first_cache_write") or 0))
    w_reads = sum(t["cache"].get("within_turn_cache_read") or 0 for t in turns)
    w_writes = sum(t["cache"].get("within_turn_cache_write") or 0 for t in turns)
    print("Hit rate")
    print(f"  turn-first request : {pct(first_hits, len(first))} ({first_hits}/{len(first)} read more than they wrote)")
    print(f"  within a turn      : {pct(w_reads, w_reads + w_writes)} of cached tokens were reads")

    # Idle-gap histogram (turn-first requests with a known gap).
    print("\nIdle gap before a turn (previous request start -> this one)")
    gaps = [c for c in first if c.get("idle_gap_secs") is not None]
    print(f"  {'gap':8} {'turns':>6} {'hit':>7} {'bridged by keep-alive':>22}")
    for name, lo, hi in BUCKETS:
        b = [c for c in gaps if lo <= c["idle_gap_secs"] < hi]
        hits = sum(1 for c in b if (c["first_cache_read"] or 0) > (c.get("first_cache_write") or 0))
        bridged = sum(1 for c in b if c.get("keepalive_bridged"))
        print(f"  {name:8} {len(b):6d} {pct(hits, len(b)):>7} {bridged:22d}")

    # Keep-alive spend vs estimated savings.
    spend = sum(e.get("cost") or 0 for e in pings)
    saved = 0.0
    for c in gaps:
        if c.get("keepalive_bridged") and c["idle_gap_secs"] > 300 and c.get("input_price_per_mtok"):
            # A bridged gap would otherwise have rewritten the prefix at 1.25x
            # instead of reading it at 0.1x.
            saved += (c.get("first_cache_read") or 0) * c["input_price_per_mtok"] * (WRITE_5M - READ) / 1e6
    print("\nKeep-alive economics")
    print(f"  ping spend            : ${spend:.4f} over {len(pings)} pings")
    print(f"  est. write savings    : ${saved:.4f} (bridged gaps >5m x prefix x (1.25-0.10) x input price)")
    print(f"  net                   : ${saved - spend:+.4f}")

    # 1h TTL what-if: every long-but-<=60m gap would be a read instead of a
    # write, but every write costs 2x instead of 1.25x.
    if first:
        price = next((c["input_price_per_mtok"] for c in first if c.get("input_price_per_mtok")), None)
        if price:
            writes = sum((c.get("first_cache_write") or 0) for c in first) + w_writes
            extra_1h = writes * price * (WRITE_1H - WRITE_5M) / 1e6
            saved_1h = sum((c.get("first_cache_write") or 0) * price * (WRITE_5M - READ) / 1e6
                           for c in gaps if 300 <= c["idle_gap_secs"] < 3600
                           and (c["first_cache_read"] or 0) <= (c.get("first_cache_write") or 0))
            print("\n1h TTL what-if (price from the log; approximate)")
            print(f"  extra write premium   : ${extra_1h:.4f} (all writes at 2x instead of 1.25x)")
            print(f"  misses avoided        : ${saved_1h:.4f} (5-60m gaps that missed)")
            print(f"  net vs plain 5m       : ${saved_1h - extra_1h:+.4f}")
            keepalive_net = saved - spend
            print("\nRecommendation")
            if len(first) < 20:
                print("  Not enough turns yet (<20) to decide; keep 5m + keep-alive and re-run later.")
            elif saved_1h - extra_1h > max(keepalive_net, 0):
                print("  1h TTL looks cheaper than 5m + keep-alive for this usage.")
            elif keepalive_net > 0:
                print("  5m + keep-alive is paying for itself; keep it.")
            else:
                print("  Neither keep-alive nor 1h clearly pays off; plain 5m is fine.")


if __name__ == "__main__":
    main()
