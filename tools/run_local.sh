#!/bin/sh
# Runs nibe-hub locally with Apple Containers (the `container` CLI).
#
# Collector and MCP server run in ONE container: every Apple container is its own VM, and the
# SQLite WAL database can only be shared by processes on the same kernel.
#
#   tools/run_local.sh up      # start (or restart) collector + MCP server
#   tools/run_local.sh down    # stop and remove the container (the volume keeps the history)
#   tools/run_local.sh logs    # collector logs
#
# Settings (environment variables): ENV_FILE (.env), NAME (nibe), IMAGE (nibe-hub:latest),
# VOLUME (nibe-data), PORT (127.0.0.1:8080).
set -eu

ENV_FILE=${ENV_FILE:-.env}
NAME=${NAME:-nibe}
IMAGE=${IMAGE:-nibe-hub:latest}
VOLUME=${VOLUME:-nibe-data}
PORT=${PORT:-127.0.0.1:8080}

exists() { container inspect "$NAME" >/dev/null 2>&1; }

# Each process gets only its own settings from $ENV_FILE: the collector never sees MCP_TOKEN
# and the MCP server never sees the pump password.
COLLECTOR_VARS='^(NIBE_[A-Z_]+|DB_PATH|RUST_LOG)='
MCP_VARS='^(MCP_[A-Z_]+|DB_PATH|TZ_NAME|RUST_LOG)='

split_env() {
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    grep -E "$COLLECTOR_VARS" "$ENV_FILE" > "$TMP/collector.env" || true
    grep -E "$MCP_VARS" "$ENV_FILE" > "$TMP/mcp.env" || true
}

case "${1:-}" in
up)
    [ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE (see README)" >&2; exit 1; }
    split_env
    container volume inspect "$VOLUME" >/dev/null 2>&1 || container volume create "$VOLUME" >/dev/null
    if exists; then
        # recreate, so changes to $ENV_FILE take effect
        container stop "$NAME" >/dev/null 2>&1 || true
        container rm "$NAME" >/dev/null
    fi
    container run -d --name "$NAME" --env-file "$TMP/collector.env" -v "$VOLUME:/data" -p "$PORT:8080" \
        --entrypoint /nibe-collector "$IMAGE" >/dev/null
    container exec -d --env-file "$TMP/mcp.env" "$NAME" /nibe-mcp
    echo "collector running in '$NAME'; MCP on http://$PORT/mcp"
    ;;
down)
    if exists; then
        container stop "$NAME" >/dev/null 2>&1 || true
        container rm "$NAME" >/dev/null
    fi
    echo "stopped '$NAME' (volume '$VOLUME' kept)"
    ;;
logs)
    container logs "$NAME"
    ;;
*)
    echo "usage: $0 up|down|logs" >&2
    exit 2
    ;;
esac
