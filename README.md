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

Create a `.env` file (do not commit it). For `NIBE_TLS_FINGERPRINT`, see [the pump's TLS certificate](#the-pumps-tls-certificate):

```sh
NIBE_URL=https://192.168.1.50:8443
NIBE_USER=...
NIBE_PASS=...
NIBE_TLS_FINGERPRINT=<SHA-256 fingerprint of the pump's certificate>
MCP_TOKEN=<openssl rand -hex 32>
```

Then start both processes with [tools/run_local.sh](tools/run_local.sh):

```sh
tools/run_local.sh up      # collector + MCP server in container "nibe", MCP on 127.0.0.1:8080
tools/run_local.sh logs    # collector logs
tools/run_local.sh down    # stop and remove the container; the nibe-data volume keeps the history
```

The script splits `.env` so each process gets only its own settings: the collector gets the `NIBE_*` variables, and the MCP server gets the `MCP_*` variables and `TZ_NAME`. Then it runs:

```sh
container volume create nibe-data
container run -d --name nibe --env-file <collector settings> -v nibe-data:/data -p 127.0.0.1:8080:8080 \
    --entrypoint /nibe-collector nibe-hub:latest
container exec -d --env-file <MCP settings> nibe /nibe-mcp
```

Both processes run as root in the same VM, so this split keeps secrets out of each other's environment but is not a hard boundary. The MCP port is published on 127.0.0.1 only, so the token never crosses the network.

`up` refuses to start without `MCP_TOKEN`. If the collector or the MCP server fails to start, `up` prints its error and exits with a non-zero status. Windows line endings in `.env` are stripped; a comment after a value is not (it becomes part of the value).

The MCP process does not restart on its own. After a reboot, or after the container stops, run `tools/run_local.sh up` again. Check the server with `curl http://127.0.0.1:8080/healthz`.

### Testing without a pump

`tools/mock_nibe.py` serves `testdata/points.json.gz` (a real dump from this installation) over HTTPS with basic auth, and makes a few values move. It creates a new self-signed certificate on every start and prints its fingerprint:

```sh
python3 tools/mock_nibe.py --port 8443 --user nibe --password secret
```

Containers reach the Mac through the gateway of the default container network, usually `192.168.64.1` (check with `container network list`). In `.env`, set `NIBE_URL=https://192.168.64.1:8443`, `NIBE_USER=nibe`, `NIBE_PASS=secret` and the `NIBE_TLS_FINGERPRINT=...` line the mock printed. Then start the container as above and query the MCP server:

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
/container/mounts/add name=nibe-tls src=/usb1/nibe-tls dst=/tls

# one envlist per container: the network-facing MCP container never gets the pump password
/container/envs/add name=nibe-collector key=NIBE_URL value="https://192.168.1.50:8443"
/container/envs/add name=nibe-collector key=NIBE_USER value="..."
/container/envs/add name=nibe-collector key=NIBE_PASS value="..."
/container/envs/add name=nibe-collector key=NIBE_TLS_FINGERPRINT value="<pump certificate fingerprint>"
/container/envs/add name=nibe-mcp key=MCP_TOKEN value="<openssl rand -hex 32>"
/container/envs/add name=nibe-mcp key=MCP_TLS_CERT value="/tls/cert.pem"
/container/envs/add name=nibe-mcp key=MCP_TLS_KEY value="/tls/key.pem"

# upload nibe-hub-arm64.tar to /usb1, and cert.pem and key.pem (see below) to /usb1/nibe-tls first
/container/add file=usb1/nibe-hub-arm64.tar interface=veth-collector envlist=nibe-collector mounts=nibe-data \
    root-dir=usb1/containers/collector entrypoint=/nibe-collector start-on-boot=yes logging=yes
/container/add file=usb1/nibe-hub-arm64.tar interface=veth-mcp envlist=nibe-mcp mounts=nibe-data,nibe-tls \
    root-dir=usb1/containers/mcp entrypoint=/nibe-mcp start-on-boot=yes logging=yes
/container/start 0
/container/start 1

# make MCP reachable only from the machine that runs Claude (adjust the address and your forward rules)
/ip/firewall/nat/add chain=dstnat protocol=tcp dst-port=8080 in-interface-list=LAN src-address=<your-mac-ip> \
    action=dst-nat to-addresses=172.17.0.3 to-ports=8080
```

Unlike Apple Containers, RouterOS containers share the router's kernel, so two separate containers can share `/data` and SQLite WAL works. Use an SSD or a good USB stick: the database is expected to grow by a few MB per day.

### HTTPS for the MCP server

The bearer token gives permanent read access, so don't send it over plain HTTP across the LAN. With `MCP_TLS_CERT` and `MCP_TLS_KEY` set, the server speaks HTTPS. Create a self-signed certificate for the router's address on your Mac:

```sh
openssl ecparam -genkey -name prime256v1 -noout -out key.pem
openssl req -x509 -new -key key.pem -days 3650 -out cert.pem \
    -subj "/CN=nibe-hub" -addext "subjectAltName=IP:<router-ip>"
```

(Generate the key with `ecparam` as shown: with the LibreSSL `openssl` that ships with macOS, `req -newkey ec` writes a key the server cannot load.)

Upload both files to `/usb1/nibe-tls` on the router and keep `cert.pem` on the Mac for Claude Desktop. Without these variables the server falls back to plain HTTP and logs a warning when it listens on a non-loopback address.

## RB5009 as a MikroTik App

An alternative to the `/container` setup above (RouterOS 7.26 or later with the container package). [mikrotik/nibe-hub.yaml](mikrotik/nibe-hub.yaml) defines one app with two services, `collector` and `mcp`, sharing a `data` volume. The app gets its own VETH on your LAN bridge, so the MCP server listens on port 443 of that VETH's own address, next to the router's own HTTPS. No port forwarding is needed.

### How the app is networked

The YAML attaches the app to an existing bridge instead of an app-created network:

```yaml
networks:
  default:
    name: lan
    external: true
```

- **`external: true`** means: use an existing bridge. Without it, RouterOS puts the app on its own `internal` network (`172.18.0.1/24`) behind NAT with a masquerade rule, and only ports listed under `ports` would be reachable, through dst-nat rules on the router's own address.
- **`lan`** is not something you create: it is an alias for the bridge set as `lan-bridge` in `/app/settings`. When that is empty, RouterOS uses the bridge it detected itself, shown as the read-only `assumed-lan-bridge`. Check with `/app/settings/print`.
- **Another bridge:** external networks also accept a bridge name, so `name: <bridge-name>` attaches the app to that bridge regardless of `lan-bridge`. Use this when each of your networks has its own bridge.
- **One VETH for the app:** RouterOS creates a single `veth-app-<app>` per app, so both services most likely share one address. Only the MCP server listens on it.

### Running the app on a VLAN

With VLANs on one bridge (`vlan-filtering=yes`), the bridge is the same for every app, and the VLAN is chosen with the app property **`network-pvid`**. It sets the PVID of the app's bridge port (default `1`), which makes the VETH an untagged access port on that VLAN. It is an app property, not a YAML field: set it after `/app/add` and before `/app/enable` (new apps are created disabled, and their configuration can be edited until they are enabled). See step 4 below.

For that VLAN:

- **Bridge VLAN table:** the VLAN must be in `/interface/bridge/vlan` with the bridge itself as a tagged member, as for any VLAN the router routes. RouterOS adds the app's port as untagged itself, based on its PVID.
- **DHCP:** the VETH's DHCP request arrives on that VLAN, so the DHCP server on the VLAN interface hands out the address, and that is where you make the lease static (step 5). The MCP certificate must be for an address in that VLAN's subnet.
- **Firewall:** allow forwarding from the machine that runs Claude to the app's address on TCP 443. The collector needs TCP 8443 to the pump; if the pump is on another VLAN, allow that too, only from the app's address.
- **Outgoing access:** leave `network-outgoing-access` at `yes` (the default). `no` blocks all outgoing connections of the app with a mangle drop rule, which also cuts the collector off from the pump; restrict traffic with your own forward rules instead.

### Steps

1. **Get the image into a registry.** Apps pull images from a registry and cannot import a tar file. The GitHub Actions workflow [.github/workflows/image.yml](.github/workflows/image.yml) does this:

   - on every push to `main` it runs the tests, builds the arm64 image on a native arm64 runner and pushes `ghcr.io/kkerssem/nibe-hub:<commit>` (the 7-character short sha) and `:latest`;
   - a `v1.2.3` git tag also gets a `1.2.3` image tag;
   - pull requests are tested and built but not pushed;
   - it can be started by hand under Actions → Image → Run workflow.

   **Once, after the first push:** the new package is private, like the repository, and Apps document no registry login. Make the package public on GitHub (your profile → Packages → `nibe-hub` → Package settings → Change visibility). It contains only the two binaries and CA certificates, no configuration or secrets.

   For a manual push from the Mac, or to another registry, use `tools/push_image.sh` (after `container registry login ghcr.io` with a token that has the `write:packages` scope; set `IMAGE_REPO` for another registry).

2. **Pick the app's address.** The app's VETH gets its address from the DHCP server of the network (or VLAN) it is attached to. Choose a free address there and create the MCP certificate for it (as in [HTTPS for the MCP server](#https-for-the-mcp-server), with `subjectAltName=IP:<app-ip>`).

3. **Choose how the settings get into the app.** [mikrotik/nibe-hub.yaml](mikrotik/nibe-hub.yaml) is a complete app definition without secrets: the pump URL, user, password, fingerprint and MCP token are empty, and the two TLS configs hold a placeholder line. Either:

   - **add it as it is** and fill in those values on the router before enabling the app. Until they are set, the services stop at startup with a message naming what is missing; or
   - **generate a filled-in copy** from your `.env` and the certificate:

     ```sh
     python3 tools/make_app_yaml.py --cert cert.pem --key key.pem
     ```

     This writes `mikrotik/nibe-hub-local.yaml` (mode 0600, git-ignored). It contains the pump password, the MCP token and the TLS key, because RouterOS generates the values of app `secrets` itself and the documentation describes no way to supply your own. Each service gets only its own settings, as with `run_local.sh`, and only the optional settings that are in `.env`. Add `--image ghcr.io/kkerssem/nibe-hub:<short-sha>` to pin a version instead of `latest`; each workflow run lists its tags in its summary.

4. **Add the app on the router.** Upload the file (`nibe-hub.yaml` or `nibe-hub-local.yaml`), then:

   ```routeros
   /app/settings/set disk=usb1 lan-bridge=<your-bridge>
   /app/add yaml=[/file/get nibe-hub-local.yaml contents]
   # only for a VLAN on a vlan-filtering bridge; leave out for the untagged LAN (PVID 1)
   /app/set nibe-hub network-pvid=<vlan-id>
   # with the file without secrets: fill in the empty values and the TLS configs here
   /app/enable nibe-hub
   /file/remove nibe-hub-local.yaml
   ```

   After enabling, `/interface/bridge/port/print where interface~"veth-app-nibe-hub"` should show the bridge and PVID you expect.

5. **Make the address static.** Find the app's VETH and its lease, and make the lease static. The lease must match the router-side `mac-address`, not `container-mac-address`:

   ```routeros
   /interface/veth/print detail where name~"veth-app-nibe-hub"
   /ip/dhcp-server/lease/print where host-name~"veth-app-nibe-hub"
   /ip/dhcp-server/lease/make-static [find where host-name~"veth-app-nibe-hub"]
   ```

   If the lease didn't get the address in your certificate, set it on the static lease (`address=<app-ip>`) and restart the app.

### Why the generated file contains the secrets

App-level `secrets` take only a name (`mcp-token:`); RouterOS generates the value itself when the app runs and stores it under `<disk>/apps/<app>/secret-<name>`. Services reference it as `[secret:name]` in `environment` or read it from `/run/secrets/<name>`. The documentation describes no way to set or override the value, so:

- the **pump password** has to be the one set on the pump and is written into the file;
- the **TLS key** has to match your certificate and is written into the file;
- the **MCP token** could be generated (`secrets: {mcp-token:}` and `MCP_TOKEN: "[secret:mcp-token]"`), but you need it on your Mac for Claude Desktop, and the documentation doesn't say how to read a generated value back. If `/app/print detail` (the read-only `variables-to-use-in-environment`) or the file `apps/nibe-hub/secret-mcp-token` turns out to show it, that would keep the token out of the file.

Because RouterOS replaces `[name]` placeholders in environment values, `make_app_yaml.py` warns about a value that contains `[`.

### Not yet tried on a real RB5009

Check these before relying on it:

- that the app's VETH has `dhcp=yes` (the documentation only says that apps create their VETHs automatically)
- that its `mac-address` stays the same after `/app/restart`, `/app/disable` and `/app/enable`; if it changes, the static lease no longer matches
- that both services share one VETH, as the single `veth-app-<app>` name suggests
- that `/app/set nibe-hub network-pvid=<vlan-id>` is accepted before enabling, and that the bridge port gets that PVID
- that `command: /nibe-mcp` replaces the image's default command (`/nibe-collector`)
- which parts of an added app can be edited on the router before enabling it: the environment values, and the contents of the `mcp-tls-cert` and `mcp-tls-key` configs (needed when adding the file without secrets)
- that empty environment values (`NIBE_PASS: ""`) are accepted by `/app/add`
- that the `healthcheck` of both services is accepted, and what RouterOS does when one fails: only show it, or restart the service

## The pump's TLS certificate

The pump uses a self-signed certificate. Instead of turning verification off, pin it: the collector then accepts only that exact certificate, so a device that takes over the pump's IP address never gets the pump password. Read the fingerprint from the pump once:

```sh
openssl s_client -connect 192.168.1.50:8443 </dev/null 2>/dev/null | openssl x509 -noout -fingerprint -sha256 -subject
```

Put the value after `Fingerprint=` in `NIBE_TLS_FINGERPRINT`; colons and case don't matter.

This reading itself trusts whatever answers at the pump's address at that moment, so make it count:

- Read it over a network you trust, ideally from a machine on the same switch or Wi-Fi as the pump, not over a VPN or guest network.
- Read it a second time from another device (for example a laptop on a different connection, or a day later) and check that both values match.
- Check the subject: the pump's certificate shows `O=NIBE` and a numeric `CN`.

If the pump gets a new certificate (for example after a firmware update, a factory reset or when it expires), polls fail with a fingerprint mismatch in the log. Read and check the new value the same way before you update `NIBE_TLS_FINGERPRINT`.

An empty `NIBE_TLS_FINGERPRINT=` line is an error rather than "not set", so a template value left unfilled is caught at startup. Without the line, the collector verifies the certificate against the system CAs, which only works for a pump behind a proxy with a CA-signed certificate; it warns at startup when `NIBE_URL` is an IP address in that mode.

`NIBE_INSECURE_TLS=true` still turns verification off completely, for a quick test.

## Claude Desktop

```json
"nibe": {
  "command": "/opt/homebrew/bin/npx",
  "args": ["mcp-remote", "https://<router-ip>:8080/mcp",
           "--header", "Authorization: Bearer <MCP_TOKEN>"],
  "env": { "NODE_EXTRA_CA_CERTS": "/path/to/cert.pem" }
}
```

`NODE_EXTRA_CA_CERTS` makes `mcp-remote` trust the self-signed certificate. For the local Apple container, use `http://127.0.0.1:8080/mcp` with `--allow-http` and no `env`; that traffic never leaves the Mac.

A self-refreshing dashboard artifact needs HTTPS on port 443 with a publicly trusted certificate (a reverse proxy or tunnel); that is not built in here.

## Configuration

| variable | default | |
|---|---|---|
| `NIBE_URL`, `NIBE_USER`, `NIBE_PASS` | | required (collector) |
| `NIBE_POLL_SECS` | 60 | minimum 10 |
| `NIBE_FAST_IDS`, `NIBE_FAST_SECS` | empty, 10 | fast poll per point via `/points/<id>`; ids the pump doesn't report are skipped with a warning |
| `NIBE_TLS_FINGERPRINT` | | SHA-256 of the pump's certificate; an empty value is an error; without the variable the certificate is checked against the system CAs, which fails for the pump |
| `NIBE_INSECURE_TLS` | false | `true` turns certificate verification off; not together with `NIBE_TLS_FINGERPRINT` |
| `DB_PATH` | /data/nibe.db | |
| `RUST_LOG` | info | log level, e.g. `debug` (logs every poll) |
| `LOG_TIMESTAMPS` | true | `false` leaves the timestamp out of each log line, for a runtime that adds its own (the app sets it for RouterOS) |
| `NO_COLOR` | | any value turns log colours off; they are only used when the output is a terminal anyway |
| `MCP_TOKEN` | | required, or `MCP_ALLOW_NO_AUTH=true` |
| `MCP_TLS_CERT`, `MCP_TLS_KEY` | | PEM files; both set means HTTPS |
| `MCP_BIND`, `MCP_ALLOWED_HOSTS`, `TZ_NAME` | 0.0.0.0:8080, off once a token is set, Europe/Amsterdam | |

Both binaries stop cleanly on SIGTERM (what `container stop` and RouterOS send) as well as Ctrl-C.

### Health checks

The image has no shell, `wget` or `curl`, so each binary has a `health` subcommand for a container health check. It prints one line and exits with 0 (healthy) or 1:

- `/nibe-collector health` reads the database (`DB_PATH`) and is healthy while the last successful full poll is at most three poll intervals plus 30 s old, the same rule as `healthy` in the MCP `status` tool. It catches an unreachable pump, a wrong password or a changed pump certificate, not only a stopped process. When unhealthy it also prints the last poll error. A last poll in the future (the clock went back, for example after a reboot before NTP syncs) counts as unhealthy.
- `/nibe-mcp health` sends `GET /healthz` to the port in `MCP_BIND` (localhost for `0.0.0.0`; `::1`, then `127.0.0.1` for `[::]`), over HTTPS when `MCP_TLS_CERT` is set, and is healthy when the server answers `ok`. It doesn't check the certificate and needs no token.

Any other argument is ignored with a warning, so a runtime that appends the image's `CMD` after an `entrypoint=` still starts the service normally.

[mikrotik/nibe-hub.yaml](mikrotik/nibe-hub.yaml) uses both as `healthcheck` (`test: ["CMD", "/nibe-collector", "health"]`). Locally: `container exec nibe /nibe-collector health`. The `/healthz` endpoint of the MCP server stays for checks from outside.

## Still to verify on the real pump

1. **Heat from the energy log.** `cop` takes heat from the energy-log counters 27378–27383. To validate them, compare `heat_kwh` over a few hours with the heat from air flow × temperature difference (points 5205, 22 and 23), and check `cop_unit` against your expectations for the season. Points 28392/28393 ("Tot. production/consumption") are only reported as `lifetime_*_delta_kwh`; their meaning is not validated: after a day, compare `lifetime_consumed_delta_kwh` with `elec_unit_kwh` in a `1d` bucket. Power points 1801 and 22130 are new compared to the Loxone route.
2. ~~**Fast poll**~~ (checked on 2026-11-15): `/points/<id>` returns a single point object, the same shape as an entry of `/points`, in about 15 ms; an unknown id gives HTTP 404 with `{"error": "Point not found"}`. `NIBE_FAST_IDS` can be enabled. After the first full poll the collector checks the ids against the pump's catalogue and skips unknown ones with a warning; during fast polling a failing id is skipped for that round without losing the other values.
3. **Certificate pinning** works against the real pump (a self-signed X.509 v3 EC certificate, valid until 3 October 2027). Check that it stays the same across pump reboots; after it is renewed, update `NIBE_TLS_FINGERPRINT`.
4. The arm64 image builds with Apple Containers and runs locally against the mock, but the image import and the RouterOS commands have not been tried on a real RB5009.
