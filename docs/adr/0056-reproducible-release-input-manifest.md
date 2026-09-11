# ADR-0056: A reproducible release input manifest

- Status: **Accepted** (2026-09-11)
- Date: 2026-09-11
- Amended: 2026-09-11 — a manifest is per artifact and platform; the macOS
  server path is native, not the Linux container.
- Depends on: Rule 1.2 (single binary), Rule 4.9 (shape before writers),
  Rule 4.14 (a number carries its provenance), Rule 6.1 (ADRs for
  irreversible decisions)

## Context

The image is built from three `FROM` lines that name mutable tags, and its
package layer resolves whatever Debian currently serves. `npm ci || npm
install` and an unlocked `cargo build` add two more moving inputs. Nothing
records which inputs produced a released image, so two builds of the same source
commit can differ with no source change, and a reviewer cannot say what moved
between two releases.

`Dockerfile:8`, `Dockerfile:15`, and `Dockerfile:22` name `node:22-bookworm`,
`rust:bookworm`, and `debian:bookworm-slim`. A tag is a pointer. It can be
retargeted without notice, and the same tag can resolve to different bytes on
different days or architectures.

This record defines the manifest that records those inputs and the comparison
that makes a change visible. It is an input record. It is not a claim that two
builds of the same inputs produce identical bytes.

## Decision

### 1. Pin every base image by readable tag and digest

All three `FROM` images are pinned as `name:tag@sha256:<digest>`. The tag stays
readable and the digest is authoritative. The manifest records both. A tag that
has moved is visible because its digest no longer matches the recorded one.

### 2. Pin Debian package resolution to snapshot timestamps

The package layer installs from Debian snapshot archives, not from the live
mirror. Both the main Debian archive and the Debian security archive are pinned
to explicit snapshot timestamps, and the manifest records both timestamps
separately. A security refresh can then move the security timestamp alone
without moving the main archive.

The manifest records the installed packages rather than trusting the names in
the `apt-get install` line. For each installed package it records the binary
version, the source package version, and the architecture, read back from
`dpkg-query` in the built image.

### 3. Lock the build commands

The web stage runs `npm ci` and no fallback. The current `npm ci || npm install`
makes the install path depend on network state. The server stage runs
`cargo build --locked`. Neither the lockfile nor the dependency graph may move
during a release build.

### 4. What the manifest records

One manifest per release artifact and platform, as canonical JSON with UTF-8
encoding, sorted object keys, compact separators and a final newline:

- the source commit (the full git object name);
- the target platform (for example `linux/amd64` or `macos/arm64`);
- the build toolchain versions (Rust, Cargo, Node, npm) as the build reports
  them;
- the SHA-256 of `Cargo.lock` and of `package-lock.json`;
- each base image reference and its digest;
- each installed package's binary version, source package version, and
  architecture;
- the Debian and Debian-security snapshot timestamps;
- the output of `ffmpeg -buildconf`.

**The Linux container artifact and the native macOS artifact are separate.**
The Linux server artifact is the Docker image built on `linux/amd64`, and the
existing two-build CI proof covers that path. A native macOS server build is a
different artifact on `macos/arm64` (or `macos/amd64` on Intel). VideoToolbox
is a macOS framework: a process must run natively on macOS to use it, and a
Linux container on a Mac does not reach the framework. Each artifact gets its
own manifest, and a Linux manifest and a macOS manifest are not compared across
platforms. This record does not select a macOS packaging, signing or
notarization mechanism; that remains part of the open publication decision.

CI emits the manifest beside its release-build evidence as an uploaded artifact.
Generated manifests are not committed. A published release will attach the
manifest beside the image or binary it describes; publication mechanics remain
outside this decision.

`ffmpeg -buildconf` is recorded because the package version alone does not
describe the build options the image ships. It is evidence, not a promise about
what the binary does.

### 5. Deterministic normalization and comparison

Normalization makes two manifests comparable:

- lists sort by a stable key: packages by package name, base images by stage
  order, toolchains by name;
- hashes are lowercase hexadecimal;
- `ffmpeg -buildconf` is reduced to its configuration line, with runs of
  whitespace collapsed to one space;
- JSON object keys are recursively sorted and arrays use the stable keys above.

Comparison is field by field. Each difference is reported as an input delta
with the old and new value. The comparison classifies the delta by kind (base
image digest, snapshot timestamp, package version, lock hash, toolchain
version) so a reviewer can see whether a release moved its inputs and which
input moved. Equal normalized manifests mean the recorded inputs are equal.
They do not mean the outputs are identical.

### 6. What this does not claim

**This is not byte-for-byte reproducible build.** Build timestamps, file
ordering, build IDs, archive metadata, and the network can differ between two
runs even with identical recorded inputs. The manifest compares inputs. It
makes no claim that the resulting binaries or images are identical, and no
acceptance test may read it as one.

### 7. Refresh is a release-maintenance action, not a cadence

Refreshing the pinned inputs is explicit. It happens when an applicable
security update lands and before a release, not on a calendar. There is no
weekly or other periodic refresh. A cadence would move inputs without a
security reason and would make the manifest churn.

When a refresh changes an input, CI shows the input delta on the change, so the
reviewer sees exactly what moved. A refresh that changes nothing produces no
delta.

## Consequences

**Good**

- A release names the inputs that produced it. Two releases can be compared
  without rebuilding either.
- A retargeted base tag or a moved package version becomes visible instead of
  silent.
- The security snapshot is separate from the main snapshot, so a security
  refresh is a small, reviewable delta.

**Bad (accepted)**

- Every base tag and both snapshot timestamps become manual maintenance. A
  snapshot archive can be slow or unavailable, so a refresh needs network
  access to `snapshot.debian.org`.
- The manifest is one more file to produce and compare at release time.
- None of this proves output identity, which is the limit stated in decision 6.

**Open, deliberately not decided here**

- The future release publication mechanism is not selected here. Whatever
  mechanism is selected must carry the per-platform manifest beside the
  artifact it describes.

## Alternatives considered

**Trust the tags and record only the source commit.** Simplest, and it is what
the repository does today. Rejected: it cannot answer what produced a release,
and a retargeted tag is exactly the failure the record exists to catch.

**Live Debian mirrors plus recorded package versions.** Keeps the current
`apt-get update` path. Rejected: the recorded version is only as good as the
mirror at build time, and a rebuild of the same commit later cannot reinstall
the same set. The snapshot timestamps are what make the package layer
reconstructible.

**Claim byte-for-byte reproducibility.** Stronger, and it would subsume the
manifest. Rejected: it is not true for this build and the record must not
overstate what it checks.

**A fixed refresh cadence.** Predictable and easy to schedule. Rejected: it
moves inputs on a clock rather than for a security reason, and it turns the
input delta into routine noise.
