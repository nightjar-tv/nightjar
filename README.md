# nightjar

**A free, open-source media server that comes alive when the lights go out.**

Nightjar turns any machine into a private streaming service for your movies and
shows. It is one small executable written in Rust: server, scanner, and web UI
together, with SQLite for local data. FFmpeg and ffprobe are documented external
media tools. There is no external database, no Nightjar-hosted account, and no
telemetry.

## Why Nightjar

Nightjar is one executable: server, scanner, and web UI. It starts in
milliseconds and idles well under 50 MB of RAM. Point it at a folder and the
index pass makes items browsable as they appear; H.264 + AAC in MP4 plays
directly in the browser.

Nightjar's self-hosted Core is free software under [GPL-3.0-only](LICENSE). Local
playback and self-managed access do not require a Nightjar subscription or a
Nightjar-hosted account. Optional Plus services are post-v1, separately gated,
and not available yet. Your data stays in one SQLite file on your disk. The web
UI uses the same public HTTP API any other client would; there are no private
endpoints.

## Quick start

Build and run from source (this is the path that works today):

```bash
cd web && npm ci && npm run codegen && npm run build && cd ..
cd server && cargo run -p nightjar-api
```

Open `http://localhost:8096`, create the first local owner account, add a
library folder, scan, and press play.

`ffprobe` must be on `PATH`. `NIGHTJAR_DATA_DIR` defaults to `./data`.
`NIGHTJAR_PORT` defaults to `8096`.

Or build the Docker image from this repo. The image includes Debian’s `ffmpeg`
/`ffprobe` plus Intel/Mesa VA drivers (external process, not linked). Pass
`/dev/dri` when you want hardware encode:

```bash
docker build -t nightjar/nightjar .
docker run --rm -p 8096:8096 \
  --device=/dev/dri \
  -v /path/to/media:/media \
  -v /path/to/config:/config \
  -e NIGHTJAR_DATA_DIR=/config \
  nightjar/nightjar
```

Without `--device=/dev/dri`, startup still verifies `libx264` and falls back to
software. Bare-binary installs are unchanged: put any FFmpeg (with VAAPI/QSV if
you want hardware) on `PATH` yourself.

Published image tags and GitHub Releases are not available yet.

## Status

The server is under active development toward v1. It includes library
scanning, playback delivery and local accounts/profiles. The current source
and API contract, rather than older roadmap descriptions, define implemented
behaviour. First-party client and release qualification remain unfinished;
this README does not claim the v1 gates have passed.

Build and run from source using the instructions above. See the ADR register
for accepted decisions and their implementation qualifications.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

GPL-3.0-only. Your media server should belong to you. See [LICENSE](LICENSE).
