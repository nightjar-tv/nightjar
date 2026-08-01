# Nightjar — single-binary image with FFmpeg on PATH (Gate 2 HW claim).
# Build context: repo root.
#
# FFmpeg is Debian bookworm's package, invoked as an external process (not
# linked). Intel VAAPI/QSV need the media drivers in the image and
# --device=/dev/dri at run time. We do not vendor a competitor's FFmpeg build.
# See nightjar-meta/notes/hw/packaging-ffmpeg-image.md.

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
	&& apt-get install -y --no-install-recommends \
		ca-certificates \
		ffmpeg \
		intel-media-va-driver \
		mesa-va-drivers \
		i965-va-driver \
	&& rm -rf /var/lib/apt/lists/*
COPY --from=server /src/server/target/release/nightjar /usr/local/bin/nightjar
ENV NIGHTJAR_PORT=8096
# Help VAAPI find drivers in slim images.
ENV LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri
EXPOSE 8096
ENTRYPOINT ["nightjar"]
