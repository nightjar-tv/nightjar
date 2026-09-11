#!/usr/bin/env python3
"""Emit and compare the release input manifest defined by ADR-0056.

The manifest records the inputs that build the release image. It does not claim
that two builds of the same inputs produce identical bytes; the comparison
covers the recorded inputs only. The image ID, the binary SHA-256 and the
emission time are recorded as output evidence and excluded from the comparison,
together with any absolute build path.

Subcommands:
  emit      write the canonical manifest for one built image
  compare   compare the governed projection of two manifests
  selftest  prove the comparator reports a difference on every governed field
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

# Governed fields are the recorded inputs. A trailing `*` matches one or more
# path segments; a `*` anywhere else matches exactly one.
GOVERNED = (
    "source.commit",
    "target.platform",
    "toolchain.*",
    "locks.cargo.sha256",
    "locks.npm.sha256",
    "bases.*.ref",
    "bases.*.digest",
    "snapshots.debian.timestamp",
    "snapshots.debian_security.timestamp",
    "dpkg.*",
    "ffmpeg.buildconf",
)

# Recorded output evidence. These never take part in the comparison.
NON_GOVERNED = ("image_id", "binary_sha256", "emitted_at")

# One artifact lives on one platform. The Linux container image is built on
# linux/amd64; a native macOS server build is a separate artifact on macos/arm64
# (or macos/amd64 on Intel). A Linux container on a Mac cannot use the macOS
# VideoToolbox framework; that path needs a native macOS process.
PLATFORM_BY_TRIPLE = {
    "x86_64-unknown-linux-gnu": "linux/amd64",
    "x86_64-unknown-linux-musl": "linux/amd64",
    "aarch64-unknown-linux-gnu": "linux/arm64",
    "aarch64-unknown-linux-musl": "linux/arm64",
    "aarch64-apple-darwin": "macos/arm64",
    "x86_64-apple-darwin": "macos/amd64",
}


def platform_for_target(triple):
    """Return the artifact platform for a target triple, unchanged if unknown."""
    return PLATFORM_BY_TRIPLE.get(triple, triple)

FROM_RE = re.compile(r"^FROM\s+(\S+)(?:\s+AS\s+(\S+))?\s*$", re.IGNORECASE)
IMAGE_RE = re.compile(
    r"^(?P<name>[^@]+?)(?::(?P<tag>[^:@]+))?@(?P<digest>sha256:[0-9a-f]{64})$"
)
DEBIAN_SNAPSHOT_RE = re.compile(
    r"snapshot\.debian\.org/archive/debian/(\d{8}T\d{6}Z)"
)
SECURITY_SNAPSHOT_RE = re.compile(
    r"snapshot\.debian\.org/archive/debian-security/(\d{8}T\d{6}Z)"
)


def canonical_json(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"


def normalize(value):
    if isinstance(value, dict):
        return {key: normalize(value[key]) for key in sorted(value)}
    if isinstance(value, list):
        items = [normalize(item) for item in value]
        return sorted(items, key=canonical_json)
    if isinstance(value, str):
        return value.strip()
    return value


def flatten(value, prefix=""):
    flat = {}
    if isinstance(value, dict):
        for key, item in value.items():
            flat.update(flatten(item, f"{prefix}.{key}" if prefix else key))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            flat.update(flatten(item, f"{prefix}.{index}"))
    else:
        flat[prefix] = value
    return flat


def matches(path, pattern):
    parts = path.split(".")
    segments = pattern.split(".")
    if segments[-1] == "*":
        head = segments[:-1]
        return len(parts) > len(head) and parts[: len(head)] == head
    if len(parts) != len(segments):
        return False
    return all(segment == "*" or segment == part for part, segment in zip(parts, segments))


def is_governed(path):
    return any(matches(path, pattern) for pattern in GOVERNED)


def load_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def compare_manifests(first, second):
    flat_first = flatten(normalize(first))
    flat_second = flatten(normalize(second))
    paths = sorted(set(flat_first) | set(flat_second))
    differences = []
    equal = 0
    for path in paths:
        if not is_governed(path):
            continue
        value_first = flat_first.get(path, "<absent>")
        value_second = flat_second.get(path, "<absent>")
        if value_first == value_second:
            equal += 1
        else:
            differences.append((path, value_first, value_second))
    return equal, differences


def ignored_count(manifests):
    paths = set()
    for manifest in manifests:
        paths.update(flatten(normalize(manifest)))
    return sum(1 for path in paths if not is_governed(path))


def set_leaf(value, path, replacement):
    parts = path.split(".")
    node = value
    for part in parts[:-1]:
        node = node[part]
    node[parts[-1]] = replacement


def read_text(path):
    return Path(path).read_text(encoding="utf-8")


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_commit(tree):
    result = subprocess.run(
        ["git", "-C", str(tree), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(f"git rev-parse HEAD failed in {tree}: {result.stderr.strip()}")
    return result.stdout.strip()


def parse_bases(dockerfile):
    bases = {}
    for line in dockerfile.splitlines():
        match = FROM_RE.match(line.strip())
        if not match:
            continue
        image, stage = match.group(1), match.group(2)
        if stage is None:
            raise SystemExit(f"FROM line has no AS stage name: {line!r}")
        image_match = IMAGE_RE.match(image)
        if not image_match:
            raise SystemExit(f"FROM image is not tag+digest pinned: {image!r}")
        ref = image_match.group("name")
        tag = image_match.group("tag")
        if tag:
            ref = f"{ref}:{tag}"
        bases[stage] = {"ref": ref, "digest": image_match.group("digest")}
    return bases


def parse_snapshots(dockerfile):
    debian = DEBIAN_SNAPSHOT_RE.search(dockerfile)
    security = SECURITY_SNAPSHOT_RE.search(dockerfile)
    if not debian or not security:
        raise SystemExit("Dockerfile does not name both Debian snapshot timestamps")
    return {
        "debian": {"timestamp": debian.group(1)},
        "debian_security": {"timestamp": security.group(1)},
    }


def ffmpeg_configuration_line(lines):
    parts = []
    collecting = False
    for line in lines:
        stripped = line.strip()
        if not collecting:
            if stripped.startswith("configuration:"):
                collecting = True
                remainder = stripped[len("configuration:") :].strip()
                if remainder:
                    parts.append(" ".join(remainder.split()))
            continue
        if stripped:
            parts.append(" ".join(stripped.split()))
    return " ".join(parts)


def parse_probe(text):
    probe = {
        "image_id": "",
        "binary_sha256": "",
        "toolchain": {},
        "dpkg": {},
        "ffmpeg_buildconf": "",
    }
    ffmpeg_lines = []
    in_ffmpeg = False
    for line in text.splitlines():
        if in_ffmpeg:
            if line.strip() == "end_ffmpeg_buildconf":
                in_ffmpeg = False
            else:
                ffmpeg_lines.append(line)
            continue
        if line.startswith("image_id:"):
            probe["image_id"] = line.split(":", 1)[1].strip()
        elif line.startswith("binary_sha256:"):
            probe["binary_sha256"] = line.split(":", 1)[1].strip()
        elif line.startswith("toolchain."):
            key, value = line.split(":", 1)
            probe["toolchain"][key.split(".", 1)[1].strip()] = value.strip()
        elif line.startswith("dpkg:"):
            fields = line.split(":", 1)[1].split()
            if len(fields) != 5:
                raise SystemExit(f"malformed dpkg probe line: {line!r}")
            package, binary, source, source_version, architecture = fields
            probe["dpkg"][package] = {
                "binary_version": binary,
                "source_package": source,
                "source_version": source_version,
                "architecture": architecture,
            }
        elif line.strip() == "ffmpeg_buildconf:":
            in_ffmpeg = True
    if in_ffmpeg:
        raise SystemExit("probe has an unterminated ffmpeg_buildconf block")
    probe["ffmpeg_buildconf"] = ffmpeg_configuration_line(ffmpeg_lines)
    return probe


def synthetic_manifest():
    return {
        "source": {"commit": "0" * 40},
        "target": {"platform": "linux/amd64"},
        "toolchain": {
            "rust": "rustc 1.97.0",
            "cargo": "cargo 1.97.0",
            "node": "v22.23.1",
            "npm": "10.9.8",
        },
        "locks": {"cargo": {"sha256": "a" * 64}, "npm": {"sha256": "b" * 64}},
        "bases": {
            "web": {"ref": "node:22-bookworm", "digest": "sha256:" + "c" * 64},
            "server": {"ref": "rust:bookworm", "digest": "sha256:" + "d" * 64},
            "runtime": {"ref": "debian:bookworm-slim", "digest": "sha256:" + "e" * 64},
        },
        "snapshots": {
            "debian": {"timestamp": "20260901T000000Z"},
            "debian_security": {"timestamp": "20260901T000000Z"},
        },
        "dpkg": {
            "ffmpeg": {
                "binary_version": "7:5.1.9-0+deb12u1",
                "source_package": "ffmpeg",
                "source_version": "7:5.1.9-0+deb12u1",
                "architecture": "amd64",
            },
            "ca-certificates": {
                "binary_version": "20250419~deb12u1",
                "source_package": "ca-certificates",
                "source_version": "20250419~deb12u1",
                "architecture": "all",
            },
        },
        "ffmpeg": {"buildconf": "--enable-gpl --enable-libx264"},
        "image_id": "sha256:" + "f" * 64,
        "binary_sha256": "1" * 64,
        "emitted_at": "2026-09-11T00:00:00Z",
    }


def cmd_emit(args):
    tree = Path(args.tree).resolve()
    dockerfile = read_text(tree / "Dockerfile")
    probe = parse_probe(read_text(args.probe))
    manifest = {
        "source": {"commit": git_commit(tree)},
        "target": {"platform": platform_for_target(args.target)},
        "toolchain": probe["toolchain"],
        "locks": {
            "cargo": {"sha256": file_sha256(tree / "server" / "Cargo.lock")},
            "npm": {"sha256": file_sha256(tree / "web" / "package-lock.json")},
        },
        "bases": parse_bases(dockerfile),
        "snapshots": parse_snapshots(dockerfile),
        "dpkg": probe["dpkg"],
        "ffmpeg": {"buildconf": probe["ffmpeg_buildconf"]},
        "image_id": probe["image_id"],
        "binary_sha256": probe["binary_sha256"],
        "emitted_at": datetime.datetime.now(datetime.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
    }
    out = Path(args.out)
    out.write_text(canonical_json(normalize(manifest)), encoding="utf-8")
    print(f"wrote {out}")
    return 0


def cmd_compare(args):
    first = load_json(args.first)
    second = load_json(args.second)
    equal, differences = compare_manifests(first, second)
    print(f"IGNORED {ignored_count([first, second])} non-governed fields")
    if differences:
        for path, value_first, value_second in differences:
            print(f"DIFF {path}: {value_first} != {value_second}")
        return 1
    print(f"OK {equal} governed fields equal")
    return 0


def cmd_selftest(_args):
    base = synthetic_manifest()
    equal, differences = compare_manifests(base, base)
    if differences or equal == 0:
        print(f"selftest: positive control failed (equal={equal})", file=sys.stderr)
        return 1

    # Every artifact lives on one platform. The mapping is asserted through the
    # same resolver emit uses, so a removed mapping falls back to the bare
    # triple and fails here.
    expected_platforms = {
        "x86_64-unknown-linux-gnu": "linux/amd64",
        "x86_64-unknown-linux-musl": "linux/amd64",
        "aarch64-unknown-linux-gnu": "linux/arm64",
        "aarch64-unknown-linux-musl": "linux/arm64",
        "aarch64-apple-darwin": "macos/arm64",
        "x86_64-apple-darwin": "macos/amd64",
    }
    platform_failures = []
    for triple, expected in sorted(expected_platforms.items()):
        actual = platform_for_target(triple)
        if actual != expected:
            platform_failures.append(triple)
            print(f"CONTROL platform {triple} -> {actual} != {expected}")
        else:
            print(f"CONTROL platform {triple} -> {actual}")
    unknown = "riscv64-unknown-linux-gnu"
    if platform_for_target(unknown) != unknown:
        platform_failures.append(unknown)
        print(f"CONTROL platform {unknown} -> not passed through unchanged")
    if platform_failures:
        print(
            f"selftest: {len(platform_failures)} platform mappings failed",
            file=sys.stderr,
        )
        return 1

    leaves = {path: value for path, value in flatten(normalize(base)).items() if is_governed(path)}
    for pattern in GOVERNED:
        if not any(matches(path, pattern) for path in leaves):
            print(f"selftest: governed pattern {pattern} has no control", file=sys.stderr)
            return 1

    failures = []
    for path in sorted(leaves):
        mutated = json.loads(json.dumps(base))
        set_leaf(mutated, path, f"mutated-{leaves[path]}")
        _, differences = compare_manifests(base, mutated)
        if not differences or not any(diff[0] == path for diff in differences):
            failures.append(path)
            print(f"CONTROL {path} -> NOT DETECTED")
        else:
            print(f"CONTROL {path} -> nonzero ({path})")

    for path in NON_GOVERNED:
        mutated = json.loads(json.dumps(base))
        set_leaf(mutated, path, f"mutated-{path}")
        if ignored_count([base, mutated]) != len(NON_GOVERNED):
            print(
                f"selftest: {path} is not classified as non-governed",
                file=sys.stderr,
            )
            return 1
        print(f"NON-GOVERNED {path} -> ignored")

    if failures:
        print(f"selftest: {len(failures)} governed fields had no control", file=sys.stderr)
        return 1
    print(f"SELFTEST ok: {len(leaves)} governed fields controlled")
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    emit = subparsers.add_parser("emit", help="write the canonical manifest")
    emit.add_argument("--out", required=True, help="manifest output path")
    emit.add_argument("--tree", default=".", help="repository root")
    emit.add_argument("--target", required=True, help="build target triple")
    emit.add_argument("--probe", required=True, help="probe bundle captured from the image")
    emit.set_defaults(func=cmd_emit)

    compare = subparsers.add_parser("compare", help="compare two manifests")
    compare.add_argument("first")
    compare.add_argument("second")
    compare.set_defaults(func=cmd_compare)

    selftest = subparsers.add_parser("selftest", help="prove the comparator can fail")
    selftest.set_defaults(func=cmd_selftest)

    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
