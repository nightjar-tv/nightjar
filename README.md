# nightjar

**A free, open-source media server that comes alive when the lights go out.**

Nightjar turns any machine into a private streaming service for your movies and
shows. It's a single small binary written in Rust — no runtime, no external
database, no accounts, no telemetry.

**Status:** Phase 0 foundations. Nothing is playable yet.

Read [ENGINEERING_RULES.md](ENGINEERING_RULES.md) before contributing.
Git workflow: [docs/GIT_RULES.md](docs/GIT_RULES.md).

## Quick start (scaffold)

```bash
# Web UI (embedded into the binary)
cd web && npm install && npm run build && cd ..

# Server
cd server && cargo run -p nightjar-api
# open http://localhost:8096
```

Or with Docker (from repo root, after the image builds):

```bash
docker build -t nightjar/nightjar .
docker run --rm -p 8096:8096 nightjar/nightjar
```

## License

GPL-3.0.
