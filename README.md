[![CI](https://github.com/IA-PieroCV/cc_matrix_channel/actions/workflows/ci.yml/badge.svg)](https://github.com/IA-PieroCV/cc_matrix_channel/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Matrix Channel for Claude Code

A two-way Matrix channel bridge for [Claude Code](https://code.claude.com). Chat with your running Claude Code session from any Matrix client — phone, desktop, or web.

Built in Rust with full E2EE support. Works with any Matrix homeserver (Continuwuity, Synapse, Conduit, Dendrite).

## How it works

This is a [Claude Code channel](https://code.claude.com/docs/en/channels) — an MCP server that pushes Matrix messages into your running session. Claude reads them, works on your files, and replies back through Matrix.

```
Matrix Client (phone/desktop)
    ↕ Matrix protocol
Your Homeserver
    ↕ Client-Server API
cc_matrix_channel (Rust binary, MCP over stdio)
    ↕ JSON-RPC
Claude Code
```

## vs Official Channels

This project matches the official Telegram/Discord channels at feature parity, with improvements in several areas:

| | Official (Telegram/Discord) | Matrix Channel |
|---|---|---|
| Text messaging | Reply, react, edit, chunking | Same |
| File handling | Download + send attachments | Same, with E2EE auto-decryption |
| Threading | reply_to support | Same |
| Typing + ack | Typing indicator + emoji reaction | Same |
| Security | Instruction-based anti-injection | Same + constant-time pairing, CWD restriction |
| Access control | Pairing flow, allowlist, state persistence | Same + rate limits, expiry |
| E2EE | N/A (platform-native) | Auto cross-signing, encrypted media download |

## Installation

### Prebuilt binaries

Download from [GitHub Releases](https://github.com/IA-PieroCV/cc_matrix_channel/releases/latest):

```bash
# Linux / macOS
tar xzf cc_matrix_channel-*.tar.gz
sudo cp cc_matrix_channel-*/cc_matrix_channel /usr/local/bin/
```

```powershell
# Windows (PowerShell)
Expand-Archive cc_matrix_channel-*.zip -DestinationPath .
Copy-Item cc_matrix_channel-*\cc_matrix_channel.exe "$env:USERPROFILE\.cargo\bin\"
```

### Build from source

Requires [Rust](https://rustup.rs/) 1.85+:

```bash
cargo install --git https://github.com/IA-PieroCV/cc_matrix_channel
```

## Setup

### 1. Create a bot account

```bash
# In your homeserver admin room (Continuwuity example):
!admin users create @bot:yourserver.org YOUR_PASSWORD
```

### 2. Configure

Add to `~/.claude.json` under your project's `mcpServers`:

```json
{
  "mcpServers": {
    "matrix": {
      "command": "cc_matrix_channel",
      "env": {
        "MATRIX_HOMESERVER_URL": "https://matrix.yourserver.org",
        "MATRIX_PASSWORD": "YOUR_BOT_PASSWORD",
        "MATRIX_USER_ID": "@bot:yourserver.org"
      }
    }
  }
}
```

First run logs in with password, bootstraps E2EE cross-signing, and saves the session. Subsequent runs restore automatically.

### 3. Launch

```bash
claude --dangerously-load-development-channels server:matrix
```

Invite the bot to a room and start chatting.

## Configuration

| Variable | Required | Default | Description |
|---|---|---|---|
| `MATRIX_HOMESERVER_URL` | Yes | — | Homeserver URL |
| `MATRIX_PASSWORD` | Yes* | — | Bot password (first-run login) |
| `MATRIX_USER_ID` | Yes | — | Bot user ID (`@bot:server`) |
| `MATRIX_ACCESS_TOKEN` | No | — | Fallback auth (limited E2EE) |
| `MATRIX_DEVICE_ID` | No | auto | Device ID (auto-generated on first login) |
| `MATRIX_ALLOWED_USERS` | No | pairing | Comma-separated allowlist, or `*` |
| `MATRIX_STORE_PATH` | No | `./data/matrix_store` | E2EE key storage |
| `MATRIX_STORE_PASSPHRASE` | No | — | Encrypt store at rest |
| `MATRIX_STATIC_ACCESS` | No | `false` | Disable pairing |
| `MATRIX_MENTION_ONLY_ROOMS` | No | — | Rooms requiring @-mention |
| `MATRIX_ACK_EMOJI` | No | `👀` | Receipt reaction (empty to disable) |

*Password needed for first run only. Session persists after that.

## Tools

| Tool | Description |
|------|-------------|
| `reply` | Send text + optional files, with threading support |
| `react` | Add emoji reaction |
| `edit_message` | Edit a previously sent message |
| `download_attachment` | Download files (auto-decrypts E2EE media) |
| `send_attachment` | Send local files |
| `fetch_messages` | Room message history |
| `approve_pairing` | Approve user access (terminal only) |

## Bot commands

Send directly in Matrix (handled by the bot, not forwarded to Claude):

- `/start` — What the bot does
- `/help` — Available commands
- `/status` — Uptime and session info

## Development

```bash
cargo build           # Debug
cargo build --release # Release
cargo test            # Tests
cargo clippy          # Lint
```

### Releasing

```bash
# Update version in Cargo.toml, then:
git tag v0.1.0
git push && git push --tags
# GitHub Actions builds binaries for Linux, macOS, Windows
```

## Troubleshooting

| Problem | Fix |
|---|---|
| E2EE cross-signing fails | Delete `./data/matrix_store` and restart with `MATRIX_PASSWORD` |
| "Cannot send to this room" | Send a message to the bot first (outbound gate) |
| Session won't restore | Check `MATRIX_STORE_PATH` points to the correct directory |
| Bot ignores messages | Check `MATRIX_ALLOWED_USERS` or complete pairing |
| Files download as encrypted bytes | Use `event_id` + `room_id` params in `download_attachment` |

## Contributing

1. Fork and branch
2. `cargo fmt && cargo clippy && cargo test`
3. Open a PR

---

Licensed under [MIT](LICENSE).
