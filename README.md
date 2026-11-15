# nibe-hub

Collector, SQLite history and read-only MCP server for a NIBE S735C-7 heat pump (local REST API, menu 7.5.15).

```
NIBE :8443 --GET /points every 60 s--> nibe-collector --(changes only)--> nibe.db (SQLite, WAL)
                                                                              ^ read-only
                                                 Claude <--MCP (HTTP, bearer)-- nibe-mcp
```

- **Collector**: the single writer, and it only sends GET requests to the pump. It stores a value only when it changes (around 1000 points, at intervals of 1 s to 1 min). The sensor sentinel -32768 (s16) is stored as NULL. Every successful poll is logged, so a gap in the data is never mistaken for a flat line.
- **MCP server** (rmcp 3.10, streamable HTTP): `status`, `list_points`, `describe_point`, `current`, `series`, `cop`. Aggregation is time-weighted; `coverage` shows how much of each bucket was actually measured.
- Nothing is ever written to the pump, and the MCP server opens the database read-only.

## Building (arm64 image)

Build with [Apple Containers](https://github.com/apple/container) on an Apple Silicon Mac. This is a native build, no emulation:

```sh
container system start
container build --platform linux/arm64 -t nibe-hub:latest .
```

The image contains both binaries, `/nibe-collector` (the default entrypoint) and `/nibe-mcp`.

To deploy it on the RB5009, export the image as a tar archive:

```sh
container image save --platform linux/arm64 -o nibe-hub-arm64.tar nibe-hub:latest
```

This writes an OCI archive (`oci-layout`, `index.json`, `blobs/`) without the Docker-style `manifest.json`. RouterOS documents importing `docker save` tarballs; whether it accepts a pure OCI archive is not verified. If the import fails, convert the archive with [skopeo](https://github.com/containers/skopeo) (`brew install skopeo`):

```sh
skopeo copy oci-archive:nibe-hub-arm64.tar docker-archive:nibe-hub-arm64-docker.tar:nibe-hub:latest
```

## Running locally with Apple Containers

Every Apple container runs in its own lightweight VM. The collector and the MCP server share one SQLite database in WAL mode, which needs both processes on the same kernel. So run them **in the same container**: start the collector as the main process and add the MCP server to it with `container exec`.

Create a `.env` file (do not commit it):

```sh
NIBE_URL=https://192.168.1.50:8443
NIBE_USER=...
NIBE_PASS=...
MCP_TOKEN=<openssl rand -hex 32>
```

Then start both processes with [tools/run_local.sh](tools/run_local.sh):

```sh
tools/run_local.sh up      # collector + MCP server in container "nibe", MCP on 127.0.0.1:8080
tools/run_local.sh logs    # collector logs
tools/run_local.sh down    # stop and remove the container; the nibe-data volume keeps the history
```

The script runs these commands:

```sh
container volume create nibe-data
container run -d --name nibe --env-file .env -v nibe-data:/data -p 127.0.0.1:8080:8080 \
    --entrypoint /nibe-collector nibe-hub:latest
container exec -d --env-file .env nibe /nibe-mcp
```

The MCP process does not restart on its own. After a reboot, or after the container stops, run `tools/run_local.sh up` again. Check the server with `curl http://127.0.0.1:8080/healthz`.

### Testing without a pump

`tools/mock_nibe.py` serves `testdata/points.json.gz` (a real dump from this installation) over HTTPS with basic auth, and makes a few values move:

```sh
python3 tools/mock_nibe.py --port 8443 --user nibe --password secret
```

Containers reach the Mac through the gateway of the default container network, usually `192.168.64.1` (check with `container network list`). In `.env`, set `NIBE_URL=https://192.168.64.1:8443`, `NIBE_USER=nibe` and `NIBE_PASS=secret`, then start the container as above and query the MCP server:

```sh
python3 tools/mcp_client.py http://127.0.0.1:8080/mcp <MCP_TOKEN> call status '{}'
```

## RB5009 (RouterOS 7, container package)

The internal flash is too small and wears out; use a USB SSD (ext4). Check the syntax against your RouterOS version.

```routeros
/system/device-mode/update container=yes      # confirm with the reset button or a power cycle
/disk/format-drive usb1 file-system=ext4      # find the name with /disk/print
/interface/bridge/add name=containers
/ip/address/add address=172.17.0.1/24 interface=containers
/interface/veth/add name=veth-collector address=172.17.0.2/24 gateway=172.17.0.1
/interface/veth/add name=veth-mcp address=172.17.0.3/24 gateway=172.17.0.1
/interface/bridge/port/add bridge=containers interface=veth-collector
/interface/bridge/port/add bridge=containers interface=veth-mcp
/ip/firewall/nat/add chain=srcnat action=masquerade src-address=172.17.0.0/24

/container/mounts/add name=nibe-data src=/usb1/nibe dst=/data
/container/envs/add name=nibe key=NIBE_URL value="https://192.168.1.50:8443"
/container/envs/add name=nibe key=NIBE_USER value="..."
/container/envs/add name=nibe key=NIBE_PASS value="..."
/container/envs/add name=nibe key=MCP_TOKEN value="<openssl rand -hex 32>"

# upload nibe-hub-arm64.tar to /usb1 first
/container/add file=usb1/nibe-hub-arm64.tar interface=veth-collector envlist=nibe mounts=nibe-data \
    root-dir=usb1/containers/collector entrypoint=/nibe-collector start-on-boot=yes logging=yes
/container/add file=usb1/nibe-hub-arm64.tar interface=veth-mcp envlist=nibe mounts=nibe-data \
    root-dir=usb1/containers/mcp entrypoint=/nibe-mcp start-on-boot=yes logging=yes
/container/start 0
/container/start 1

# make MCP reachable from the LAN (adjust in-interface-list and your forward rules)
/ip/firewall/nat/add chain=dstnat protocol=tcp dst-port=8080 in-interface-list=LAN action=dst-nat to-addresses=172.17.0.3 to-ports=8080
```

Unlike Apple Containers, RouterOS containers share the router's kernel, so two separate containers can share `/data` and SQLite WAL works. Use an SSD or a good USB stick: the database is expected to grow by a few MB per day.

## Claude Desktop

```json
"nibe": {
  "command": "/opt/homebrew/bin/npx",
  "args": ["mcp-remote", "http://<router-ip>:8080/mcp", "--allow-http",
           "--header", "Authorization: Bearer <MCP_TOKEN>"]
}
```

For the local Apple container, use `http://127.0.0.1:8080/mcp`.

A self-refreshing dashboard artifact needs HTTPS on port 443 (a reverse proxy or tunnel); that is not built in here.

## Configuration

| variable | default | |
|---|---|---|
| `NIBE_URL`, `NIBE_USER`, `NIBE_PASS` | | required (collector) |
| `NIBE_POLL_SECS` | 60 | minimum 10 |
| `NIBE_FAST_IDS`, `NIBE_FAST_SECS` | empty, 10 | fast poll per point via `/points/<id>` |
| `NIBE_INSECURE_TLS` | true | the pump has a self-signed certificate |
| `DB_PATH` | /data/nibe.db | |
| `MCP_TOKEN` | | required, or `MCP_ALLOW_NO_AUTH=true` |
| `MCP_BIND`, `MCP_ALLOWED_HOSTS`, `TZ_NAME` | 0.0.0.0:8080, off once a token is set, Europe/Amsterdam | |

## Still to verify on the real pump

1. **Meaning of points 28392/28393** ("Tot. production/consumption"). After a day, run `cop` with bucket `1d`; if `counter_consumed_kwh` ≈ `elec_unit_kwh` (the integral of point 22130), the interpretation is correct. Power points 1801 and 22130 are new compared to the Loxone route.
2. **Fast poll**: the response of `/points/<id>` is undocumented; test `curl -k -u user:pass https://IP:8443/api/v1/devices/0/points/1801` before enabling `NIBE_FAST_IDS`.
3. The arm64 image builds with Apple Containers and runs locally against the mock, but the image import and the RouterOS commands have not been tried on a real RB5009.
