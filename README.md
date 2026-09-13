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

## Quick start

Requires **Rust 1.75+** (build) and a working **Docker daemon** (runtime).

```bash
# build
git clone https://github.com/WildanDeveloper/roost.git
cd roost
cargo build --release
```

```bash
# register the node from a panel API key (writes /etc/pterodactyl/config.yml)
sudo ./target/release/roost configure \
    --panel-url https://panel.example.com \
    --token <application-api-key> \
    --node 1

# run the daemon
sudo ROOST_CONFIG=/etc/pterodactyl/config.yml ./target/release/roost
```

Then add the node in the panel as usual (**Admin → Nodes → create**), or point
an existing node at this machine. SSL termination follows the same
`api.ssl` block as Wings.

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
