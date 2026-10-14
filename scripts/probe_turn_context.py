#!/usr/bin/env python3
"""Live probe for the turn-context ledger request shapes (Phase 4).

Answers what the unit tests can't:
  1. Is `clear_at: "next_user_message"` accepted on a turn-scoped system
     message (beta mid-conversation-system-clear-at-2026-08-21), and does a
     cleared copy stop costing input tokens?
  2. Is `thinking.block_binding.prefix_mismatch_behavior: "drop_block"`
     (beta thinking-binding-controls-2026-08-01) accepted after a history
     edit, and what does `input_transformations` look like?
  3. Does a max_tokens:0 keep-alive ping work with both betas?
  4. What exact error text comes back when a beta header is missing or the
     model doesn't support mid-conversation system messages? claude.rs's
     fallback detectors match on that wording.

Usage: ANTHROPIC_API_KEY=... python3 scripts/probe_turn_context.py [model] [unsupported_model]

Request shapes mirror claude.rs: cached system block, ledger entries as text
blocks on the turn's user message (last one marked), the tail as a trailing
role:system message with clear_at. Prints status, usage and errors per call
and writes them to probe_turn_context_results.json. Costs cents.
"""

import json
import os
import sys
import urllib.error
import urllib.request

API_URL = "https://api.anthropic.com/v1/messages"
MODEL = sys.argv[1] if len(sys.argv) > 1 else "claude-opus-5-5"
UNSUPPORTED = sys.argv[2] if len(sys.argv) > 2 else "claude-sonnet-5"
CLEAR_AT_BETA = "mid-conversation-system-clear-at-2026-08-21"
BINDING_BETA = "thinking-binding-controls-2026-08-01"
CACHE = {"type": "ephemeral"}


PARAGRAPHS = [
    "Hobbes is a desktop assistant that keeps a daily plan. The plan lists todos "
    "with estimates, calendar events, and focus blocks, and it is reviewed each morning.",
    "A todo has a title, a project, an estimate in minutes, and a status. Done todos "
    "stay visible until the end of the day so the review can count them.",
    "Calendar events count against daily capacity when the user enables that setting. "
    "Focus blocks reserve time for deep work and can be moved during the day.",
    "The weekly review looks at what was planned, what was finished, and what slipped, "
    "and suggests a smaller plan when the same todos keep moving to the next day.",
]


def filler(n_words: int, seed: str) -> str:
    """Plain-English reference text (nonsense tokens triggered refusals)."""
    out, i = [], 0
    while sum(len(p.split()) for p in out) < n_words:
        out.append(f"({seed}{i}) " + PARAGRAPHS[i % len(PARAGRAPHS)])
        i += 1
    return "\n".join(out)


SYSTEM = [{
    "type": "text",
    "text": "You are Hobbes, a helpful assistant. Reference material follows.\n\n"
            + filler(4500, "s"),
    "cache_control": CACHE,
}]
LEDGER = ("[Context update: planner_today. Replaces earlier values.]\n"
          + json.dumps({"todos": filler(600, "p")}))


def tail(n: int) -> str:
    return (f"[Turn context]\n{{\"current_time\":\"2026-10-02T10:0{n}:00-07:00\"}}\n"
            "TURN NOTE: answer in one short sentence.\n" + filler(120, f"t{n}"))


def thinking(binding: str | None = None) -> dict:
    t = {"type": "adaptive", "display": "summarized"}
    if binding:
        t["block_binding"] = {"prefix_mismatch_behavior": binding}
    return t


def clean(blocks: list) -> list:
    """Echo response content back as request content (drop null fields)."""
    return [{k: v for k, v in b.items() if v is not None} for b in blocks]


def post(body: dict, key: str, betas: list[str]) -> dict:
    headers = {
        "x-api-key": key,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
    }
    if betas:
        headers["anthropic-beta"] = ",".join(betas)
    req = urllib.request.Request(
        API_URL, data=json.dumps(body, separators=(",", ":")).encode(),
        headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=180) as resp:
            j = json.loads(resp.read().decode())
            return {"status": resp.status, "stop_reason": j.get("stop_reason"),
                    "stop_details": j.get("stop_details"),
                    "usage": j.get("usage"), "content": j.get("content"),
                    "input_transformations": j.get("input_transformations")}
    except urllib.error.HTTPError as e:
        return {"status": e.code, "error": e.read().decode()}


