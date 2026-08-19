#!/usr/bin/env python3
"""Count the oracle rows a mechanism accounts for. Counted, never estimated.

The loop's rule is that an estimate sitting in a table of measurements reads as
a measurement. So this reads `scored-a.json` and counts rows by the structural
property the mechanism is about — not by shape name, because a shape name is a
label and the property is what the code sees.

The property for each mechanism is derived with the same rule the product uses,
transcribed here for one reason only: `show_folder_relpath` needs the library
root, and the scored rows carry the relpath but not the root. The relpaths the
oracle generates are always root-relative, so "has no show folder" is exactly
"the relpath has no directory component" for a file, and "one directory
component" for the flat shapes. Both are checked against the shipped function
in probe_mechanisms.py rather than trusted from here.

Usage: population.py [scored.json]
"""
import collections, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
SCORED = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    HERE, "..", "..", "..", "out", "scored-a.json")

# The scorer's own labels, read off a run rather than guessed. Guessing them
# once made this script report wrong=0 for a shape the scorer said had 2,553 —
# the verdict is `wrong.entity`, and a label that does not exist counts zero
# exactly like a mechanism that is not there.
VERDICTS = ["correct", "wrong.entity", "wrong.ep", "partial", "absent", "stalled"]
WRONG = ("wrong.entity", "wrong.ep")


def check_labels(rows):
    """Abort if the run carries a verdict this script does not know."""
    seen = {r["verdict"] for r in rows}
    unknown = seen - set(VERDICTS)
    if unknown:
        raise SystemExit("scorer emitted verdicts this script does not count: %s"
                         % sorted(unknown))


rows = json.load(open(SCORED))["rows"]
check_labels(rows)


def tally(sel):
    c = collections.Counter(r["verdict"] for r in sel)
    meas = sum(c[v] for v in VERDICTS if v != "stalled")
    return c, meas


def report(label, sel):
    c, meas = tally(sel)
    pct = (100.0 * c["correct"] / meas) if meas else float("nan")
    print("  %-46s rows %6d  meas %6d  correct %6d (%5.1f%%)  wrong %5d  absent %5d  stalled %6d"
          % (label, len(sel), meas, c["correct"], pct, sum(c[v] for v in WRONG),
             c["absent"], c["stalled"]))


def depth(p):
    """Directory components above the file in the generated relpath."""
    return p.count("/")


print("M6 — episode files with no show folder (shared library root)")
print("     the grouping key is (library_id, show_folder_relpath(...)), and a")
print("     root-level file has no show folder, so every show in the root")
print("     shares one key and one winner.")
m6 = [r for r in rows if r["kind"] == "tv" and depth(r["path"]) == 0]
report("all root-level episode rows", m6)
for shape in sorted({r["shape"] for r in m6}):
    report("  shape %s" % shape, [r for r in m6 if r["shape"] == shape])

print()
print("M4 — episode files whose show folder is one level up (flat layout)")
print("     year_from_show_folder walks exactly two parents from the file, so")
print("     a flat layout lands on the library root and never sees the year.")
m4 = [r for r in rows if r["kind"] == "tv" and depth(r["path"]) == 1]
report("all one-level episode rows", m4)
for shape in sorted({r["shape"] for r in m4}):
    sel = [r for r in m4 if r["shape"] == shape]
    # Only a folder that actually carries `(YYYY)` can lose a year to this.
    withyear = [r for r in sel if "(" in r["path"].split("/")[0]]
    report("  shape %s (folder has a year: %d)" % (shape, len(withyear)), sel)

print()
print("controlled pair for M4 — same filename form, only the layout differs")
for shape in ("tv.sonarr.plain", "tv.flat.titled"):
    report("  %s" % shape, [r for r in rows if r["shape"] == shape])

print()
print("M5 — movie rows with no year in the filename or the folder")
m5 = [r for r in rows if r["kind"] == "movie" and r["shape"] == "movie.noyear"]
report("movie.noyear", m5)

print()
print("M3 — movie rows whose title ends in a 4-digit number")
m3 = [r for r in rows if r["kind"] == "movie"
      and r["name"].split()[-1].isdigit() and len(r["name"].split()[-1]) == 4]
report("title ends in a 4-digit number", m3)
for shape in sorted({r["shape"] for r in m3}):
    report("  shape %s" % shape, [r for r in m3 if r["shape"] == shape])
names = sorted({r["name"] for r in m3})
print("     entities: %s" % ", ".join(names))

print()
print("M1/M2 — the titleless shapes, which the harness cannot derive a title for")
for shape in ("tv.numbered", "tv.handmade"):
    report("  %s" % shape, [r for r in rows if r["shape"] == shape])
