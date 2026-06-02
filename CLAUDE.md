# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo check                    # Fast compilation check
```

### Run server
```bash
./target/release/server                          # Development (config from CWD)
./target/release/server gen-cert                 # Generate self-signed TLS certs
sudo ./target/release/server install && start    # System service mode
```

### Run client
```bash
./target/release/client
# Or with TLS verification disabled for self-signed certs:
TLS_INSECURE=true ./target/release/client
```

### Environment
Config is loaded from `.env` or `client_config.*` / `server_config.*` files, with env var overrides (`APP_*` prefix). Files in the executable directory take priority over CWD files.

## Architecture

Workspace with 3 crates:

- **`common/`** — Shared protocol library. Defines `Message` enum (Register, Auth, Heartbeat, Command, Response) and `CommandPayload`/`CommandResult` types. All communication is JSON over WebSocket.

- **`server/`** — Axum HTTP/WS server with SQLite (SQLx). Embedded Vue.js SPA via `rust-embed`. Key flow: Web browser ←HTTP/REST→ Server ←WebSocket→ Clients. State is held in `AppState` (SQLite pool + DashMap for connected clients, results, sessions). Database schema auto-creates on startup with idempotent migrations.

- **`client/`** — Long-lived WebSocket client that registers with the server, sends heartbeats, and executes remote commands (`ShellExec`, `DownloadFile`, `UploadFile`, `ListDir`, `ReadFile`, `WriteFile`, `UpdateClient`, etc.). Uses `sysinfo` for hardware reporting and `self-replace` for binary updates.

### Key patterns

- TLS via `rustls` with optional self-signed cert generation (`server gen-cert`)
- Service management via `service-manager` crate (`install`/`uninstall`/`start`/`stop` subcommands)
- SQLx offline mode (`SQLX_OFFLINE=true`) — no database needed at build time; update `.sqlx/` cache when queries change
- Axum auth middleware checks JWT tokens for most `/api/*` routes (exempt: login, info, static assets, file download/upload)
- Frontend is a single 234 KB `server/web/index.html` — edit and restart server to see changes
- File operations use streaming (reqwest + tokio) for large files; zip operations use `zip` crate
- Client reconnects on disconnect with 5s retry; heartbeat interval configurable
- Script groups support multi-step workflows (Shell + Upload + Download steps) executed concurrently across selected clients
