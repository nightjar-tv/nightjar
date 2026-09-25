# Release inputs

Nightjar ships the server as one binary. The current Linux server artifact is a
Docker image built on `linux/amd64`. The planned Apple Silicon server release
path is a native `macos/arm64` build. Every released artifact records the inputs
that produced it in a manifest. The manifest lets a reviewer compare two
releases of the same artifact without rebuilding either.

## Artifacts and platforms

A manifest describes one artifact on one platform. The Linux container and the
native macOS server are separate artifacts, so each has its own manifest and
they are not compared across platforms.

| Artifact | Platform | Build path |
|---|---|---|
| Linux container image | `linux/amd64` | The pinned `Dockerfile` |
| Planned native macOS server | `macos/arm64` (or `macos/amd64` on Intel) | A native build on a Mac; packaging is not yet selected |

VideoToolbox is a macOS framework. A process must run natively on macOS to use
it; a Linux container on a Mac does not reach the framework. The Linux image is
therefore the software and Linux-hardware path. Selecting, signing and
notarizing a macOS package is not decided here.

## Pinned inputs

The `Dockerfile` pins:

- the three base images by readable tag and digest: `node:22-bookworm`,
  `rust:bookworm` and `debian:trixie-slim`;
- the Debian and Debian-security snapshot timestamps, so `apt` resolves the
  package set from an immutable archive;
- the six installed packages by exact version: `ca-certificates`, `ffmpeg`,
  `intel-media-va-driver-non-free`, `libmfx-gen1.2`, `mesa-va-drivers` and
  `i965-va-driver`;
- the build commands: `npm ci` in the web stage and
  `cargo build --release --locked` in the server stage.

## The manifest

`scripts/check_release_inputs.py emit` writes one canonical JSON manifest per
artifact on one platform. The manifest records:

- `source.commit`, the full git object name;
- `target.platform`, the build platform;
- `toolchain.*`, the Rust, Cargo, Node and npm versions the build used;
- `locks.cargo.sha256` and `locks.npm.sha256`, the hashes of `Cargo.lock` and
  `package-lock.json`;
- `bases.*.ref` and `bases.*.digest`, each base image reference and digest;
- `snapshots.debian.timestamp` and `snapshots.debian_security.timestamp`;
- `dpkg.*`, each installed package's binary version, source package, source
  version and architecture;
- `ffmpeg.buildconf`, the configuration line from `ffmpeg -buildconf`.

The manifest also records the image ID, the binary SHA-256 and the emission
time as output evidence. Those fields, and any absolute build path, are not
governed and take no part in the comparison.

## Comparison

`scripts/check_release_inputs.py compare A B` compares the governed projection
of two manifests. It prints `OK <n> governed fields equal` and exits 0 when the
recorded inputs are equal. It prints one `DIFF <field>: <a> != <b>` line per
difference and exits nonzero otherwise. It prints `IGNORED <n> non-governed
fields` for the fields it excludes.

Two builds of the same source commit and the same pinned inputs must compare
equal. This is a comparison of inputs. It is not a byte-identical claim: build
timestamps, file ordering, build IDs, archive metadata and the network can
differ between two runs, and the comparator excludes image and binary bytes.

`scripts/check_release_inputs.py selftest` proves the comparator can fail. It
compares a synthetic manifest to itself, then mutates one governed field at a
time and requires each mutation to be reported. It also checks the target
mapping, including `aarch64-apple-darwin` to `macos/arm64` and
`x86_64-apple-darwin` to `macos/amd64`. The `release-inputs` CI job runs this
check beside the two-build comparison.

## Corresponding source and notices

Nightjar ships the notice set inside the runtime image at
`/usr/share/doc/nightjar/`. The root `NOTICE` names the shipped runtime
components; `notices/THIRD-PARTY.md` carries the Rust and web dependency
notices; `notices/Apache-2.0.txt` and `notices/hls.js-1.6.16.txt` carry the
bundled `hls.js` 1.6.16 notice; `SOURCE.md` is this document; and
`debian/<package>/copyright` is the installed Debian copyright file for each
redistributed package.

The Nightjar source for a release is the repository at the release tag. The
Nightjar release/tag URL is supplied at publication.

No Nightjar release or tag exists yet, and this document does not claim that
one does.

The runtime image redistributes these Debian binary packages. Their
corresponding source is retrievable from the same Debian Snapshot archive the
image installs from:

