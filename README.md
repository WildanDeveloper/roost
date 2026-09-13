<div align="center">

# roost

**A Pterodactyl Wings-compatible game server daemon — rewritten in Rust.**

Drop-in replacement for the [Wings](https://github.com/pterodactyl/wings) daemon.
The Pterodactyl panel manages containers on this node exactly as it would with
the official daemon — no panel modifications required.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![Wings API](https://img.shields.io/badge/wings%20API-v1.13.3-green.svg)](https://github.com/pterodactyl/wings)
[![Conformance](https://img.shields.io/badge/conformance-38%2F38%20passed-brightgreen.svg)](#testing)

</div>

---

## Why roost?

| | Wings | roost |
|---|---|---|
| Language | Go (~24.8k LOC) | Rust (~12k LOC) |
| Panel compatibility | official | API-compatible, drop-in |
| Memory safety | GC | no `null`, no data races, no panics on the hot path |
| File operations | path checks | `O_NOFOLLOW` + canonicalized confinement — the bug class behind Wings CVE-1.12.2 is prevented by construction |
| Error responses | stacktraces | typed `AppError` enum, 5xx internals never leak to clients |
| Config secrets | plain file | `$ENV` / `file://` indirection, 0600 on panel pushes |

- **Smaller audit surface** — roughly half the code for the same feature set
- **Hardened by default** — symlinks, SSRF, JWT revocation and path traversal
  are handled defensively (see [Security](#security))
- **Verified parity** — a 38-check conformance suite drives the full HTTP,
  WebSocket and SFTP surface against a mock or live panel

## Feature matrix

| Subsystem | Details |
|---|---|
| HTTP API | full Wings v1.13.3 surface: system info, servers, power/console, file manager, backups, transfers, downloads/uploads |
| Authentication | constant-time token compare, HS256 JWTs (`sub`/`scope`/`server_uuid`), panel-driven revocation, boot-time denylist |
| Containers | Docker via [bollard]: create/start/stop/kill, cgroup burst, CPU/memory/swap/IO limits, outgoing-IP SNAT, `machine-id` mounts, registry auth |
| Console | live WebSocket stream, per-server ring buffer, line throttling, `done`-line startup detection with `regex:` matchers and ANSI stripping |
| File manager | read/write, rename, copy, chmod, compress/extract (`tar`, `tar.{gz,bz2,xz,zst,lz4}`, `zip`, `7z`), gitignore-style denylists, SHA-1 checksums |
| Backups | local + S3 adapters, `.pteroignore` support, throttled writes, restore with truncation, panel status callbacks |
| Transfers | outgoing push (multipart + checksum, live progress events) and incoming receive with disk-limit verification and rollback |
| SFTP | [russh] server, panel-delegated auth (password + public key), per-operation permission checks, session tracking/cancellation |
| Activity | SQLite-backed audit log (WAL), wings `activityCron` + per-minute `sftpCron` merge, never drops events |
| Crash detection | OOM detection, clean-exit policy, restart cooldown, intentional-stop tracking |
| Persistence | `states.json` boot restore, activity database survives restarts |
| Remote downloads | SSRF guard (private/loopback/CGNAT/ULA refused), per-server concurrency cap, disk-limit enforcement |

## Installation

### 0. Panel + roost in one shot (recommended)

The combined installer installs the Pterodactyl panel **and** roost on the
same machine, then configures everything automatically: it creates the
location and the node in the panel, generates
`/etc/pterodactyl/config.yml` straight from the panel and starts the daemon.

```bash
sudo bash <(curl -s https://raw.githubusercontent.com/WildanDeveloper/roost/master/pterodactyl-roost-installer.sh)
```

You will be asked (defaults in brackets):

| Prompt | Default |
|---|---|
| Database name | `panel` |
| Database username | `pterodactyl` |
| MySQL password | press enter → generated |
| Timezone | `Asia/Jakarta` |
| Email (panel + admin account) | — |
| Admin username / first / last name | `admin` / `Admin` / `User` |
| Admin password | press enter → generated (shown in `/root/pterodactyl-install-info.txt`) |
| Panel FQDN (domain or IP) | your public IP |
| Configure UFW firewall? (y/N) | `n` |
| Let's Encrypt for the panel? (y/N) | `n` (needs a domain with DNS pointing at this machine) |
| Assume SSL? (y/N) | `n` |
| Telemetry | `yes` |
| Node FQDN | the panel FQDN (Enter) |
| Let's Encrypt for the node? (y/N) | `n` (reuses the panel certificate automatically) |

When it finishes the node shows **green** in the panel
(**Admin → Nodes**), and the summary is saved to
`/root/pterodactyl-install-info.txt`. Everything works exactly like the
official panel + Wings stack.

### 1. Install roost on an existing panel node (one-line)

Installs Docker (if missing), the release binary, the systemd service and
walks you through registering the node:

```bash
sudo bash <(curl -s https://raw.githubusercontent.com/WildanDeveloper/roost/master/install.sh)
```

Headless (no prompts):

```bash
sudo ROOST_PANEL_URL=https://panel.example.com \
     ROOST_PANEL_TOKEN=<application-api-key> \
     ROOST_NODE_ID=1 \
     ROOST_AUTO_START=true \
     bash <(curl -s https://raw.githubusercontent.com/WildanDeveloper/roost/master/install.sh)
```

If Wings is already installed on the node, the installer stops with
instructions — both daemons cannot share a machine (same Docker containers,
data directories and ports 8080/2022).

### 2. Manual install

Download the binary from [releases](https://github.com/WildanDeveloper/roost/releases/latest)
(amd64/arm64, SHA-256 verified by the installer) or build from source:

```bash
git clone https://github.com/WildanDeveloper/roost.git
cd roost
cargo build --release
```

Register the node and run:

```bash
# writes /etc/pterodactyl/config.yml from a panel application API key
sudo ./target/release/roost configure \
    --panel-url https://panel.example.com \
    --token <application-api-key> \
    --node 1

# run the daemon (Ctrl+C to stop)
sudo ./target/release/roost
```

Then install the systemd service (or copy it from `install.sh`):

```bash
sudo systemctl enable --now roost
```

### 3. Domain + SSL on the node

Identical to Wings — the daemon reads the panel-generated `api.ssl` block:

1. Point `node.example.com` (A record) at the node
2. Obtain a certificate: `sudo certbot certonly --standalone -d node.example.com`
3. Panel → **Admin → Nodes → edit node** → FQDN `node.example.com`,
   scheme **HTTPS**
4. `sudo systemctl restart roost` — the API now serves
   `https://node.example.com:8080` (API + `wss://` console over the same TLS;
   SFTP stays on SSH/2022)

The combined installer and `install.sh` do this for you when you answer
`y` to the Let's Encrypt prompts.

### Managing the service

```bash
systemctl {start,stop,restart,status} roost
journalctl -u roost -f          # live logs
roost diagnostics               # sanitized debugging report (no tokens)
```

Requires a working **Docker daemon** at runtime; roost refuses to boot
without one (the systemd unit depends on `docker.service`).

### CLI

```
roost                       run the daemon (default)
roost configure [...]       fetch node config from the panel and write config.yml
roost diagnostics [...]     sanitized debugging report (never includes tokens)
roost version               print the version
```

## Configuration

`roost` reads the exact `config.yml` the panel generates
(**Nodes → your node → Configuration**) and falls back to bundled defaults.
Token values support indirection, matching Wings:

```yaml
token_id: "xyzabc123"
token: "$DAEMON_TOKEN"          # or file:///etc/pterodactyl/.token
```

| Section | Purpose |
|---|---|
| `api` | bind host/port, SSL cert/key, upload limits, trusted proxies |
| `system` | data/log/archive/backup/tmp directories, SFTP, activity, crash detection |
| `docker` | network, registries, installer limits, CPU burst/overhead |
| `remote` | panel base URL, query tuning |
| `throttles` | console output rate limiting |

`WINGS_TOKEN` / `WINGS_TOKEN_ID` environment overrides work as in Wings.

## Architecture

```
main.rs               entrypoint: subcommands, config, docker, TLS, serve
cli.rs                configure + diagnostics subcommands
config.rs             Wings-compatible config.yml + $ENV/file:// token resolution
auth.rs               constant-time bearer-token middleware
jwt/                  HS256 claims validation + panel revocation store
parser.rs             egg file parsers (file/yaml/json/ini/xml/properties)
server/               per-server core: lifecycle, console, events, files,
                      install, activity, config rewriting, manager
docker/               bollard wrapper: containers, networks, cgroups, SNAT
remote/               panel client with retry/backoff
router/               HTTP routes, websocket, downloads, middleware
tests/conformance.py  end-to-end suite (mock panel + live-panel mode)
```

## Testing

```bash
# unit tests: egg parser semantics, gitignore, activity store, payload shapes
cargo test

# conformance suite: 38 end-to-end checks — every HTTP route, websocket
# auth/events, real container power cycles, backups, transfers, JWT flows
# and real SFTP sessions (paramiko) against a mock panel
cargo build && python3 tests/conformance.py

# against a live panel instead of the mock
PANEL_URL=https://panel.example.com PANEL_TOKEN=<apikey> NODE_ID=1 \
DAEMON_URL=http://127.0.0.1:8080 DAEMON_TOKEN=<daemon-token> \
    python3 tests/conformance.py
```

The suite doubles as a regression net: every bug found while building it is
now a permanent check.

## Security

- **Path confinement**: canonicalized-root checks *plus* `O_NOFOLLOW` on
  every file open — planted symlinks cannot escape the data directory
- **SSRF guards**: remote downloads and S3 restore URLs are resolved and
  checked against private/loopback/link-local/CGNAT/ULA ranges; redirects
  are re-validated per hop (or refused)
- **JWT hardening**: constant-time comparisons, panel revocation,
  boot-time denylist, per-message revalidation on websockets
- **Secrets**: tokens support `$ENV`/`file://` indirection; panel config
  pushes persist with `0600`; diagnostics never include tokens
- **No information leaks**: internal 5xx details are never returned to clients

## Compatibility notes

roost aims for behavioral parity with Wings 1.13.x, verified by the
conformance suite. A few deliberate differences exist where roost is
stricter (backup during install is refused, invalid backup UUIDs return
422, missing backups return 404). RAR extraction is not supported — there
is no dependable pure-Rust decoder.

## License

[MIT](LICENSE) — same as Wings.

[bollard]: https://github.com/fussybeaver/bollard
[russh]: https://github.com/Eugeny/russh
