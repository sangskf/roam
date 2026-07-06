# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo check                    # Fast compilation check
cargo clippy                   # Lint
cargo fmt                      # Format code
```

### Tests
```bash
cargo test                     # All tests
cargo test -p server            # Server crate tests only
cargo test -p server scheduler::tests::test_cron_every_minute  # Single test
```

### Release a new version
```bash
./release.sh 0.6.0             # Bumps version in server/client/common Cargo.tomls, commits, tags
git push && git push --tags    # Triggers CI release workflow
```

### Run server
```bash
./target/release/server                          # Development (config from CWD)
./target/release/server gen-cert                 # Generate self-signed TLS certs (rcgen)
sudo ./target/release/server install && start    # System service mode (service-manager crate)
```

### Run client
```bash
./target/release/client
TLS_INSECURE=true ./target/release/client        # Skip TLS verification for self-signed certs
```

### SQL queries
Uses SQLx with `SQLX_OFFLINE=true` (queries cached in `.sqlx/`). After changing queries:
```bash
cargo install sqlx-cli
cargo sqlx prepare -- --lib                      # Update .sqlx/ cache
```

## Architecture

Workspace with 3 crates. Communication: server <-> client over JSON-over-WS; browser <-> server over HTTP/REST.

### `common/` — Protocol library

Defines the `Message` enum with these variants:
- `Register` / `AuthSuccess` / `AuthFailed` — connection lifecycle
- `Heartbeat` — keepalive from client
- `Command { id, cmd: CommandPayload }` — server→client
- `Response { id, result: CommandResult }` — client→server (correlated by id)
- `Progress { id, message }` — streaming progress during file transfers

`CommandPayload` is a tagged enum (`cmd_type` field in JSON) with variants: `ShellExec`, `ChangeDir`, `DownloadFile`, `UploadFile`, `ListDir`, `GetHardwareInfo`, `UpdateClient`, `ReadFile`, `WriteFile`, `DownloadAndUnzip`, `ZipAndUpload`, `CopyFile`, `MoveFile`, `DeleteFile`, `HttpRequest`.

### `server/` — Axum HTTP/WS server + SQLite (SQLx) + embedded Vue.js SPA

WebSocket handler (`ws_handler`) performs auth, then spawns per-connection read/write tasks. Server sends `Command` messages via mpsc channels stored in `ClientConnection.tx`.

Key state machine in `AppState`:
- `clients: DashMap<Uuid, ClientConnection>` — connected clients with mpsc sender
- `waiters: DashMap<Uuid, oneshot::Sender<CommandResult>>` — pending command results
- `results: DashMap<Uuid, CommandResult>` — completed results
- `active_executions: DashMap<Uuid, ExecutionProgress>` — running script groups
- `web_sessions: DashMap<String, String>` — web auth tokens → username

**Command dispatch pattern:** REST handlers (e.g. `POST /api/clients/:id/command`) send a `Message::Command` over the client's mpsc channel and insert a oneshot sender into `waiters`. The WS read task receives the `Response`, resolves the oneshot, and stores the result in `results`.

Auth middleware exempts: `/api/auth/login`, `/api/auth/status`, `/api/info`, static assets, file download/upload/chunked URLs.

### `client/` — Long-lived WebSocket client

Startup: reads/persists `.client_id` UUID (exe dir) → connects with TLS (optional NoCertificateVerification) → registers → main loop (heartbeat + command handler). Command execution runs on spawned tasks so heartbeats continue during long operations.

File transfers use configurable chunk_size (default 10MB) and parallel transfers (default 4 concurrent). Self-update via `self-replace` crate.

### Database

SQLite with idempotent schema: `CREATE TABLE IF NOT EXISTS` + idempotent `ALTER TABLE` for migrations (ignoring errors). Tables: `clients`, `scripts`, `execution_history`, `client_groups`, `client_group_members`, `group_scripts`, `client_updates`, `scheduled_tasks`, `web_users`. Default admin user seeded (username: `admin`, SHA256 of "admin").

### Scheduler

Checks `scheduled_tasks` table every 60s for enabled tasks due to run. Supports two task types:
- `group` — bind a client group + scripts
- `custom` — direct client IDs + steps

Built-in CRON parser (5-field, standard syntax) — no external dependency. Also runs a daily cleanup of old `client_data` directory.

### Frontend

Single `server/web/index.html` (~248KB) — Vue.js 3 + TailwindCSS. Edit and restart server to see changes. No build step.

## Config

Config sources (later overrides earlier): `*_config.toml/json/yaml` → `.env` → `APP_*` env vars. Executable directory prioritized over CWD (for service mode). Server uses `server_config.*`, client uses `client_config.*`. Both also load `.env`.