def summarize(name: str, r: dict) -> None:
    print(f"\n{name}: HTTP {r['status']} stop_reason={r.get('stop_reason')}")
    if r.get("stop_details"):
        print("  stop_details: " + json.dumps(r["stop_details"]))
    if "error" in r:
        print("  error: " + r["error"])
        return
    u = r.get("usage") or {}
    print("  usage: input={} cache_read={} cache_write={} output={}".format(
        u.get("input_tokens"), u.get("cache_read_input_tokens"),
        u.get("cache_creation_input_tokens"), u.get("output_tokens")))
    if r.get("input_transformations"):
        print("  input_transformations: " + json.dumps(r["input_transformations"]))


def main() -> int:
    key = os.environ.get("ANTHROPIC_API_KEY")
    if not key:
        print("ANTHROPIC_API_KEY is not set", file=sys.stderr)
        return 2
    results = {"model": MODEL}

    def run(name, body, betas):
        r = post(body, key, betas)
        results[name] = r
        summarize(name, r)
        return r

    user1 = {"role": "user", "content": [
        {"type": "text", "text": "Give me a one-line status for today."},
        {"type": "text", "text": LEDGER, "cache_control": CACHE},
    ]}
    base = {"model": MODEL, "max_tokens": 1024, "stream": False,
            "system": SYSTEM, "thinking": thinking()}

    # 1. Turn 1: ledger on the user turn, tail as clear_at system message.
    t1 = {"role": "system", "content": tail(1), "clear_at": "next_user_message"}
    a = run("A turn1 clear_at", dict(base, messages=[user1, t1]), [CLEAR_AT_BETA])
    if a["status"] != 200 or a.get("stop_reason") == "refusal":
        print("\nTurn 1 failed or was refused; later calls depend on it. Stopping.")
        return 1
    assistant1 = {"role": "assistant", "content": clean(a["content"])}
    user2 = {"role": "user", "content": [
        {"type": "text", "text": "And tomorrow?", "cache_control": CACHE}]}

    # 2. Turn 2 with the turn-1 tail replayed cleared vs. replayed plain.
    #    If clearing works, B's input+read+write is smaller than B' by ~tail size.
    t2 = {"role": "system", "content": tail(2), "clear_at": "next_user_message"}
    run("B turn2 tail1 cleared", dict(base, messages=[user1, t1, assistant1, user2, t2]),
        [CLEAR_AT_BETA])
    t1_plain = {"role": "system", "content": tail(1)}
    run("B' turn2 tail1 not cleared",
        dict(base, messages=[user1, t1_plain, assistant1, user2, t2]), [CLEAR_AT_BETA])

    # 3. Keep-alive ping of B with the beta header (mirrors keepalive.rs).
    run("C ping max_tokens:0", dict(base, max_tokens=0,
        messages=[user1, t1, assistant1, user2, t2]), [CLEAR_AT_BETA])

    # 4. History edit with A's thinking replayed: drop_block should drop it.
    edited = json.loads(json.dumps(user1))
    edited["content"][0]["text"] = "Give me a two-line status for today."
    run("D edited history + drop_block",
        dict(base, thinking=thinking("drop_block"),
             messages=[edited, t1, assistant1, user2, t2]),
        [CLEAR_AT_BETA, BINDING_BETA])

    # 5. Error wording for the fallback detectors.
    run("E clear_at without beta header", dict(base, messages=[user1, t1]), [])
    run("F block_binding without beta header",
        dict(base, thinking=thinking("drop_block"), messages=[user1, t1_plain]), [])
    run(f"G role:system on {UNSUPPORTED}",
        {"model": UNSUPPORTED, "max_tokens": 256, "stream": False, "system": SYSTEM,
         "messages": [user1, t1_plain]}, [])

    for r in results.values():
        if isinstance(r, dict):
            r.pop("content", None)
    with open("probe_turn_context_results.json", "w") as f:
        json.dump(results, f, indent=2)
    print("\nWrote probe_turn_context_results.json")
    return 0


if __name__ == "__main__":
    sys.exit(main())
