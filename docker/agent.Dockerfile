FROM rust:1.99.1-bookworm AS build
# The build context has no .git, so the caller passes the revision it built from.
ARG FARM_BUILD_ID=unknown
ENV FARM_BUILD_ID=${FARM_BUILD_ID}
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY docker ./docker
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release --bin farm-agent && \
    mkdir /out && cp target/release/farm-agent /out/farm-agent

FROM scratch AS artifact
COPY --from=build /out/farm-agent /farm-agent
