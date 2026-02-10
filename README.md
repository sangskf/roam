# Roam - Remote Maintenance Tool

A Rust-based remote maintenance tool with Client-Server architecture.

## Architecture

- **Server**: Axum Web Server + WebSocket + SQLite
- **Client**: Tokio + Tungstenite + Sysinfo
- **Protocol**: JSON over WebSocket

## Features

- Client Registration (Auth)
- Heartbeat Monitoring
- Remote Command Execution
- Hardware Information Gathering
- REST API for Management

## Usage

### Prerequisites
- Rust (cargo)

### Running the Server

```bash
cargo run --bin server
```
Server listens on `0.0.0.0:3000` by default.

### Running the Client

```bash
cargo run --bin client
```
Client connects to `ws://127.0.0.1:3000/ws`.

### API Usage

List Clients:
```bash
curl http://localhost:3000/api/clients
```

Send Command (Get Hardware Info):
```bash
# Replace CLIENT_ID with actual ID from list
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd": {"cmd_type": "GetHardwareInfo"}}' \
  http://localhost:3000/api/clients/CLIENT_ID/command
```

Send Shell Command:
```bash
curl -X POST -H "Content-Type: application/json" \
  -d '{"cmd": {"cmd_type": "ShellExec", "args": {"cmd": "ls", "args": ["-la"]}}}' \
  http://localhost:3000/api/clients/CLIENT_ID/command
```

## Project Structure

- `common`: Shared message types and protocol definitions.
- `server`: Server implementation.
- `client`: Client implementation.
