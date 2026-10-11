#!/usr/bin/env python3
"""Read-only audit of prophecy capture coverage (see docs/prophecy-design.md).

Reports, for done prompt cells since a cutoff:
  * how many cells filed at least one prophecy, per agent and cell index;
  * where in each terminal transcript the `RALPHUS_PROPHECY:` markers sit
    (mid-run vs. within the closing lines), and
  * markers present in a transcript that never reached the database.

Usage: python scripts/audit_prophecy_coverage.py [--db PATH] [--logs DIR]
       [--since-ms EPOCH_MS] [--tail-lines N]

Nothing is written; the DB is opened read-only.
"""

import argparse
import re
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path

KINDS = "discovery|decision|hazard|deferred|unconfirmed|refused"
MARKER = re.compile(rf"^\W*RALPHUS_PROPHECY:\s*({KINDS})\s*:\s*(.+)")


def norm(text):
    return " ".join(text.split())


def main():
    home = Path.home() / ".ralphus"
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--db", default=str(home / "tasks.db"))
    ap.add_argument("--logs", default=str(home / "terminal_logs"))
    ap.add_argument("--since-ms", type=int, default=0)
    ap.add_argument("--tail-lines", type=int, default=20)
    args = ap.parse_args()

    db = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    cells = db.execute(
        "SELECT squad_id, task_idx, idx, agent FROM cells "
        "WHERE state = 'done' AND prompt IS NOT NULL AND prompt != '' "
        "AND COALESCE(started_at_ms, 0) >= ?",
        (args.since_ms,),
    ).fetchall()
    filed = defaultdict(set)
    for uri, body in db.execute(
        "SELECT entity_uri, body FROM prophecies WHERE entity_uri LIKE 'cell:%'"
    ):
        filed[uri].add(norm(body))

    by_agent = defaultdict(lambda: [0, 0])
    by_idx = defaultdict(lambda: [0, 0])
    for squad, task, idx, agent in cells:
        has = bool(filed.get(f"cell:{squad}:{task}:{idx}"))
        for bucket, key in ((by_agent, agent or "?"), (by_idx, idx)):
            bucket[key][0] += 1
            bucket[key][1] += has

    print(f"done prompt cells: {len(cells)}")
    for title, bucket in (("agent", by_agent), ("cell idx", by_idx)):
        print(f"\nwith >=1 prophecy, by {title}:")
        for key, (total, with_p) in sorted(bucket.items(), key=lambda kv: str(kv[0])):
            print(f"  {key}: {with_p}/{total}")

    mid = tail = missing = 0
    squads = {squad for squad, _, idx, _ in cells if idx == 0}
    for squad in sorted(squads):
        stored = [b for uri, bodies in filed.items() if f":{squad}:" in uri for b in bodies]
        for log in Path(args.logs).glob(f"ralphus_{squad}_*_work/0000.log"):
            lines = log.read_text(errors="replace").splitlines()
            for n, line in enumerate(lines):
                m = MARKER.search(line)
                if not m:
                    continue
                if len(lines) - n <= args.tail_lines:
                    tail += 1
                else:
                    mid += 1
                head = norm(m.group(2))[:40]
                if not any(head in b for b in stored):
                    missing += 1
    print(f"\ntranscript markers: {mid} mid-run, {tail} in last {args.tail_lines} lines")
    print(f"markers not found in the database (matched by body): {missing}")
    print(
        "note: only work-cell (idx 0) squads are scanned, `_work` dirs are matched"
        " by squad id, and bodies by their first 40 characters (the terminal wraps"
        " long lines); treat as indicative."
    )


if __name__ == "__main__":
    sys.exit(main())
