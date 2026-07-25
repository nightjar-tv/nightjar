# Nightjar — single-binary image (Phase 0 hello-world)
# Build context: repo root

FROM node:22-bookworm AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json* ./
RUN npm ci || npm install
COPY web/ ./
RUN npm run build

FROM rust:bookworm AS server
WORKDIR /src
COPY server/ ./server/
COPY --from=web /src/web/build ./web/build
WORKDIR /src/server
RUN cargo build --release -p nightjar-api

FROM debian:bookworm-slim
RUN apt-get update \
	&& apt-get install -y --no-install-recommends ca-certificates \
	&& rm -rf /var/lib/apt/lists/*
COPY --from=server /src/server/target/release/nightjar /usr/local/bin/nightjar
ENV NIGHTJAR_PORT=8096
EXPOSE 8096
ENTRYPOINT ["nightjar"]