| Binary package | Source package | Version | Snapshot source route |
|---|---|---|---|
| `ca-certificates` | `ca-certificates` | `20250419` | `snapshot.debian.org/archive/debian/20260901T000000Z/pool/main/c/ca-certificates/` |
| `ffmpeg` | `ffmpeg` | `7:7.1.5-0+deb13u1` | `snapshot.debian.org/archive/debian/20260901T000000Z/pool/main/f/ffmpeg/` |
| `intel-media-va-driver-non-free` | `intel-media-driver-non-free` | `25.2.3+ds1-1` | `snapshot.debian.org/archive/debian/20260901T000000Z/pool/non-free/i/intel-media-driver-non-free/` |
| `libmfx-gen1.2` | `onevpl-intel-gpu` | `25.1.4-1` | `snapshot.debian.org/archive/debian/20260901T000000Z/pool/main/o/onevpl-intel-gpu/` |
| `mesa-va-drivers` | `mesa` | `25.0.7-2+deb13u1` | `snapshot.debian.org/archive/debian/20260901T000000Z/pool/main/m/mesa/` |
| `i965-va-driver` | `intel-vaapi-driver` | `2.4.1+dfsg1-2` | `snapshot.debian.org/archive/debian/20260901T000000Z/pool/main/i/intel-vaapi-driver/` |

The Debian archive is pinned at
`snapshot.debian.org/archive/debian/20260901T000000Z` and the security archive
at `snapshot.debian.org/archive/debian-security/20260901T000000Z`. A `deb-src`
line with the same timestamp, followed by `apt-get source <source package>`,
fetches the exact source. `scripts/check_notices.py check` fails if this route
drifts from the `Dockerfile` pins.

### The complete installed-package record

The image writes a record of every installed binary package to
`/usr/share/doc/nightjar/debian/installed-packages.txt`. The `Dockerfile`
builds it with `dpkg-query` after installation and sorts it, so the same pinned
inputs produce the same record. Each line is tab-separated:

```text
<binary package> <binary version> <source package> <source version> <architecture>
```

The image does not duplicate the transitive copyright files. Each installed
package keeps its Debian copyright file at the standard
`/usr/share/doc/<binary package>/copyright` path, and native Linux CI checks
that every package in the record has a readable file there.

### Retrieving the source of any recorded package

The record names the exact source package and source version for every
installed package, so one route covers all of them. The route uses only the
pinned snapshots and needs no live mirror:

1. Read the source package and source version from the record line.
2. Point `deb-src` at both pinned snapshots:

   ```text
   deb-src http://snapshot.debian.org/archive/debian/20260901T000000Z trixie main non-free
   deb-src http://snapshot.debian.org/archive/debian-security/20260901T000000Z trixie-security main non-free
   ```

3. Fetch the exact version:

   ```text
   apt-get -o Acquire::Check-Valid-Until=false update
   apt-get source <source package>=<source version>
   ```

4. If `apt-get source` cannot serve the exact version, fetch the matching
   `.dsc`, `.orig.tar.*` and `.debian.tar.*` files from the archive pool. The
   main archive serves `pool/<component>` and the security archive serves
   `pool/updates/<component>`:

   ```text
   snapshot.debian.org/archive/debian/<timestamp>/pool/<component>/<letter>/<source package>/
   snapshot.debian.org/archive/debian-security/<timestamp>/pool/updates/<component>/<letter>/<source package>/
   ```

   `<component>` is the archive component that holds the source package: `main`
   or `non-free`. `<letter>` is the first letter of the source package name.
   `<timestamp>` is the matching pinned timestamp. Fetch only the exact version
   the record names; a pool directory can hold more than one version.

This procedure is a retrieval route. It is not a license conclusion or a legal
clearance, and it states no public release.

## Refreshing a pin

A refresh is an explicit release-maintenance action, not a calendar cadence.
Refresh the pins in two cases:

- before a release, so the release records current inputs;
- promptly when a pinned base image, snapshot or apt package has an applicable
  security update.

There is no weekly or other periodic refresh. A cadence would move inputs
without a security reason and churn the manifest.

A refresh is an ordinary pull request. Its CI artifact is the field diff
between the old manifest and the new one, so the reviewer sees exactly which
input moved. A refresh that changes nothing produces no diff.

## CI

The `release-inputs` job proves the `linux/amd64` Docker path. It builds the
image twice with no shared cache, captures one probe per build, emits
`manifest-a.json` and `manifest-b.json`, runs `compare` and `selftest`, and
uploads all four files as artifacts. It builds only `linux/amd64`; it does not
emulate another platform. Generated manifests are never committed.
