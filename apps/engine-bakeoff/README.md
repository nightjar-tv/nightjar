# Engine bake-off (measurement only)

Not Phase 4. Not a product client. Exists to score T2/T3/T4 and Part B/C
against `notes/client-arch/engine-bakeoff.md`.

Rule 2.4’s single Rust/libmpv core is what the client ADR will supersede once
this bake-off names the path.

## Engines

- **media_kit** (libmpv) — Flutter path for composition / OSD.
- **libvlc_bakeoff** — thin Dart FFI to VLC.app `libvlc.dylib`. No maintained
  macOS Flutter VLC plugin; that FFI tax is a pre-code T3 finding.

## Part A URL resolution

Nightjar `GET /api/v0/items/{id}/stream` is gated by hardcoded `BROWSER_V0`
and returns **415** for typical Matroska. The measurement harness serves the
same file bytes via:

```
python3 apps/engine-bakeoff/tool/dp_byte_serve.py   # :18097
```

URL shape: `http://127.0.0.1:18097/items/{id}/stream` (Range supported).
Request patterns are logged to `/tmp/bakeoff-request-pattern.jsonl`.

## Run measurements

```bash
bash apps/engine-bakeoff/tool/run_phased.sh
```

Outputs land in `notes/client-arch/bakeoff-runs/`.

## Flutter UI (composition)

```bash
export PATH="$PWD/.tools/flutter/bin:$PATH"
cd apps/engine-bakeoff
flutter run -d macos
```

`BAKEOFF_AUTO=1 flutter run -d macos --dart-define=...` or `--auto` runs the
Dart suite (media_kit + libvlc FFI) when the macOS surface is available.
