# Builds the collector and the MCP server as static musl binaries in one small image.
#   container build --platform linux/arm64 -t nibe-hub:latest .
#   container image save --platform linux/arm64 -o nibe-hub-arm64.tar nibe-hub:latest
# or push it to a registry for a MikroTik App: tools/push_image.sh
# On an Apple Silicon Mac this is a native build (no emulation). See README.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && mkdir /out && cp target/release/nibe-collector target/release/nibe-mcp /out/

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /out/nibe-collector /out/nibe-mcp /
ENV DB_PATH=/data/nibe.db RUST_LOG=info
VOLUME /data
EXPOSE 8080 443
# A default command rather than a fixed entrypoint, so the binary can be chosen per container:
# MikroTik Apps only have `command` (e.g. `command: /nibe-mcp`); RouterOS /container and
# `container run` can also set entrypoint= / --entrypoint.
CMD ["/nibe-collector"]
