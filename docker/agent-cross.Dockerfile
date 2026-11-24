# Cross-builds the stackbox-agent for macOS (aarch64) and Windows (x86_64) with cargo-zigbuild,
# natively on the build host. The macOS link needs the SDK's .tbd stubs, bound in as the
# `sdk` build context (an empty one for Windows).
FROM rust:1.99.1-bookworm AS build
# zig 0.14 and 0.15 segfault linking the macOS agent.
ARG ZIG_VERSION=0.16.0
ARG ZIGBUILD_VERSION=0.24.0
RUN apt-get update && apt-get install -y --no-install-recommends xz-utils && rm -rf /var/lib/apt/lists/* && \
    curl -fsSL "https://ziglang.org/download/$ZIG_VERSION/zig-$(uname -m)-linux-$ZIG_VERSION.tar.xz" \
        | tar -xJ -C /opt && ln -s /opt/zig-*/zig /usr/local/bin/zig && \
    cargo install --locked cargo-zigbuild --version "$ZIGBUILD_VERSION" && \
    rustup target add aarch64-apple-darwin x86_64-pc-windows-gnu
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY docker ./docker
COPY guest ./guest
ARG TARGET
# zig prefixes every -L/-F with SDKROOT, so the SDK paths are given relative to it.
RUN --mount=type=bind,from=sdk,target=/sdk \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    case "$TARGET" in \
        macos-aarch64) triple=aarch64-apple-darwin exe=stackbox-agent; export SDKROOT=/sdk \
            CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="-C link-arg=-L/usr/lib -C link-arg=-F/System/Library/Frameworks" ;; \
        windows-x86_64) triple=x86_64-pc-windows-gnu exe=stackbox-agent.exe ;; \
        *) echo "unknown TARGET $TARGET" >&2; exit 2 ;; \
    esac && \
    cargo zigbuild --locked --release --bin stackbox-agent --target "$triple" && \
    mkdir /out && cp "target/$triple/release/$exe" "/out/$exe"

FROM scratch AS artifact
COPY --from=build /out/ /
