#!/usr/bin/env python3
"""Count bindings to the wrong *kind* — an episode file bound to a film.

The scorer has no verdict for this. For a tv entity it looks for an episode link,
then a show link, and files "neither" as `partial` (score_binding.py:107).
`partial` reads as incomplete-but-not-wrong. A movie link is not incomplete: the
file has left the TV library entirely, which is the most severe outcome in the
suite — worse than a wrong show, because a wrong show at least keeps the item in
the right place.

So this reads the run databases directly and counts what the scorer cannot say.
It does not modify the scorer: the instrument understating severity is a finding
to report, not something to edit around.

Usage: crosskind.py <run-root> [shape-prefix]
"""
import os, sqlite3, sys, collections

RUNROOT = sys.argv[1]
PREFIX = sys.argv[2] if len(sys.argv) > 2 else ""

tot = collections.Counter()
examples = []
for run in sorted(os.listdir(RUNROOT)):
    if PREFIX and not run.startswith(PREFIX):
        continue
    db = os.path.join(RUNROOT, run, "data", "nightjar.db")
    if not os.path.exists(db):
        continue
    shape = run.rpartition("-")[0]
    con = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
    try:
        rows = con.execute(
            """SELECT mi.path, l.item_key, mi.kind
                 FROM media_items mi
                 JOIN media_item_links l ON l.media_item_id = mi.id
                WHERE l.item_key LIKE 'tmdb:movie:%'
                  AND mi.library_id IN (SELECT id FROM libraries WHERE kind='shows')"""
        ).fetchall()
    except sqlite3.Error as e:
        print("  !! %s: %s" % (run, e))
        con.close()
        continue
    con.close()
    tot[shape] += len(rows)
    for p, k, _ in rows[:3]:
        if len(examples) < 12:
            examples.append((shape, p, k))

print("episode files in a shows library bound to a FILM, by shape:")
for shape, n in tot.most_common():
    print("  %-20s %6d" % (shape, n))
print("  %-20s %6d" % ("TOTAL", sum(tot.values())))
print()
print("examples:")
for shape, p, k in examples:
    print("  %-18s %-52s -> %s" % (shape, p, k))
print()
print("The scorer files every one of these as `partial`. They are the wrong kind,")
print("not an incomplete match: the file has left the TV library altogether.")
