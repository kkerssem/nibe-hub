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

case "${1:-}" in
up)
    [ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE (see README)" >&2; exit 1; }
    container volume inspect "$VOLUME" >/dev/null 2>&1 || container volume create "$VOLUME" >/dev/null
    if exists; then
        container stop "$NAME" >/dev/null 2>&1 || true
        container start "$NAME" >/dev/null
    else
        container run -d --name "$NAME" --env-file "$ENV_FILE" -v "$VOLUME:/data" -p "$PORT:8080" \
            --entrypoint /nibe-collector "$IMAGE" >/dev/null
    fi
    container exec -d --env-file "$ENV_FILE" "$NAME" /nibe-mcp
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
