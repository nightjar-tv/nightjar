#!/usr/bin/env python3
"""Exactly how many groups each oracle shape forms, and out of how many files.

  ORACLE_QUERY=.../oracle_query group_census.py <shape> [shape ...]

The drain's `groups=` counter is cumulative across passes — `tv.single` reports
1,386 groups for 693 files and 698 entities — so it is a ratio at best and a
census never. This counts the groups themselves.

The key is `episode_group_key`'s: the library, plus the show folder, or the
cleaned title when there is no show folder. Both derivations come from the
shipped chain through `oracle_query` (`show_folder_relpath`, `clean_show_title`,
`norm_key`) rather than from a rule rewritten here — a reimplemented `norm_key`
has misreported this project before.
"""
import collections
import json
import os
import subprocess
import sys

SPIKE = os.path.expanduser("~/nightjar-spikes/matcher-oracle-2026-08-19")
BIN = os.environ.get("ORACLE_QUERY")
if not BIN or not os.path.exists(BIN):
    sys.exit("set ORACLE_QUERY to the built oracle_query binary")

shapes = set(sys.argv[1:])
man = json.load(open(os.path.join(SPIKE, "out", "manifest.json")))
rows = [m for m in man if not shapes or m["shape"] in shapes]
if not rows:
    sys.exit("no manifest rows for %s" % (sorted(shapes) or "any shape"))

stdin = "".join("%s\t%s\n" % (m["path"], "episode" if m["kind"] == "tv" else "movie")
                for m in rows)
out = subprocess.run([BIN], input=stdin, capture_output=True, text=True, check=True)
lines = out.stdout.splitlines()
if len(lines) != len(rows):
    sys.exit("oracle_query returned %d lines for %d paths" % (len(lines), len(rows)))

per = collections.defaultdict(lambda: [0, set(), set()])
for m, line in zip(rows, lines):
    f = line.split("\t")
    kind, norm, show_folder = f[1], f[6], f[9]
    # A file with no show folder groups by its own cleaned title; one with a
    # folder groups by the folder. `library` stands in for `library_id`.
    key = (m["library"], show_folder if show_folder else "\0" + norm)
    slot = per[m["shape"]]
    slot[0] += 1
    slot[1].add(key)
    slot[2].add(m["entity"])

print("%-20s %8s %8s %8s %10s" % ("shape", "files", "groups", "entities", "files/grp"))
for shape in sorted(per):
    files, groups, ents = per[shape]
    print("%-20s %8d %8d %8d %10.2f"
          % (shape, files, len(groups), len(ents), files / len(groups)))
