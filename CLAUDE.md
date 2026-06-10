# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo check                    # Fast compilation check
```

### Release a new version
```bash
./release.sh 0.6.0             # Bumps version in all Cargo.tomls, commits, tags
git push && git push --tags    # Triggers CI build (see .github/workflows/release.yml)
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
TLS_INSECURE=true ./target/release/client        # Skip TLS verification for self-signed certs
```

### When modifying SQL queries
```bash
cargo install sqlx-cli
SQLX_OFFLINE=true cargo build                    # Works offline with cached queries
cargo sqlx prepare -- --lib                      # Update .sqlx/ cache after query changes
```

### Environment & Config
Config sources (later overrides earlier): `*_config.toml/json/yaml` files → `.env` file → `APP_*` env vars. Files in the executable directory (service mode) take priority over CWD files (development). Server uses `server_config.*`, client uses `client_config.*`, and both also load `.env`.

## Architecture

Workspace with 3 crates:

- **`common/`** — Shared protocol library. Defines `Message` enum (Register, Auth, Heartbeat, Command, Response) and `CommandPayload`/`CommandResult` types. All client↔server communication is JSON over WebSocket. The protocol follows a strict send-ack pattern: Client connects → sends Register with token → waits for AuthSuccess → then bidirectional message exchange begins.

- **`server/`** — Axum HTTP/WS server with SQLite (SQLx). Embedded Vue.js SPA via `rust-embed`. Key data flow: Web browser ←HTTP/REST→ Server ←WebSocket→ Clients. State held in `AppState` (SQLite pool + DashMap for connected clients, pending results, oneshot waiters, execution tracking). REST handlers call `send_command()` which sends a Message::Command over the client's mpsc channel and waits on a oneshot for the result. Auth middleware exempts: /api/auth/login, /api/auth/status, /api/info, static assets, and file download/upload URLs (used by unauthenticated clients).

- **`client/`** — Long-lived WebSocket client. Startup: reads/persists `.client_id` UUID in executable directory → connects with TLS (optionally skipping verification via `NoCertificateVerification`) → registers with server → enters main loop (heartbeat on configurable interval + command handler). Command execution covers ShellExec (async subprocess), file operations (streaming download/upload with chunked transfer), directory zipping, binary self-update (self-replace crate), and HTTP requests. File transfers support configurable chunk_size (default 8MB) and parallel transfers (default 4).

### Database
SQLite with idempotent schema management: `CREATE TABLE IF NOT EXISTS` + idempotent `ALTER TABLE` migrations (ignoring errors for existing columns). Schema is defined in `server/src/db.rs`. Reset client status to 'disconnected' on startup. Example scripts/groups seeded on first run.

### Key patterns

- **TLS**: rustls with optional self-signed cert generation (`server gen-cert` via rcgen)
- **Service management**: service-manager crate with install/uninstall/start/stop subcommands (both server and client)
- **File operations**: Chunked upload via PUT `/api/files/chunked-upload/:cmd_id/chunk/:chunk_index` + POST `/api/files/chunked-upload/:cmd_id/complete`. Download via streaming. Directory transfer uses zip.
- **Frontend**: Single `server/web/index.html` (~248 KB) — Vue.js 3 + TailwindCSS. Edit and restart server to see changes. No build step needed.
- **Script groups**: Multi-step workflows (Shell/Upload/Download/Copy/Move/Delete/HttpRequest steps) persisted in SQLite, executed concurrently across selected clients via `ActiveExecutions` tracking.
- **Client reconnection**: Retry loop with 5s delay on disconnect; persistent UUID identity via `.client_id` file.
