# Production image: stackbox coordinator (`stackbox serve`) + web portal (`web/server`, static `web/dist`).
# Build context is the repository root; see .dockerignore.

# ---- Stage 1: stackbox binary (same toolchain as docker/agent.Dockerfile) ----
FROM rust:1.99.1-bookworm AS rust-builder
WORKDIR /build
# build.rs hashes these inputs into STACKBOX_BUILD_ID and reads no .git (BUILD_ID env overrides it).
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY guest ./guest
COPY docker ./docker
COPY tests ./tests
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release --bin stackbox && \
    cp target/release/stackbox /stackbox

# ---- Stage 2: portal static bundle ----
FROM node:22-bookworm-slim AS web-builder
WORKDIR /app/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# ---- Stage 3: runtime ----
FROM node:22-bookworm-slim AS runtime

# curl: entrypoint readiness probe and HEALTHCHECK. openssh-client: SSH to remote hosts.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl openssh-client \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 10001 stackbox \
    && useradd --system --uid 10001 --gid stackbox --home-dir /data --shell /usr/sbin/nologin stackbox \
    && mkdir -p /data \
    && chown stackbox:stackbox /data

COPY --from=rust-builder /stackbox /usr/local/bin/stackbox
COPY docker/entrypoint.sh /usr/local/bin/stackbox-entrypoint
# web/server is dependency-free ESM (node: built-ins only), so no node_modules are needed.
COPY web/package.json /app/web/package.json
COPY web/server /app/web/server
COPY --from=web-builder /app/web/dist /app/web/dist
RUN chmod 0755 /usr/local/bin/stackbox-entrypoint /usr/local/bin/stackbox

ENV NODE_ENV=production \
    HOME=/data \
    STACKBOX_STATE_DIR=/data \
    STACKBOX_API_BIND=127.0.0.1:10010 \
    PORT=10020 \
    PORTAL_HOST=0.0.0.0

VOLUME ["/data"]
WORKDIR /app/web
USER stackbox

# Probes the portal and, through its proxy, the coordinator API (the proxy adds STACKBOX_API_TOKEN).
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD curl -fsS -o /dev/null "http://127.0.0.1:${PORT}/api/v1/host" || exit 1

# 10020: web portal (HTTP). 10011: private relay (STACKBOX_RELAY_TLS=private). 10012: relay for proxy/tunnel mode.
EXPOSE 10020 10011 10012

ENTRYPOINT ["/usr/local/bin/stackbox-entrypoint"]
