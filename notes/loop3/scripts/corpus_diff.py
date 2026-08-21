#!/usr/bin/env python3
"""Case-by-case diff of two parser-corpus runs.

  corpus_diff.py <before.json> <after.json>

A rate that goes up says nothing about what it traded. This lists every case
that changed verdict **and** every case whose parse changed without changing
verdict — a fail that becomes a *different* fail is the class the corpus scores
identically and the project cares about, because a parse can go from no claim to
a wrong claim while both read as failure.
"""
import json
import sys


def index(path):
    # By position, not by input: the corpus holds the same name more than once
    # under different expectations, so the name is not a key. Both runs read the
    # same `cases.json` in the same order, and the inputs are compared
    # positionally below to prove it.
    return json.load(open(path))


def main():
    b, a = index(sys.argv[1]), index(sys.argv[2])
    if len(b) != len(a):
        sys.exit("case counts differ: %d before, %d after" % (len(b), len(a)))
    for i, (x, y) in enumerate(zip(b, a)):
        if x["input"] != y["input"]:
            sys.exit("case %d is a different name in the two runs" % i)
    gain = [i for i in range(len(b))
            if b[i]["verdict"] == "fail" and a[i]["verdict"] == "pass"]
    loss = [i for i in range(len(b))
            if b[i]["verdict"] == "pass" and a[i]["verdict"] == "fail"]
    same_verdict_new_parse = [
        i for i in range(len(b))
        if b[i]["verdict"] == a[i]["verdict"]
        and b[i].get("parsed") != a[i].get("parsed")
    ]
    print("cases %d   fail->pass %d   pass->fail %d   parse moved, verdict did not %d"
          % (len(b), len(gain), len(loss), len(same_verdict_new_parse)))
    for label, idxs in (("+", gain), ("-", loss)):
        for i in idxs:
            print("  %s %s" % (label, b[i]["input"][:90]))
            print("      before %s" % b[i].get("parsed"))
            print("      after  %s" % a[i].get("parsed"))
    for i in same_verdict_new_parse:
        print("  ~ %s   [%s]" % (b[i]["input"][:80], b[i]["verdict"]))
        print("      before %s" % b[i].get("parsed"))
        print("      after  %s" % a[i].get("parsed"))
    return 1 if loss else 0


if __name__ == "__main__":
    sys.exit(main())
