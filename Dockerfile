#
# Build from the core repository with the web repo as a named context:
#   docker build --build-context web=../localreel-web -t localreel .
#
# BuildKit is required for named contexts (Docker Desktop/Engine 23+).

FROM node:22-bookworm-slim AS web-build
WORKDIR /web
COPY --from=web package.json package-lock.json tsconfig.json vite.config.ts index.html ./
COPY --from=web src ./src
RUN npm ci
RUN npm run build

FROM rust:1.85-bookworm AS builder
WORKDIR /src
COPY . .
COPY --from=web-build /web/dist /localreel-web/dist
RUN cargo build --release --locked -p localreel-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg tini \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --create-home --home-dir /data localreel \
    && mkdir -p /data \
    && chown -R localreel:localreel /data
COPY --from=builder /src/target/release/localreel /usr/local/bin/localreel
VOLUME ["/data"]
EXPOSE 3001
# The container must bind 0.0.0.0 to be reachable through a published port.
# Keep the host-side publish on 127.0.0.1 in Compose; this disables the
# in-app loopback guard, so do not expose the port to untrusted networks.
ENV LOCALREEL_HOST=0.0.0.0 \
    LOCALREEL_ALLOW_REMOTE=1 \
    LOCALREEL_PORT=3001 \
    LOCALREEL_DATA_DIR=/data \
    RUST_LOG=info
USER localreel
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3001/api/v1/health || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/localreel", "serve"]
