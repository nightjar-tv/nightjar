#!/usr/bin/env python3
"""Generate and check the Nightjar release notice set.

The root NOTICE names the shipped runtime components. The generated
notices/THIRD-PARTY.md derives the Rust coverage from server/Cargo.lock and the
web runtime coverage from web/package-lock.json, joined to the committed
publisher-declared expression map in notices/rust-licenses.json. The checker
also cross-checks the corresponding-source route in docs/RELEASE.md against the
pinned packages in the Dockerfile, and the notice inclusion in the Dockerfile.

Python standard library only.

Subcommands:
  generate  rewrite notices/THIRD-PARTY.md from the committed lockfiles and map
  check     verify the committed notice set, routes and Docker inclusion
  selftest  prove the checker reports a failure on every controlled mutation
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import tempfile
from pathlib import Path

PROJECT_LICENSE = "GPL-3.0-only"
REGISTRY_SOURCE_PREFIX = "registry+"

# The runtime packages the image installs from the pinned Debian snapshot.
RUNTIME_PACKAGES = (
    "ca-certificates",
    "ffmpeg",
    "intel-media-va-driver",
    "mesa-va-drivers",
    "i965-va-driver",
)

# The Debian source package that builds each runtime binary package.
SOURCE_PACKAGES = {
    "ca-certificates": "ca-certificates",
    "ffmpeg": "ffmpeg",
    "intel-media-va-driver": "intel-media-driver",
    "mesa-va-drivers": "mesa",
    "i965-va-driver": "intel-vaapi-driver",
}

# The one production npm dependency compiled into the delivered web assets.
WEB_RUNTIME_EXPECTED = {"hls.js": ("1.6.16", "Apache-2.0")}

# Literal strings the root NOTICE must name.
NOTICE_MARKERS = (
    "Nightjar server binary",
    "GPL-3.0-only",
    "LICENSE",
    "server/Cargo.lock",
    "notices/THIRD-PARTY.md",
    "notices/rust-licenses.json",
    "hls.js 1.6.16",
    "Apache-2.0",
    "notices/hls.js-1.6.16.txt",
    "notices/Apache-2.0.txt",
    "docs/RELEASE.md",
    "ffmpeg",
    "ffprobe",
    "ca-certificates",
    "intel-media-va-driver",
    "mesa-va-drivers",
    "i965-va-driver",
)

# Literal strings docs/RELEASE.md must state about the source route.
SOURCE_MARKERS = (
    "supplied at publication",
    "No Nightjar release or tag exists yet",
)

NOTICE_FILES = (
    "NOTICE",
    "notices/README.md",
    "notices/THIRD-PARTY.md",
    "notices/rust-licenses.json",
    "notices/Apache-2.0.txt",
    "notices/hls.js-1.6.16.txt",
)

PACKAGE_RE = re.compile(r"^\[\[package\]\]$", re.MULTILINE)
NAME_RE = re.compile(r'^name = "([^"]+)"$', re.MULTILINE)
VERSION_RE = re.compile(r'^version = "([^"]+)"$', re.MULTILINE)
SOURCE_RE = re.compile(r'^source = "([^"]+)"$', re.MULTILINE)
APT_PIN_RE = re.compile(r"^\s*([a-z0-9][a-z0-9+.-]*)=([^\s\\]+)\s*\\?$", re.MULTILINE)
SNAPSHOT_RE = re.compile(
    r"snapshot\.debian\.org/archive/(debian-security|debian)/(\d{8}T\d{6}Z)"
)


def parse_cargo_lock(text):
    """Return the [[package]] records in lock order as name/version/source."""
    packages = []
    for block in PACKAGE_RE.split(text)[1:]:
        name = NAME_RE.search(block)
        version = VERSION_RE.search(block)
        if not name or not version:
            raise SystemExit(f"malformed Cargo.lock package block: {block[:80]!r}")
        source = SOURCE_RE.search(block)
        packages.append(
            {
                "name": name.group(1),
                "version": version.group(1),
                "source": source.group(1) if source else None,
            }
        )
    return packages


def lock_key(package):
    return f"{package['name']}@{package['version']}"


def load_license_map(path):
    document = json.loads(Path(path).read_text(encoding="utf-8"))
    crates = document.get("crates")
    if not isinstance(crates, dict):
        raise SystemExit(f"{path} has no 'crates' object")
    return crates


def parse_web_runtime(text):
    """Return the production node_modules records as (name, version, license)."""
    lock = json.loads(text)
    records = {}
    for key, record in lock.get("packages", {}).items():
        if not key.startswith("node_modules/") or record.get("dev"):
            continue
        name = key.rsplit("node_modules/", 1)[-1]
        records[name] = (record.get("version", ""), record.get("license", ""))
    return records


def render_third_party(packages, crates, web_runtime, strict=True):
    lines = [
        "# Third-party notices",
        "",
        "Generated from `server/Cargo.lock`, `web/package-lock.json` and",
        "`notices/rust-licenses.json` by `scripts/check_notices.py generate`.",
        "Do not edit by hand.",
        "",
        "The license expressions below are the publishers' declared metadata, not",
        "an audit of file-level copyright notices.",
        "",
        "## Rust dependencies (`server/Cargo.lock`)",
        "",
        "| Crate | Version | Declared license |",
        "|---|---|---|",
    ]
    for package in sorted(packages, key=lambda item: (item["name"], item["version"])):
        key = lock_key(package)
        if key not in crates:
            if strict:
                raise SystemExit(f"rust-licenses.json has no entry for {key}")
            license_expression = "<missing>"
        else:
            license_expression = crates[key]
        lines.append(f"| {package['name']} | {package['version']} | {license_expression} |")
    lines += [
        "",
        "## Web runtime dependencies (`web/package-lock.json`)",
        "",
        "Only production records are listed; build and test tooling is not",
        "delivered.",
        "",
        "| Package | Version | Declared license |",
        "|---|---|---|",
    ]
    for name in sorted(web_runtime):
        version, license_expression = web_runtime[name]
        lines.append(f"| {name} | {version} | {license_expression} |")
    return "\n".join(lines) + "\n"


def parse_apt_pins(dockerfile):
    pins = {}
    for name, version in APT_PIN_RE.findall(dockerfile):
        if name in RUNTIME_PACKAGES:
            pins[name] = version
    return pins


def check_tree(root):
    """Return the list of notice failures for a tree. Empty means the tree is OK."""
    root = Path(root)
    failures = []

    def require(condition, message):
        if not condition:
            failures.append(message)

    for relative in NOTICE_FILES:
        require((root / relative).is_file(), f"missing notice file: {relative}")
    if failures:
        return failures

    notice = (root / "NOTICE").read_text(encoding="utf-8")
    third_party = (root / "notices" / "THIRD-PARTY.md").read_text(encoding="utf-8")
    apache = (root / "notices" / "Apache-2.0.txt").read_text(encoding="utf-8")
    hls_notice = (root / "notices" / "hls.js-1.6.16.txt").read_text(encoding="utf-8")
    release = (root / "docs" / "RELEASE.md").read_text(encoding="utf-8")
    dockerfile = (root / "Dockerfile").read_text(encoding="utf-8")

    for marker in NOTICE_MARKERS:
        require(marker in notice, f"NOTICE does not name: {marker}")
    require(
        "Apache License\n" in apache and "Version 2.0, January 2004" in apache,
        "notices/Apache-2.0.txt is not the Apache text",
    )
    require("Apache License, Version 2.0" in hls_notice, "hls.js notice has no Apache grant")
    require("hls.js" in hls_notice, "hls.js notice does not name hls.js")

    packages = parse_cargo_lock((root / "server" / "Cargo.lock").read_text(encoding="utf-8"))
    crates = load_license_map(root / "notices" / "rust-licenses.json")
    lock_keys = {lock_key(package) for package in packages}
    require(len(packages) == len(lock_keys), "server/Cargo.lock has duplicate identities")
    missing = sorted(lock_keys - set(crates))
    stale = sorted(set(crates) - lock_keys)
    require(not missing, f"rust-licenses.json misses locked crates: {missing[:5]}")
    require(not stale, f"rust-licenses.json has stale crates: {stale[:5]}")

    web_runtime = parse_web_runtime((root / "web" / "package-lock.json").read_text(encoding="utf-8"))
    for name, expected in WEB_RUNTIME_EXPECTED.items():
        require(
            web_runtime.get(name) == expected,
            f"web runtime {name} is {web_runtime.get(name)!r}, expected {expected!r}",
        )

    generated = render_third_party(packages, crates, web_runtime, strict=False)
    require(
        generated == third_party,
        "notices/THIRD-PARTY.md has drifted; run scripts/check_notices.py generate",
    )
    require("hls.js" in third_party, "THIRD-PARTY.md omits hls.js")

    pins = parse_apt_pins(dockerfile)
    require(
        set(pins) == set(RUNTIME_PACKAGES),
        f"Dockerfile apt pins are {sorted(pins)}, expected {sorted(RUNTIME_PACKAGES)}",
    )
    snapshots = {
        archive: timestamp for archive, timestamp in SNAPSHOT_RE.findall(dockerfile)
    }
    require(
        {"debian", "debian-security"} <= set(snapshots),
        "Dockerfile does not pin both Debian snapshot timestamps",
    )
    for marker in SOURCE_MARKERS:
        require(marker in release, f"docs/RELEASE.md does not state: {marker}")
    for archive, timestamp in snapshots.items():
        route = f"snapshot.debian.org/archive/{archive}/{timestamp}"
        require(route in release, f"docs/RELEASE.md does not record snapshot route {route}")
    debian_timestamp = snapshots.get("debian")
    for package in RUNTIME_PACKAGES:
        version = pins.get(package)
        require(
            version and version in release,
            f"docs/RELEASE.md does not record {package} version {version}",
        )
        source_package = SOURCE_PACKAGES[package]
        require(
            source_package in release,
            f"docs/RELEASE.md does not name Debian source package {source_package}",
        )
        if debian_timestamp:
            pool_route = (
                f"snapshot.debian.org/archive/debian/{debian_timestamp}"
                f"/pool/main/{source_package[0]}/{source_package}/"
            )
            require(
                pool_route in release,
                f"docs/RELEASE.md does not record source route for {package}: {pool_route}",
            )

    require(
        "/usr/share/doc/nightjar/NOTICE" in dockerfile,
        "Dockerfile does not copy NOTICE into /usr/share/doc/nightjar/",
    )
    require(
        "COPY notices/" in dockerfile and "/usr/share/doc/nightjar/notices/" in dockerfile,
        "Dockerfile does not copy notices/ into /usr/share/doc/nightjar/",
    )
    require(
        "docs/RELEASE.md" in dockerfile and "/usr/share/doc/nightjar/SOURCE.md" in dockerfile,
        "Dockerfile does not copy docs/RELEASE.md as SOURCE.md",
    )
    require(
        "/usr/share/doc/nightjar/debian/" in dockerfile and "copyright" in dockerfile,
        "Dockerfile does not copy installed Debian copyright files",
    )
    for package in RUNTIME_PACKAGES:
        require(
            package in dockerfile,
            f"Dockerfile does not name runtime package {package}",
        )
        version = pins.get(package)
        require(
            version and version in notice,
            f"NOTICE does not record {package} version {version}",
        )

    return failures


def cmd_generate(args):
    tree = Path(args.tree).resolve()
    packages = parse_cargo_lock((tree / "server" / "Cargo.lock").read_text(encoding="utf-8"))
    crates = load_license_map(tree / "notices" / "rust-licenses.json")
    web_runtime = parse_web_runtime(
        (tree / "web" / "package-lock.json").read_text(encoding="utf-8")
    )
    output = tree / "notices" / "THIRD-PARTY.md"
    output.write_text(render_third_party(packages, crates, web_runtime), encoding="utf-8")
    print(f"wrote {output}")
    return 0


def cmd_check(args):
    failures = check_tree(args.tree)
    for failure in failures:
        print(f"FAIL {failure}")
    if failures:
        print(f"CHECK failed: {len(failures)} problem(s)", file=sys.stderr)
        return 1
    print("CHECK ok: notice coverage, source route and Docker inclusion")
    return 0


def copy_notice_tree(real, target):
    for relative in NOTICE_FILES:
        destination = target / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(real / relative, destination)
    for relative in ("server/Cargo.lock", "web/package-lock.json", "docs/RELEASE.md", "Dockerfile"):
        destination = target / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(real / relative, destination)


def mutate_text(path, old, new):
    text = path.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"selftest cannot find {old!r} in {path}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def selftest_mutations():
    """Return (name, mutate) pairs. Each mutation must make check_tree fail."""

    def drop_locked_crate(root):
        path = root / "notices" / "rust-licenses.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        document["crates"].pop("adler2@2.0.1")
        path.write_text(json.dumps(document), encoding="utf-8")

    def add_locked_crate(root):
        path = root / "server" / "Cargo.lock"
        path.write_text(
            path.read_text(encoding="utf-8")
            + '\n[[package]]\nname = "notice-selftest-crate"\nversion = "9.9.9"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"\n',
            encoding="utf-8",
        )

    def drop_web_runtime(root):
        path = root / "web" / "package-lock.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        del document["packages"]["node_modules/hls.js"]
        path.write_text(json.dumps(document), encoding="utf-8")

    def drift_notice(root):
        path = root / "notices" / "THIRD-PARTY.md"
        path.write_text(path.read_text(encoding="utf-8") + "drift\n", encoding="utf-8")

    def change_route(root):
        mutate_text(
            root / "docs" / "RELEASE.md",
            "snapshot.debian.org/archive/debian/20260901T000000Z",
            "snapshot.debian.org/archive/debian/19990101T000000Z",
        )

    def drop_docker_copy(root):
        mutate_text(
            root / "Dockerfile",
            "COPY NOTICE /usr/share/doc/nightjar/NOTICE\n",
            "",
        )

    def drop_apache_text(root):
        mutate_text(root / "notices" / "Apache-2.0.txt", "Apache License", "Not a license")

    def drop_notice_marker(root):
        mutate_text(root / "NOTICE", "hls.js 1.6.16", "hls.js")

    def change_notice_version(root):
        mutate_text(root / "NOTICE", "7:5.1.9-0+deb12u1", "0:0")

    return (
        ("drop locked crate from map", drop_locked_crate),
        ("add unlisted locked crate", add_locked_crate),
        ("drop web runtime dependency", drop_web_runtime),
        ("drift generated notice", drift_notice),
        ("change snapshot route", change_route),
        ("drop Docker COPY", drop_docker_copy),
        ("drop Apache text", drop_apache_text),
        ("drop NOTICE marker", drop_notice_marker),
        ("change NOTICE version", change_notice_version),
    )


def cmd_selftest(args):
    real = Path(args.tree).resolve()
    positive = check_tree(real)
    if positive:
        for failure in positive:
            print(f"FAIL {failure}")
        print("selftest: positive control failed", file=sys.stderr)
        return 1
    print(f"CONTROL clean tree -> 0 failures")

    failures = 0
    for name, mutate in selftest_mutations():
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            copy_notice_tree(real, root)
            mutate(root)
            found = check_tree(root)
        if found:
            print(f"CONTROL {name} -> nonzero ({len(found)} failure(s))")
        else:
            print(f"CONTROL {name} -> NOT DETECTED", file=sys.stderr)
            failures += 1

    if failures:
        print(f"selftest: {failures} mutation(s) not detected", file=sys.stderr)
        return 1
    print(f"SELFTEST ok: {len(selftest_mutations())} mutations controlled")
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    generate = subparsers.add_parser("generate", help="rewrite notices/THIRD-PARTY.md")
    generate.add_argument("--tree", default=".", help="repository root")
    generate.set_defaults(func=cmd_generate)

    check = subparsers.add_parser("check", help="verify the committed notice set")
    check.add_argument("--tree", default=".", help="repository root")
    check.set_defaults(func=cmd_check)

    selftest = subparsers.add_parser("selftest", help="prove the checker can fail")
    selftest.add_argument("--tree", default=".", help="repository root")
    selftest.set_defaults(func=cmd_selftest)

    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
