# Builds the collector and the MCP server as static musl binaries in one small image.
#   container build --platform linux/arm64 -t nibe-hub:latest .
#   container image save --platform linux/arm64 -o nibe-hub-arm64.tar nibe-hub:latest
# or push it to a registry for a MikroTik App: tools/push_image.sh
# On an Apple Silicon Mac this is a native build (no emulation). See README.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev build-base
WORKDIR /src

# 1. Dependencies only. This layer depends on the manifests and Cargo.lock alone, so it is
#    reused (locally, and from the GitHub Actions cache) until a dependency changes. No cache
#    mounts here: the compiled dependencies in target/ must be part of the layer to be cached.
COPY Cargo.toml Cargo.lock ./
COPY crates/nibe-core/Cargo.toml crates/nibe-core/
COPY crates/nibe-collector/Cargo.toml crates/nibe-collector/
COPY crates/nibe-mcp/Cargo.toml crates/nibe-mcp/
RUN mkdir -p crates/nibe-core/src crates/nibe-collector/src crates/nibe-mcp/src \
 && touch crates/nibe-core/src/lib.rs \
 && echo 'fn main() {}' > crates/nibe-collector/src/main.rs \
 && echo 'fn main() {}' > crates/nibe-mcp/src/main.rs \
 && cargo build --release --locked \
 && cargo clean --release -p nibe-core -p nibe-collector -p nibe-mcp \
 && rm -rf crates/*/src

# 2. The workspace crates. A code change only rebuilds from here.
#    The stub builds are cleaned above, and the sources are touched because COPY keeps their
#    original timestamps (with buildx these can be older than the stub build, and cargo would
#    then consider the stubs up to date).
COPY crates crates
RUN find crates -type f -exec touch {} + \
 && cargo build --release --locked \
 && mkdir /out && cp target/release/nibe-collector target/release/nibe-mcp /out/ \
 && /out/nibe-collector 2>&1 | grep -q 'NIBE_URL is missing' \
 && /out/nibe-mcp 2>&1 | grep -q 'MCP_TOKEN is missing' \
 || { echo 'the built binaries are not the real nibe-collector/nibe-mcp' >&2; exit 1; }

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
