# Nightjar — single-binary image with FFmpeg on PATH (Gate 2 HW claim).
# Build context: repo root.
#
# FFmpeg is Debian bookworm's package, invoked as an external process (not
# linked). Intel VAAPI/QSV need the media drivers in the image and
# --device=/dev/dri at run time. We do not vendor a competitor's FFmpeg build.
#
# Every release input is pinned: the three base images by readable tag and
# digest, the Debian packages by snapshot timestamp and exact version. ADR-0056
# defines the manifest that records them and docs/RELEASE.md says how a pin is
# refreshed.

FROM node:22-bookworm@sha256:8a34c4ab3ea2c5cd194f07e317b2a8f09461d3c8b05c4e34c8ccd56d56024c4d AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json* ./
RUN npm ci
COPY web/ ./
RUN npm run build
RUN node --version > /nightjar-toolchain-web.txt \
	&& npm --version >> /nightjar-toolchain-web.txt

FROM rust:bookworm@sha256:9a73a5088750b4c95158ab26629c854c3d6fc4b173cb7bc8079ad252d8ed7bfa AS server
WORKDIR /src
COPY server/ ./server/
COPY --from=web /src/web/build ./web/build
WORKDIR /src/server
RUN cargo build --release --locked -p nightjar-api
RUN rustc --version > /nightjar-toolchain-server.txt \
	&& cargo --version >> /nightjar-toolchain-server.txt

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime
# Snapshot timestamps fix the apt package set. Refresh them only for a release
# or an applicable security update (docs/RELEASE.md).
RUN rm -f /etc/apt/sources.list.d/debian.sources \
	&& printf '%s\n' \
		'deb http://snapshot.debian.org/archive/debian/20260901T000000Z bookworm main' \
		'deb http://snapshot.debian.org/archive/debian-security/20260901T000000Z bookworm-security main' \
		> /etc/apt/sources.list \
	&& apt-get -o Acquire::Check-Valid-Until=false update \
	&& apt-get install -y --no-install-recommends \
		ca-certificates=20250419~deb12u1 \
		ffmpeg=7:5.1.9-0+deb12u1 \
		intel-media-va-driver=23.1.1+dfsg1-1 \
		mesa-va-drivers=22.3.6-1+deb12u2 \
		i965-va-driver=2.4.1+dfsg1-1 \
	&& rm -rf /var/lib/apt/lists/*
# Ship the notice set, the corresponding-source route and the installed Debian
# copyright files for the packages this image redistributes. This adds files
# only; it changes no runtime behaviour and vendors no third-party binary.
RUN mkdir -p /usr/share/doc/nightjar/debian
COPY NOTICE /usr/share/doc/nightjar/NOTICE
COPY notices/ /usr/share/doc/nightjar/notices/
COPY docs/RELEASE.md /usr/share/doc/nightjar/SOURCE.md
RUN for pkg in ca-certificates ffmpeg intel-media-va-driver mesa-va-drivers i965-va-driver; do \
		mkdir -p "/usr/share/doc/nightjar/debian/$pkg"; \
		cp -a "/usr/share/doc/$pkg/copyright" "/usr/share/doc/nightjar/debian/$pkg/copyright"; \
	done
RUN mkdir -p /etc/nightjar-release-inputs
COPY --from=web /nightjar-toolchain-web.txt /etc/nightjar-release-inputs/toolchains-web.txt
COPY --from=server /nightjar-toolchain-server.txt /etc/nightjar-release-inputs/toolchains-server.txt
COPY --from=server /src/server/target/release/nightjar /usr/local/bin/nightjar
ENV NIGHTJAR_PORT=8096
# Help VAAPI find drivers in slim images.
ENV LIBVA_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri
EXPOSE 8096
ENTRYPOINT ["nightjar"]
