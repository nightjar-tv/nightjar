#!/usr/bin/env python3
"""Reduce the pinned Sonarr/Radarr parser fixtures to Nightjar's parse schema.

The upstream fixtures assert their own parser's output. Nightjar produces only
title / kind / year / season / episode(s), so every other asserted field is
dropped and recorded rather than failed. A case that asserts only dropped
fields has no parse question left in it and is excluded, not failed.

This tool is development evidence, not product code. It reads the pinned files
under `development/upstream/`, writes one canonical UTF-8 JSON corpus, and makes
no network call. `check.py` validates the result against `SOURCES.json`.
"""
import argparse
import collections
import glob
import json
import os
import re
import sys

# server/tools/naming_corpus/extract.py -> server/
SERVER = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
NAMING = os.path.join(SERVER, "crates", "core", "tests", "fixtures", "naming")
DEFAULT_UPSTREAM = os.path.join(NAMING, "development", "upstream")
DEFAULT_OUT = os.path.join(NAMING, "development", "corpus.json")

SCHEMA_VERSION = 1
SET = "development"

# param name -> our field, or None to drop it (recorded by name)
KEEP = {
    "title": "title", "seriestitle": "title", "movietitle": "title",
    "seasonnumber": "season", "season": "season",
    "episodenumber": "episode", "episode": "episode", "episodes": "episodes", "episodenumbers": "episodes",
    "year": "year",
}
# params we know are theirs and not ours
DROP_KNOWN = {
    "absoluteepisodenumber", "absoluteepisodenumbers", "edition", "quality",
    "releasegroup", "language", "languages", "month", "day", "part",
    "airdate", "isdaily", "special", "full", "resolution", "source",
    "isproper", "version", "hash", "subgroup", "expected", "seriestitleinfo",
    "isseasonextra", "ispartialseason", "isspecial", "seasonpart",
}

# Methods asserting only their own schema (ids, quality, language, edition,
# their SeriesTitleInfo decomposition). Not failures for us -- not applicable.
NOT_APPLICABLE = {
    "should_normalize_imdbid", "should_parse_tmdb_id", "should_parse_imdb_in_title",
    "should_parse_quality_from_extension", "should_parse_language_after_parsing_title",
    "should_parse_releasetitle", "should_parse_edition", "should_not_parse_edition",
    "should_parse_hardcoded_subs", "should_not_parse_wrong_language_in_title",
    "should_parse_series_title_info", "should_parse_multiple_series_titles",
    "should_remove_request_info_from_title", "should_clean_up_invalid_path_characters",
    "should_parse_movie_folder_name",
}
# `should_parse_series_name` asserts Sonarr's normalised key -- lowercased,
# punctuation and spaces removed -- not a title. Comparing it against our title
# fails on spacing alone, so it is emitted as `title_key` and compared squashed.
SERIES_KEY_METHOD = "should_parse_series_name"

# Methods asserting the parser rejects the name. Nightjar's parse never rejects,
# so these become a no-episode-claimed check instead.
MUST_NOT_PARSE = {
    "should_not_parse_crap", "should_not_parse_invalid_release_name",
    "should_not_parse_file_name_without_proper_spacing",
    "should_not_parse_special_with_part_number", "should_parse_unknown_formats_without_error",
    "should_not_accept_ancient_daily_series", "should_not_accept_future_dates",
    "should_not_parse_ambiguous_daily_episode",
}

REASON_MAPPED = "maps at least one upstream assertion to a Nightjar field"
REASON_UPSTREAM_ONLY = "upstream asserts only fields Nightjar does not produce"
REASON_REJECT = "upstream asserts the name has no episode; Nightjar records whether it claims one"

case_re = re.compile(r'^\s*\[TestCase\((.*)\)\]\s*$')
meth_re = re.compile(r'public\s+void\s+(\w+)\s*\(([^)]*)\)')


def _squash(x):
    return re.sub(r'[^a-z0-9]', '', (x or "").lower())


def sonarr_key_only(inp, key):
    """True when Sonarr's key does more than drop case and punctuation.

    It appends the parenthesised year, or it drops English stop-words. A case
    relying on either asserts their normalisation, not our title extraction, so
    there is no parse question left in it. The returned string is the reason.
    """
    m = re.search(r'\((19|20)\d{2}\)', inp)
    if m and key.endswith(m.group(0)[1:-1]):
        return "sonarr key appends the year"
    if not _squash(inp).startswith(key) and \
       _squash(re.sub(r'\b(of|the|a|an)\b', '', inp, flags=re.I)).startswith(key):
        return "sonarr key drops stop-words"
    return None


def split_args(s):
    """Split a C# argument list on top-level commas, honouring strings/braces."""
    out, buf, depth, instr, esc, verb = [], "", 0, False, False, False
    i = 0
    while i < len(s):
        c = s[i]
        if instr:
            buf += c
            if esc: esc = False
            elif c == "\\" and not verb: esc = True
            elif c == '"':
                if verb and i + 1 < len(s) and s[i+1] == '"':
                    buf += s[i+1]; i += 2; continue
                instr = False; verb = False
            i += 1; continue
        if c == '@' and i + 1 < len(s) and s[i+1] == '"':
            buf += c; verb = True; instr = True; i += 1; buf += s[i]; i += 1; continue
        if c == '"':
            instr = True; buf += c; i += 1; continue
        if c in "{[(": depth += 1
        elif c in "}])": depth -= 1
        if c == "," and depth == 0:
            out.append(buf.strip()); buf = ""; i += 1; continue
        buf += c; i += 1
    if buf.strip(): out.append(buf.strip())
    return out


