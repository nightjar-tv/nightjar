#!/usr/bin/env python3
"""Ask the shipped parser and cleaners whether each named mechanism is still live.

The oracle's brief names six mechanisms, all measured against `2f6b1a9`. `main`
then squash-merged a 2,249-line rewrite of the parser (#149), so every one of
them has to be re-asked before it is worth fixing. A mechanism that is already
gone and gets "fixed" anyway measures flat, and flat is indistinguishable from
a change that never ran.

This shells to `oracle_query`, which calls the shipped `parse_filename`,
`clean_movie_title`, `clean_show_title`, `norm_key`, `year_from_path` and
`year_from_show_folder`. Nothing here reimplements any of them: a rewritten
`norm_key` has misreported this project once already.

Usage: probe_mechanisms.py            (needs $ORACLE_QUERY)
"""
import os, subprocess, sys

OQ = os.environ.get("ORACLE_QUERY")
if not OQ or not os.path.exists(OQ):
    sys.exit("set ORACLE_QUERY to the built oracle_query binary")

FIELDS = ("relpath kind query year season episode norm raw_year folder_year "
          "show_folder folder_norm showfolder_year").split()

# One probe per mechanism. `want` describes what a *healthy* parse looks like,
# and is only ever used to print a verdict — never to assert, because the point
# of the probe is to find out.
CASES = [
    ("M1", "episode filename with no title; folder carries it",
     "Closure (2001)/Season 01/S01E01.mkv"),
    ("M1", "same, scene-marker form",
     "Scrubs (2001)/Season 01/1x04.mkv"),
    ("M2", "episode filename that is number + episode title",
     "Scrubs (2001)/Season 01/01 - My Old Lady.mkv"),
    ("M2", "same, no leading number (the wrong-bind form)",
     "Scrubs (2001)/Season 01/My Old Lady.mkv"),
    ("M3", "title carries a year that is not the release year",
     "Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv"),
    ("M3", "same, scene form",
     "Blade.Runner.2049.2017.1080p.BluRay.x264.mkv"),
    ("M3", "control: title year is the only year",
     "Blade.Runner.2049.1080p.BluRay.x264.mkv"),
    ("M4", "show folder two levels up from the file",
     "Scrubs (2001)/Season 01/Scrubs - S01E01 - My Old Lady.mkv"),
    ("M4", "show folder three levels up (deeper layout)",
     "TV/Scrubs (2001)/Season 01/Scrubs - S01E01 - My Old Lady.mkv"),
    ("M5", "movie with no year anywhere",
     "Scrubs/Scrubs.mkv"),
    ("M6", "two shows sharing one root",
     "Scrubs - S01E01 - My Old Lady.mkv"),
]


def run(paths):
    inp = "".join(p + "\tx\n" for p in paths)
    out = subprocess.run([OQ], input=inp, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit("oracle_query failed: " + out.stderr[:400])
    rows = []
    for line in out.stdout.splitlines():
        if line.strip():
            rows.append(dict(zip(FIELDS, line.split("\t"))))
    return rows


rows = run([c[2] for c in CASES])
by_path = {r["relpath"]: r for r in rows}

for mech, label, path in CASES:
    r = by_path.get(path)
    print("%-3s %s" % (mech, label))
    print("    path         %s" % path)
    if not r:
        print("    !! oracle_query returned no row for this path")
        continue
    print("    kind=%-7s query=%-28r year=%-6s s=%-3s e=%-3s"
          % (r["kind"], r["query"], r["year"], r["season"], r["episode"]))
    print("    norm=%-24r folder_norm=%-24r consulted_folder=%s"
          % (r["norm"], r["folder_norm"], r["folder_norm"] == r["norm"]))
    print("    raw_year=%-6s folder_year=%-6s showfolder_year=%-6s show_folder=%r"
          % (r["raw_year"], r["folder_year"], r["showfolder_year"], r["show_folder"]))
    print()
