#!/bin/sh
# Runs nibe-hub locally with Apple Containers (the `container` CLI).
#
# Collector and MCP server run in ONE container: every Apple container is its own VM, and the
# SQLite WAL database can only be shared by processes on the same kernel.
#
#   tools/run_local.sh up      # start (or restart) collector + MCP server
#   tools/run_local.sh down    # stop and remove the container (the volume keeps the history)
#   tools/run_local.sh logs    # collector logs (the MCP server's output is shown by `up` if it fails to start)
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
running() { container list | awk 'NR > 1 { print $1 }' | grep -qx "$NAME"; }

# Each process gets only its own settings from $ENV_FILE: the collector never sees MCP_TOKEN
# and the MCP server never sees the pump password.
COLLECTOR_VARS='^(NIBE_[A-Z_]+|DB_PATH|RUST_LOG)='
MCP_VARS='^(MCP_[A-Z_]+|DB_PATH|TZ_NAME|RUST_LOG)='

split_env() {
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    # strip Windows line endings: a trailing \r would end up in every value
    tr -d '\r' < "$ENV_FILE" > "$TMP/all.env"
    grep -E "$COLLECTOR_VARS" "$TMP/all.env" > "$TMP/collector.env" || true
    grep -E "$MCP_VARS" "$TMP/all.env" > "$TMP/mcp.env" || true
}

# value of KEY in an env file (last definition wins), or empty
value_of() { grep -E "^$1=" "$2" | tail -n 1 | cut -d= -f2-; }

healthy() {
    i=0
    while [ $i -lt 20 ]; do
        curl -sf "http://$PORT/healthz" >/dev/null 2>&1 && return 0
        sleep 0.5
        i=$((i + 1))
    done
    return 1
}

case "${1:-}" in
up)
    [ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE (see README)" >&2; exit 1; }
    split_env
    # fail here, not silently inside the container: the MCP server refuses to start without a token
    if [ -z "$(value_of MCP_TOKEN "$TMP/mcp.env")" ] && [ "$(value_of MCP_ALLOW_NO_AUTH "$TMP/mcp.env")" != "true" ]; then
        echo "MCP_TOKEN is empty in $ENV_FILE; generate one with: openssl rand -hex 32" >&2
        exit 1
    fi
    container volume inspect "$VOLUME" >/dev/null 2>&1 || container volume create "$VOLUME" >/dev/null
    if exists; then
        # recreate, so changes to $ENV_FILE take effect
        container stop "$NAME" >/dev/null 2>&1 || true
        container rm "$NAME" >/dev/null
    fi
    container run -d --name "$NAME" --env-file "$TMP/collector.env" -v "$VOLUME:/data" -p "$PORT:8080" \
        --entrypoint /nibe-collector "$IMAGE" >/dev/null
    if ! container exec -d --env-file "$TMP/mcp.env" "$NAME" /nibe-mcp 2>/dev/null; then
        echo "the collector stopped right after starting; its log:" >&2
        container logs "$NAME" >&2 || true
        exit 1
    fi
    if ! healthy; then
        if ! running; then
            echo "the collector stopped right after starting; its log:" >&2
            container logs "$NAME" >&2 || true
            exit 1
        fi
        # the detached MCP process has no visible log; run it once in the foreground to show why it fails
        echo "the MCP server did not come up on $PORT; its output:" >&2
        container exec --env-file "$TMP/mcp.env" "$NAME" /nibe-mcp >&2 &
        pid=$!
        sleep 5
        kill "$pid" 2>/dev/null || true
        exit 1
    fi
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