def lit(tok):
    tok = tok.strip()
    if tok.startswith('@"') and tok.endswith('"'):
        return tok[2:-1].replace('""', '"')
    if tok.startswith('"') and tok.endswith('"'):
        # Decode C# escapes without a latin-1 round trip: `unicode_escape` on a
        # str containing non-ASCII mangles every multi-byte character.
        body = tok[1:-1]
        out, i = "", 0
        while i < len(body):
            if body[i] == "\\" and i + 1 < len(body):
                n = body[i+1]
                if n == "u" and i + 5 < len(body):
                    try:
                        out += chr(int(body[i+2:i+6], 16)); i += 6; continue
                    except ValueError:
                        pass
                out += {"n": "\n", "t": "\t", "r": "\r", "\\": "\\",
                        '"': '"', "'": "'", "0": "\0"}.get(n, n)
                i += 2; continue
            out += body[i]; i += 1
        return out
    if tok.lower() in ("null", "true", "false"):
        return {"null": None, "true": True, "false": False}[tok.lower()]
    m = re.match(r'^new(?:\s+\w+)?\s*\[\s*\]\s*\{(.*)\}$', tok, re.S)
    if m:
        return [lit(x) for x in split_args(m.group(1))]
    try:
        return int(tok.lstrip("0") or "0") if re.fullmatch(r'-?0*\d+', tok) else tok
    except ValueError:
        return tok


# --- form classification, on the input string ---------------------------------
METHOD_FORM = {
    "should_parse_edition": "edition", "should_not_parse_edition": "edition",
    "should_parse_german_movie": "non-English / dual title",
    "should_parse_chinese_anime_releases": "non-English / dual title",
    "should_parse_chinese_anime_season_episode_releases": "non-English / dual title",
    "should_parse_unbracketed_chinese_anime_releases": "non-English / dual title",
    "should_parse_chinese_multiepisode_releases": "non-English / dual title",
    "should_parse_gm_team_releases_and_files": "non-English / dual title",
    "should_parse_unicode_digits": "non-English / dual title",
    "should_parse_false_positive_chinese_anime_releases": "non-English / dual title",
    "should_parse_korean_series_episode": "non-English / dual title",
    "should_parse_daily_episode": "date-based",
    "should_parse_daily_episode_with_multiple_parts": "date-based",
    "should_parse_daily_episode_using_short_month_format": "date-based",
    "should_parse_full_season_release": "season pack",
    "should_parse_multi_season_release": "season pack",
    "should_parse_season_subpack": "season pack",
    "should_parse_partial_season_release": "season pack",
    "should_parse_season_extras": "season extras",
    "should_parse_absolute_specials": "specials / S00",
    "should_parse_absolute_specials_without_absolute_number": "specials / S00",
    "should_parse_decimal_number_as_special": "specials / S00",
    "should_parse_from_path": "season-folder context (path)",
    "should_parse_multi_episode_from_path": "season-folder context (path)",
    "should_parse_movie_year": "scene-style movie",
    "should_parse_movie_title": "scene-style movie",
    "should_parse_anime_movie_title": "scene-style movie",
    "should_parse_anime_movie_title_without_year": "scene-style movie",
    "should_parse_mini_series_episode": "mini-series (no season number)",
    "should_parse_japanese_variety_show_format": "non-English / dual title",
}


def form(c):
    s = c["input"]
    low = s.lower()
    meth = c["method"]
    if meth in MUST_NOT_PARSE:
        return "must-not-parse (junk)"
    if meth in NOT_APPLICABLE:
        return "not applicable (their schema)"
    if meth in METHOD_FORM:
        return METHOD_FORM[meth]
    if re.search(r'^\s*(https?://|www\.)', low) or re.match(r'^\s*www\.\S+\s*-\s*', low):
        return "site prefix / junk"
    if "\\" in s or "/" in s:
        return "season-folder context (path)"
    if re.search(r's\s?\d{1,2}\s?e\s?\d{1,3}([\s._-]*[e-]\s?\d{1,3})+', low):
        return "multi-episode"
    if re.search(r'\d{1,2}x\d{1,2}([\s._-]*\d{1,2})+', low):
        return "multi-episode"
    if re.search(r's0?0e\d', low) or "special" in meth.lower():
        return "specials / S00"
    if re.search(r's\d{1,2}e\d{1,3}', low):
        return "SxxEyy"
    if re.search(r'\b\d{1,2}x\d{1,2}\b', low):
        return "NxNN"
    if re.search(r'(19|20)\d{2}[\s._-](0?\d|1[012])[\s._-](0?\d|[12]\d|3[01])\b', low):
        return "date-based"
    if "absolute" in meth.lower() or re.search(r'^\[[^\]]+\]', s):
        return "absolute numbering (anime)"
    if "edition" in meth.lower():
        return "edition"
    if re.search(r'^\s*(\d{3,4}|24|1917|9-1-1)[\s._(\[]', s) or re.search(r'\b(19|20)\d{2}\b.*\b(19|20)\d{2}\b', s):
        return "adversarial numeric/year title"
    if re.search(r'[^\x00-\x7f]', s) or " AKA " in s or " / " in s:
        return "non-English / dual title"
    if "season" in meth.lower():
        return "season pack"
    if re.search(r'\b(19|20)\d{2}\b', s):
        return "scene-style movie"
    return "other"


