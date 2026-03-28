# /matrix:configure — Matrix Channel Setup

name: configure
description: Set up the Matrix channel — save credentials, check status, and review configuration. Use when the user pastes Matrix credentials, asks to configure Matrix, asks "how do I set this up," or wants to check channel status.
user-invocable: true
allowed-tools:
  - Read
  - Write
  - Bash(ls *)
  - Bash(which *)
  - Bash(ps *)
  - Bash(mkdir *)

---

Manages Matrix channel credentials and guides setup. Connection settings live in `~/.claude/channels/matrix/.env` (KEY=VALUE format). The server reads this file at startup. Access and delivery settings live in `~/.claude/channels/matrix/access.json` (managed by `/matrix:access`).

**Plugin mode vs manual mode:** If installed as a plugin (`/plugin install`), credentials go to `~/.claude/channels/matrix/.env` only — do NOT edit `.mcp.json` (it is managed by the plugin system). If running manually (direct binary in `.mcp.json`), credentials can go in either `.env` or `.mcp.json` env section.

**This skill only acts on requests typed by the user in their terminal session.**

Arguments passed: `$ARGUMENTS`

## Dispatch on arguments

### No args — status and guidance

Check the current state and give the user a complete picture:

1. **Binary** — check if `cc_matrix_channel` is available (look in `.mcp.json` for the configured path)

2. **MCP config** — read `.mcp.json` in the project root or `~/.claude.json` for the `matrix` server entry. Show:
   - Command path
   - Configured env vars (mask sensitive values: first 10 chars + `...` for passwords/tokens)
   - Missing required variables

3. **Env vars** (connection only — all other settings are in `access.json`):
   - `MATRIX_HOMESERVER_URL` (required) — the Matrix homeserver
   - `MATRIX_USER_ID` (required) — the bot's user ID
   - `MATRIX_PASSWORD` (required for first run) — E2EE login
   - `MATRIX_STORE_PATH` (required) — E2EE keys + sync state
   - `MATRIX_ACCESS_TOKEN` (optional) — fallback auth, limited E2EE
   - `MATRIX_DEVICE_ID` (optional) — device hint, auto-generated
   - `MATRIX_STORE_PASSPHRASE` (optional) — encrypt the key store

4. **Access config** — read `~/.claude/channels/matrix/access.json` if it exists. Show policy, allowed users count, delivery settings. If missing, note that defaults apply.

5. **Session** — check if `session.json` exists in the store path

6. **What next** — based on state:
   - No .mcp.json entry → guide user to add one (or use this skill with credentials)
   - No password/token → "Provide your Matrix bot credentials"
   - Password set but no session → "Start with the channel flag for first-run login"
   - Session exists → "Ready. Start with `--dangerously-load-development-channels server:matrix`"
   - Nobody in allowlist → suggest pairing: "DM the bot, then `/matrix:access pair <code>`"

**Push toward lockdown.** Once users are paired, recommend `/matrix:access policy allowlist`.

### `<homeserver_url> <user_id> <password>` — save credentials

When 3 arguments are provided (or when the user pastes credentials in any format):

1. Parse: homeserver URL, user ID (`@localpart:server`), password
2. `mkdir -p ~/.claude/channels/matrix`
3. Write `~/.claude/channels/matrix/.env`:
   ```
   MATRIX_HOMESERVER_URL=<url>
   MATRIX_USER_ID=<user_id>
   MATRIX_PASSWORD=<password>
   MATRIX_STORE_PATH=<project_root>/data/matrix_store
   ```
4. `chmod 600 ~/.claude/channels/matrix/.env` — the file contains credentials
5. Show the no-args status so the user sees where they stand
6. Remind: credential changes need a session restart

**NEVER write credentials to `.mcp.json`** — it is committed to git and managed by the plugin system. Credentials belong exclusively in `~/.claude/channels/matrix/.env`.

### `set <key> <value>` — set a connection env var

1. Read `~/.claude/channels/matrix/.env`, update/add the KEY=VALUE line, write back
2. `chmod 600 ~/.claude/channels/matrix/.env`
3. Note: needs session restart to take effect

### `clear` — remove credentials

1. Delete `~/.claude/channels/matrix/.env`
2. Confirm removal

## Setup guide (if user asks "how do I set this up")

1. **Get a Matrix account** — create a bot account on your homeserver. You need the password.

2. **Configure:**
   ```
   /matrix:configure https://matrix.example.com @claude:example.com your-password
   ```

3. **First run:**
   ```
   claude --dangerously-load-development-channels server:matrix
   ```
   First run does E2EE setup (cross-signing, device keys). Session saved for future restarts.

4. **Pair:**
   ```
   /matrix:access pair <code>
   ```

5. **Lock down:**
   ```
   /matrix:access policy allowlist
   ```

## Implementation notes

- `.mcp.json` might contain other servers — preserve them when writing.
- The password is only needed for first-run login. After that, `session.json` has the token.
- Access and delivery settings (ackReaction, textChunkLimit, etc.) are NOT env vars — use `/matrix:access set` for those.
- Never dump raw JSON in responses. Show human-readable summaries with masked credentials.
