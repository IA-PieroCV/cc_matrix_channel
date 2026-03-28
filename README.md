[![CI](https://github.com/IA-PieroCV/cc_matrix_channel/actions/workflows/ci.yml/badge.svg)](https://github.com/IA-PieroCV/cc_matrix_channel/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Matrix Channel for Claude Code

Chat with your running Claude Code session from any Matrix client.

Works with any Matrix homeserver (Continuwuity, Synapse, Conduit, Dendrite). Full E2EE support.

## Features

- Two-way messaging with reply threading
- File attachments (send, receive, auto-decrypt E2EE media)
- Permission relay — approve/deny tool calls remotely
- Runtime config — change settings without restart
- Pairing-based access control

## Setup (one-time)

Requires [Bun](https://bun.sh).

```
/plugin marketplace add IA-PieroCV/cc_matrix_channel
/plugin install matrix@cc-matrix-channel
/reload-plugins
# You must need to restart your session to get the slash commands working and connect to the channel MCP
# claude --dangerously-load-development-channels plugin:matrix@cc-matrix-channel
/configure https://matrix.example.com @bot:example.com YOUR_PASSWORD
OR
/matrix:configure https://matrix.example.com @bot:example.com YOUR_PASSWORD
```

Then pair your account — DM the bot from your Matrix client:

```
/access pair <code>
OR
/matrix:access pair <code>

/access policy allowlist
OR
/matrix:access policy allowlist
```

## Usage (every session)

```bash
claude --dangerously-load-development-channels plugin:matrix@cc-matrix-channel
```

## Skills

| Skill | Description |
|-------|-------------|
| `/matrix:access` | Pairing, allowlists, groups, delivery settings |
| `/matrix:configure` | Credentials, status, setup guide |

## Tools

| Tool | Description |
|------|-------------|
| `reply` | Send text + optional files, with threading |
| `react` | Emoji reaction |
| `edit_message` | Edit a sent message |
| `download_attachment` | Download files (auto-decrypts E2EE) |
| `send_attachment` | Send local files |
| `fetch_messages` | Room history |
| `approve_pairing` | Approve user access (terminal only) |

## Configuration

### Connection (`~/.claude/channels/matrix/.env`)

| Variable | Required | Description |
|---|---|---|
| `MATRIX_HOMESERVER_URL` | Yes | Homeserver URL |
| `MATRIX_USER_ID` | Yes | Bot user ID (`@bot:server`) |
| `MATRIX_PASSWORD` | Yes* | Bot password (first-run only) |
| `MATRIX_STORE_PATH` | No | E2EE key storage (default: `./data/matrix_store`) |
| `MATRIX_ACCESS_TOKEN` | No | Fallback auth (limited E2EE) |
| `MATRIX_DEVICE_ID` | No | Device ID hint (auto-generated) |
| `MATRIX_STORE_PASSPHRASE` | No | Encrypt store at rest |

### Access & Delivery (`~/.claude/channels/matrix/access.json`)

Managed via `/matrix:access`. Changes take effect immediately.

| Setting | Default | Description |
|---------|---------|-------------|
| `dmPolicy` | `pairing` | `pairing`, `allowlist`, or `disabled` |
| `allowFrom` | `[]` | Allowed user IDs |
| `groups` | `{}` | Per-room mention-only + patterns |
| `ackReaction` | `👀` | Ack emoji (empty to disable) |
| `textChunkLimit` | `4096` | Max chars per chunk |
| `chunkMode` | `newline` | `newline` or `length` |
| `replyToMode` | `first` | `first`, `all`, or `off` |

## Manual Install

Without the plugin system:

1. Download binary from [Releases](https://github.com/IA-PieroCV/cc_matrix_channel/releases/latest)
2. Save credentials to `~/.claude/channels/matrix/.env`
3. Add to `.mcp.json`:
   ```json
   { "mcpServers": { "matrix": { "command": "/path/to/cc_matrix_channel" } } }
   ```
4. `claude --dangerously-load-development-channels server:matrix`

## Build from Source

```bash
git clone https://github.com/IA-PieroCV/cc_matrix_channel
cd cc_matrix_channel
cargo build --release   # Requires Rust 1.85+
```

## Troubleshooting

| Problem | Fix |
|---|---|
| E2EE fails | Delete store directory, restart with password |
| "Cannot send to this room" | Send a message to the bot first |
| Bot ignores messages | Check `/matrix:access` or complete pairing |
| Session won't restore | Verify `MATRIX_STORE_PATH` |

---

Licensed under [MIT](LICENSE).
