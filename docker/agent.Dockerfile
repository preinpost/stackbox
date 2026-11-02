FROM rust:1.99.1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY docker ./docker
COPY guest ./guest
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release --bin farm-agent && \
    mkdir /out && cp target/release/farm-agent /out/farm-agent

FROM scratch AS artifact
COPY --from=build /out/farm-agent /farm-agent
