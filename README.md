# nightjar

**A free, open-source media server that comes alive when the lights go out.**

Nightjar turns any machine into a private streaming service for your movies and
shows. It is a single small binary written in Rust: no runtime, no external
database, no accounts, no telemetry.

## Why Nightjar

Nightjar is one executable: server, scanner, and web UI. It starts in
milliseconds and idles well under 50 MB of RAM. Point it at a folder and the
index pass makes items browsable as they appear; H.264 + AAC in MP4 plays
directly in the browser.

Every feature is free for everyone, forever. No premium tier exists or ever
will. That is the license ([GPL-3.0](LICENSE)). Your data stays in one SQLite
file on your disk. The web UI uses the same public HTTP API any other client
would; there are no private endpoints.

## Quick start

Build and run from source (this is the path that works today):

```bash
cd web && npm ci && npm run codegen && npm run build && cd ..
cd server && cargo run -p nightjar-api
```

Open `http://localhost:8096`, add a library folder, scan, and press play.

`ffprobe` must be on `PATH`. `NIGHTJAR_DATA_DIR` defaults to `./data`.
`NIGHTJAR_PORT` defaults to `8096`.

Or build the Docker image from this repo:

```bash
docker build -t nightjar/nightjar .
docker run --rm -p 8096:8096 \
  -v /path/to/media:/media \
  -v /path/to/config:/config \
  -e NIGHTJAR_DATA_DIR=/config \
  nightjar/nightjar
```

Published image tags and GitHub Releases are not available yet.

## Status

Nightjar is in active development toward v1.

Working now (Phase 1): libraries, async scan (index then probe), item list,
direct-play streaming with HTTP Range, embedded web UI. Single-user, no auth.

Not built yet: remux/transcode, multi-user, watch state/resume, metadata
providers, official app clients. See the ADRs under `docs/adr/` for decisions
already locked, and [ENGINEERING_RULES.md](ENGINEERING_RULES.md) for v1 scope
(Live TV, DVR, plugins, and music are out).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

GPL-3.0. Your media server should belong to you. See [LICENSE](LICENSE).
