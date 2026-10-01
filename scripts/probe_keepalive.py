#!/usr/bin/env python3
"""Live probe: is a `max_tokens: 0` keep-alive ping accepted with adaptive
thinking, and does a `stream: false` ping read a cache entry that a streamed
request wrote?

Usage: ANTHROPIC_API_KEY=... python3 scripts/probe_keepalive.py [model]

Calls (same body each time, ~6k-token cached system prompt):
  A) stream:true,  max_tokens 1024  -- the real turn; expect a cache write
  B) stream:false, max_tokens 0     -- expect 200, empty content,
                                       stop_reason max_tokens, cache read ~= A's write
  C) stream:false, max_tokens 1     -- the fallback path
  D) B rebuilt with the same transform as keepalive.rs build_ping_body()
     (stream:false, max_tokens:0, drop "diagnostics")

Prints status, stop_reason and full usage per call and writes them to
probe_keepalive_results.json. Costs cents.
"""

import json
import os
import sys
import urllib.error
import urllib.request

API_URL = "https://api.anthropic.com/v1/messages"
MODEL = sys.argv[1] if len(sys.argv) > 1 else "claude-opus-5-5"  # ClaudeModel::DEFAULT_CHAT_SLUG
# Models where Hobbes sends display:"summarized" (ClaudeModel::thinking_always_on).
ALWAYS_ON = ("claude-opus-5-5", "claude-fable-5-1")


def filler(n_words: int) -> str:
    words = ["hobbes", "keeps", "the", "prompt", "cache", "warm", "while",
             "the", "user", "is", "away", "from", "their", "desk"]
    return " ".join(words[i % len(words)] + str(i % 97) for i in range(n_words))


def real_body() -> dict:
    thinking = {"type": "adaptive"}
    if MODEL.startswith(ALWAYS_ON):
        thinking["display"] = "summarized"
    return {
        "model": MODEL,
        "max_tokens": 1024,
        "stream": True,
        "system": [{
            "type": "text",
            "text": "You are Hobbes, a helpful assistant. Reference material follows.\n\n"
                    + filler(4500),
            "cache_control": {"type": "ephemeral"},
        }],
        "messages": [{"role": "user", "content": "Say hello in five words."}],
        "thinking": thinking,
        "tools": [{
            "name": "hobbes_core__HOBBES_LIST_TIMERS",
            "description": "List the timers set in this session.",
            "input_schema": {"type": "object", "properties": {}},
        }],
        "tool_choice": {"type": "auto"},
    }


def build_ping_body(real: dict, max_tokens: int) -> dict:
    """Mirror of keepalive.rs build_ping_body."""
    v = json.loads(json.dumps(real))
    v["stream"] = False
    v["max_tokens"] = max_tokens
    v.pop("diagnostics", None)
    return v


def post(body: dict, key: str) -> dict:
    req = urllib.request.Request(
        API_URL,
        data=json.dumps(body, separators=(",", ":")).encode(),
        headers={
            "x-api-key": key,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            status, raw = resp.status, resp.read().decode()
    except urllib.error.HTTPError as e:
        return {"status": e.code, "error": e.read().decode()}

    if not body.get("stream"):
        j = json.loads(raw)
        return {"status": status, "stop_reason": j.get("stop_reason"),
                "content": j.get("content"), "usage": j.get("usage")}

    # SSE: usage arrives in message_start and is completed by message_delta.
    usage, stop = {}, None
    for line in raw.splitlines():
        if not line.startswith("data:"):
            continue
        ev = json.loads(line[5:])
        if ev.get("type") == "message_start":
            usage.update(ev["message"].get("usage", {}))
        elif ev.get("type") == "message_delta":
            usage.update({k: v for k, v in ev.get("usage", {}).items() if v is not None})
            stop = ev.get("delta", {}).get("stop_reason", stop)
    return {"status": status, "stop_reason": stop, "usage": usage}


def main() -> int:
    key = os.environ.get("ANTHROPIC_API_KEY")
    if not key:
        print("ANTHROPIC_API_KEY is not set", file=sys.stderr)
        return 2
    real = real_body()
    b = dict(real, stream=False, max_tokens=0)
    calls = [
        ("A stream:true max_tokens:1024", real),
        ("B stream:false max_tokens:0", b),
        ("C stream:false max_tokens:1", dict(real, stream=False, max_tokens=1)),
        ("D build_ping_body(real, 0)", build_ping_body(real, 0)),
    ]
    results = {"model": MODEL}
    for name, body in calls:
        r = post(body, key)
        results[name] = r
        print(f"{name}: HTTP {r['status']} stop_reason={r.get('stop_reason')}")
        print("  " + json.dumps(r.get("usage") or r.get("error")))
    with open("probe_keepalive_results.json", "w") as f:
        json.dump(results, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
