#!/usr/bin/env python3
"""Manage the Hobbes session archive (sessions-archive.jsonl).

The archive is append-only JSONL: one Session object per line. It holds
sessions recovered from old sessions.json snapshots/backups. Duplicate
session ids are resolved last-line-wins.

Since the SQLite migration, live sessions are stored in sessions.db —
sessions.json is a legacy file the app imported once and no longer reads.

Subcommands:
  list                       List archived sessions (newest first).
  merge <state.json> [...]   Merge sessions out of full session-state JSON
                             files (sessions.json snapshots/backups) into the
                             archive. Inputs are processed in the order given,
                             so pass oldest first — a newer copy of the same
                             session id wins. Sessions already live in
                             sessions.db are skipped.
  restore <session-id>       Copy a session from the archive back into the
                             live sessions.db. Quit Hobbes first!

Run with the app's config dir auto-detected, or override with --config-dir.
"""

import argparse
import json
import os
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path


def default_config_dir() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "com.hobbes.app"
    if sys.platform == "win32":
        return Path(os.environ["APPDATA"]) / "com.hobbes.app"
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "com.hobbes.app"


def open_db(config_dir: Path) -> sqlite3.Connection:
    db_path = config_dir / "sessions.db"
    if not db_path.exists():
        print(f"Live session store not found at {db_path}.", file=sys.stderr)
        print("Launch Hobbes once so it creates sessions.db, then retry.", file=sys.stderr)
        sys.exit(1)
    return sqlite3.connect(db_path)


def live_session_ids(config_dir: Path) -> set:
    with open_db(config_dir) as conn:
        return {row[0] for row in conn.execute("SELECT id FROM sessions")}


def read_archive(archive_path: Path) -> dict:
    """Return {session_id: session_dict}, last line wins, skipping bad lines."""
    sessions = {}
    if not archive_path.exists():
        return sessions
    with open(archive_path, encoding="utf-8") as f:
        for lineno, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                session = json.loads(line)
                sessions[session["id"]] = session
            except (json.JSONDecodeError, KeyError, TypeError):
                print(f"  warning: skipping unparseable archive line {lineno}", file=sys.stderr)
    return sessions


def append_to_archive(archive_path: Path, sessions: list) -> None:
    with open(archive_path, "a", encoding="utf-8") as f:
        for session in sessions:
            f.write(json.dumps(session, separators=(",", ":"), ensure_ascii=False))
            f.write("\n")
        f.flush()
        os.fsync(f.fileno())
    try:
        os.chmod(archive_path, 0o600)
    except OSError:
        pass


def parse_ts(session: dict) -> str:
    return session.get("last_updated") or ""


def fixed_width_ts(raw: str) -> str:
    """Normalize a timestamp to the store's fixed-width UTC micros format
    (lexicographic order == time order). Falls back to the raw string."""
    try:
        dt = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    except (ValueError, AttributeError):
        return raw


def message_text(message: dict) -> str:
    content = message.get("content")
    if isinstance(content, dict):
        text = content.get("Text")
        if isinstance(text, dict):
            return text.get("content") or ""
        if isinstance(text, str):  # pre-migration tuple format
            return text
    return ""


def build_row(session: dict, seq: int) -> tuple:
    """Mirror src/session_store.rs build_row for a raw session dict."""
    messages = session.get("messages", [])
    usages = [m.get("usage") or {} for m in messages]
    total_cost = (session.get("accumulated_cost") or 0.0) + sum(
        u.get("cost") or 0.0 for u in usages
    )
    total_tokens = (session.get("accumulated_tokens") or 0) + sum(
        u.get("total_tokens") or 0 for u in usages
    )
    name = session.get("name", "")
    summary = (
        (session.get("active_context") or {}).get("conversation_summary") or {}
    ).get("summary") or ""
    search_parts = [name.lower(), summary.lower()]
    search_parts.extend(t.lower() for t in (message_text(m) for m in messages) if t)
    return (
        session["id"],
        name,
        fixed_width_ts(parse_ts(session)),
        len(messages),
        total_cost,
        total_tokens,
        1 if session.get("scheduled_timers") else 0,
        summary,
        "\n".join(search_parts),
        seq,
        json.dumps(session, separators=(",", ":"), ensure_ascii=False),
    )


def cmd_list(args) -> int:
    archive = read_archive(args.config_dir / "sessions-archive.jsonl")
    if not archive:
        print("Archive is empty or missing.")
        return 0
    rows = sorted(archive.values(), key=parse_ts, reverse=True)
    print(f"{len(rows)} archived sessions:\n")
    for s in rows:
        ts = parse_ts(s)[:16].replace("T", " ")
        msgs = len(s.get("messages", []))
        print(f"  {s['id']}  {ts}  {msgs:4d} msgs  {s.get('name', '')}")
    return 0


def cmd_merge(args) -> int:
    archive_path = args.config_dir / "sessions-archive.jsonl"
    archive = read_archive(archive_path)
    live_ids = live_session_ids(args.config_dir)

    to_append = []
    for input_file in args.files:
        path = Path(input_file)
        try:
            with open(path, encoding="utf-8") as f:
                data = json.load(f)
        except (OSError, json.JSONDecodeError) as e:
            print(f"skipping {path}: {e}", file=sys.stderr)
            continue

        sessions = data.get("sessions")
        if not isinstance(sessions, dict):
            print(f"skipping {path}: no 'sessions' map found", file=sys.stderr)
            continue

        added = skipped_live = skipped_dup = 0
        for sid, session in sessions.items():
            if sid in live_ids:
                skipped_live += 1
                continue
            existing = archive.get(sid)
            if existing is not None and parse_ts(existing) >= parse_ts(session):
                skipped_dup += 1
                continue
            archive[sid] = session
            to_append.append(session)
            added += 1
        print(
            f"{path.name}: {added} to merge, {skipped_dup} already archived "
            f"(same or newer), {skipped_live} live-skipped"
        )

    if not to_append:
        print("Nothing new to merge.")
        return 0
    append_to_archive(archive_path, to_append)
    print(f"\nAppended {len(to_append)} sessions to {archive_path}")
    return 0


def cmd_restore(args) -> int:
    archive = read_archive(args.config_dir / "sessions-archive.jsonl")
    session = archive.get(args.session_id)
    if session is None:
        print(f"Session {args.session_id} not found in archive.", file=sys.stderr)
        return 1

    with open_db(args.config_dir) as conn:
        exists = conn.execute(
            "SELECT 1 FROM sessions WHERE id = ?", (args.session_id,)
        ).fetchone()
        if exists:
            print("Session already present in live sessions.db — nothing to do.")
            return 0

        (max_seq,) = conn.execute("SELECT COALESCE(MAX(seq), 0) FROM sessions").fetchone()
        conn.execute(
            "INSERT INTO sessions (id, name, last_updated, message_count, total_cost,"
            " total_tokens, has_timers, summary, search_text, seq, data)"
            " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            build_row(session, max_seq + 1),
        )
        conn.commit()

    print(f"Restored '{session.get('name', args.session_id)}' into sessions.db.")
    print("Restart Hobbes (make sure it was not running during the restore).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config-dir", type=Path, default=default_config_dir())
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("list").set_defaults(func=cmd_list)

    p_merge = sub.add_parser("merge")
    p_merge.add_argument("files", nargs="+")
    p_merge.set_defaults(func=cmd_merge)

    p_restore = sub.add_parser("restore")
    p_restore.add_argument("session_id")
    p_restore.set_defaults(func=cmd_restore)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
