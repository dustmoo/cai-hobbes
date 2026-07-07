#!/usr/bin/env python3
"""Manage the Hobbes session archive (sessions-archive.jsonl).

The archive is append-only JSONL: one Session object per line. The app's
session GC appends stale sessions here instead of deleting them. Duplicate
session ids are resolved last-line-wins.

Subcommands:
  list                       List archived sessions (newest first).
  merge <state.json> [...]   Merge sessions out of full session-state JSON
                             files (sessions.json snapshots/backups) into the
                             archive. Inputs are processed in the order given,
                             so pass oldest first — a newer copy of the same
                             session id wins. Sessions already live in
                             sessions.json are skipped.
  restore <session-id>       Copy a session from the archive back into the
                             live sessions.json. Quit Hobbes first! A backup
                             of sessions.json is written alongside it.

Run with the app's config dir auto-detected, or override with --config-dir.
"""

import argparse
import json
import os
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path


def default_config_dir() -> Path:
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "com.hobbes.app"
    if sys.platform == "win32":
        return Path(os.environ["APPDATA"]) / "com.hobbes.app"
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "com.hobbes.app"


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

    live_ids = set()
    live_path = args.config_dir / "sessions.json"
    if live_path.exists():
        with open(live_path, encoding="utf-8") as f:
            live_ids = set(json.load(f).get("sessions", {}).keys())

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

    live_path = args.config_dir / "sessions.json"
    with open(live_path, encoding="utf-8") as f:
        state = json.load(f)
    if args.session_id in state["sessions"]:
        print("Session already present in live sessions.json — nothing to do.")
        return 0

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    backup = live_path.with_name(f"sessions.json.pre-restore-{stamp}")
    shutil.copy2(live_path, backup)
    print(f"Backed up live state to {backup.name}")

    state["sessions"][args.session_id] = session
    tmp = live_path.with_name("sessions.json.restore-tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f, indent=2, ensure_ascii=False)
        f.flush()
        os.fsync(f.fileno())
    os.chmod(tmp, 0o600)
    os.replace(tmp, live_path)
    print(f"Restored '{session.get('name', args.session_id)}' into sessions.json.")
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