def extract(upstream_dir):
    cases, pending = [], []
    seq = collections.Counter()
    for path in sorted(glob.glob(os.path.join(upstream_dir, "*.cs"))):
        src = os.path.basename(path)
        for line in open(path, encoding="utf-8", errors="replace"):
            m = case_re.match(line)
            if m:
                pending.append(m.group(1)); continue
            mm = meth_re.search(line)
            if mm and pending:
                name = mm.group(1)
                params = [re.split(r'\s*=', p.strip())[0].strip().split()[-1].lower()
                          for p in split_args(mm.group(2)) if p.strip()]
                for raw in pending:
                    args = [lit(a) for a in split_args(raw)]
                    if not args or not isinstance(args[0], str):
                        continue
                    expect, dropped = {}, []
                    reason = REASON_MAPPED
                    if name in MUST_NOT_PARSE:
                        expect = {"reject": True}
                        dropped = [p for p in params[1:]]
                        reason = REASON_REJECT
                    elif name in NOT_APPLICABLE:
                        dropped = [p for p in params[1:]]
                        reason = REASON_UPSTREAM_ONLY
                    else:
                        for pname, val in zip(params[1:], args[1:]):
                            f = KEEP.get(pname)
                            if f and val is not None:
                                expect[f] = val
                            else:
                                dropped.append(pname)
                        # Absent-as-zero, repaired in two places because the
                        # value means "absent" in two different fixtures.
                        #
                        # GUARD 1 (season/episode): scoped to the Absolute
                        # fixture only, and to the pair being zero together.
                        # Season 0 is Specials everywhere else.
                        if "Absolute" in src and expect.get("season") == 0 and expect.get("episode") == 0:
                            expect.pop("season"); expect.pop("episode")
                            dropped += ["seasonnumber(absent)", "episodenumber(absent)"]
                        # GUARD 2 (year): year 0 is never a real value in any
                        # fixture. Radarr passes 0 for "no year asserted".
                        if expect.get("year") == 0:
                            expect.pop("year")
                            dropped += ["year(absent)"]
                        # KNOWN THIRD VARIANT, deliberately not repaired: one
                        # case asserts season 2 with episode 0 -- absent-as-zero
                        # again, but the pair is not zero together so GUARD 1
                        # misses it. No corpus case asserts a meaningful
                        # episode 0, so the change would be safe but
                        # unmeasurable. Recorded rather than made.
                    if name == SERIES_KEY_METHOD and "title" in expect:
                        key = expect.pop("title")
                        key_reason = sonarr_key_only(args[0], key)
                        if key_reason:
                            dropped.append(key_reason)  # -> not applicable
                            reason = key_reason
                        else:
                            expect["title_key"] = key
                    if not expect:
                        reason = reason if reason != REASON_MAPPED else REASON_UPSTREAM_ONLY
                    cid = f"{src[:-3]}.{name}.{seq[(src, name)] + 1:03d}"
                    seq[(src, name)] += 1
                    case = {
                        "id": cid,
                        "source": src,
                        "test": name,
                        "input": args[0],
                        "expect": expect,
                        "dropped": dropped,
                        "applicable": bool(expect),
                        "reason": reason,
                        "category": form({"input": args[0], "method": name}),
                    }
                    cases.append(case)
                pending = []
            elif mm:
                pending = []
    return cases


def build(upstream_dir):
    cases = extract(upstream_dir)
    categories = collections.Counter(c["category"] for c in cases)
    sources = sorted({c["source"] for c in cases})
    return {
        "schema_version": SCHEMA_VERSION,
        "set": SET,
        "counts": {
            "total": len(cases),
            "applicable": sum(1 for c in cases if c["applicable"]),
            "excluded": sum(1 for c in cases if not c["applicable"]),
        },
        "categories": {k: categories[k] for k in sorted(categories)},
        "sources": sources,
        "cases": cases,
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--upstream", default=DEFAULT_UPSTREAM)
    ap.add_argument("--out", default=DEFAULT_OUT)
    args = ap.parse_args()

    corpus = build(args.upstream)
    text = json.dumps(corpus, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(text)

    counts = corpus["counts"]
    print(f"extracted {counts['total']} cases from {len(corpus['sources'])} sources")
    print(f"  applicable: {counts['applicable']}")
    print(f"  excluded:   {counts['excluded']}")
    print(f"  wrote {args.out} ({len(text.encode('utf-8'))} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
