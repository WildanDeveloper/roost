# roost

A Pterodactyl **Wings-compatible** game server daemon written in Rust.

`roost` is a from-scratch implementation of the Pterodactyl Wings daemon. It
speaks the same HTTP API and JWT authentication protocol as Wings, so the
Pterodactyl panel can manage containers on this node exactly like it would
with the official daemon — no panel modifications required.

## Features

- **Panel-compatible HTTP API** (v1.13.3+): system info, server
  power/console, file management, backups, remote downloads
- **JWT authentication** compatible with Wings: `sub`/`scope`/`server_uuid`
  claims, token whitelist/history revocation via the panel
- **Docker backend** via [bollard]:
  container create/start/stop/kill, logs, stats, attach console, resource
  updates, network setup, image pulls with registry auth
- **Live websocket console** with per-server event streams, rate limiting
  and token expiry handling
- **File manager**: list, read/write, rename, copy, delete, chmod,
  compress/extract (tar.gz), directory tree, SHA-1 checksums
- **Backups**: local `wings` adapter (tar.gz + SHA-1, status reported back
  to the panel), install/restore flows
- **Remote downloads** with an SSRF guard (private/loopback/CGNAT ranges
  refused)
- **Wings-compatible config**: reads the panel-generated `config.yml`
  (same schema), `file://`/`$ENV` token indirection, `WINGS_TOKEN`
  overrides, defaults from a bundled example

## Architecture

```
main.rs               entrypoint: config load, dirs, docker, TLS, serve
config.rs             Wings-compatible config.yml load + token resolution
auth.rs               JWT request authentication middleware
jwt/                  token parsing/validation + panel revocation store
state.rs              daemon state and request helpers
error.rs              AppError/AppResult (panel-style JSON errors)
models/               configuration/resource models (Wings-compatible)
docker/               bollard Docker client wrapper + container config
server/               per-server core: state, console, events, files,
                      install, manager (start/stop/restart/kill)
remote/               panel client: servers list, config, install/uploads
router/               HTTP routes: system, servers, files, backups,
                      downloads, middleware, websocket console
```

## Building

Requires Rust (1.75+, MSRV) and a working Docker daemon at runtime.

```bash
cargo build --release
```

The binary is written to `target/release/roost`.

## Configuration

`roost` reads a Wings-compatible `config.yml`. By default it looks at
`/etc/pterodactyl/config.yml`; override with the `ROOST_CONFIG` environment
variable:

```bash
ROOST_CONFIG=./config.yml ./target/release/roost
```

If the file is missing, bundled defaults from `config.example.yml` are used.
The Pterodactyl panel generates a matching file under
**Settings → Nodes → (your node) → Configuration**.

Key sections (same as Wings):

| Section | Purpose |
| --- | --- |
| `api` | bind host/port, SSL cert/key, upload limits, trusted proxies |
| `system` | data/log/archive/backup/tmp directories, SFTP, crash detection |
| `docker` | network, registries, installer limits, CPU/overhead settings |
| `remote` | panel base URL and query tuning |
| `token` / `token_id` | daemon secret; supports `$ENV_VAR` and `file://` |

## Usage

Start the daemon, then add it as a node in the panel as you normally would
(panel → Nodes → create → autoconfiguration). The panel will serve the
`config.yml` for this daemon.

```bash
chmod +x target/release/roost
ROOST_CONFIG=/etc/pterodactyl/config.yml ./target/release/roost
```

### CLI subcommands

Besides running the daemon, the binary ships the same helper subcommands as
wings:

```bash
# Fetch the node configuration from the panel and write config.yml
# (wings `configure`).
roost configure --panel-url https://panel.example.com --token <apikey> \
    --node 1 [--config-path /etc/pterodactyl/config.yml] \
    [--override] [--allow-insecure]

# Collect a debugging report (versions, sanitized config, docker info,
# managed containers, latest logs — never tokens) (wings `diagnostics`).
roost diagnostics [--config-path PATH] [--log-lines N] [--output FILE]
    [--include-endpoints] [--no-logs]

roost version
```

## Tests

Unit tests (egg parser semantics, gitignore, activity store, payload
shapes):

```bash
cargo test
```

Conformance suite: spins up a mock panel implementing the Wings
`/api/remote/*` surface, launches the daemon against it, and drives the
full HTTP + WebSocket + SFTP API end-to-end (including a real container
start/command/stop cycle, backups, transfers, JWT flows and real SSH/SFTP
sessions via paramiko):

```bash
cargo build && python3 tests/conformance.py
```

Against a live panel instead of the mock:

```bash
PANEL_URL=https://panel.example.com PANEL_TOKEN=<apikey> NODE_ID=1 \
DAEMON_URL=http://127.0.0.1:8080 DAEMON_TOKEN=<daemon-token> \
    python3 tests/conformance.py
```

## Status / Roadmap

- [x] API skeleton, auth, system routes
- [x] Server lifecycle (install, start, stop, restart, kill, suspend)
- [x] Console websocket + event streams
- [x] File manager + remote downloads + backups
- [x] SFTP server (russh, panel-delegated auth, permission checks)
- [x] Crash detection (OOM, clean-exit policy, restart cooldown)
- [x] Egg configuration file rewriting (`file`/`yaml`/`json`/`ini`/`xml`/
      `properties` parsers with `{{ config.* }}` templating — wings parser port)
- [x] Startup "done" line detection (`regex:` matchers, strip_ansi) and
      stop-command echo handling
- [x] Server state persistence (`states.json`) with re-attach/boot restore
- [x] Installer exit-code checking (improvement over wings: non-zero
      script exit fails the install)
- [x] Hardlink-aware disk usage accounting (improvement over wings)
- [x] Activity log persistence (SQLite, wings activityCron + sftpCron merge)
- [x] CLI `configure` + `diagnostics` subcommands
- [x] Conformance suite (`tests/conformance.py`) exercising the full HTTP +
      WebSocket + SFTP surface against a mock or live panel
- [ ] Battle-tested production track record (CVE history, audits)

## License

MIT