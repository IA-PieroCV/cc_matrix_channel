#!/usr/bin/env bun
/**
 * Bun launcher for cc_matrix_channel.
 *
 * On first run: detects OS/arch, downloads the correct binary from GitHub releases.
 * On subsequent runs: spawns the binary directly.
 * All logging goes to stderr (stdout is reserved for MCP JSON-RPC).
 *
 * Usage:
 *   bun scripts/launcher.ts          # Download if needed, then spawn the MCP server
 *   bun scripts/launcher.ts --check  # Download if needed, then exit 0 (for SessionStart hook)
 */

import { spawn } from "bun";
import { existsSync, mkdirSync, chmodSync, readdirSync, unlinkSync } from "fs";
import { join, dirname } from "path";
import { homedir } from "os";

const REPO = "IA-PieroCV/cc_matrix_channel";
const BINARY_NAME = process.platform === "win32" ? "cc_matrix_channel.exe" : "cc_matrix_channel";

// Platform → Rust target mapping
const TARGETS: Record<string, Record<string, string>> = {
  linux:  { x64: "x86_64-unknown-linux-gnu",  arm64: "aarch64-unknown-linux-gnu" },
  darwin: { x64: "x86_64-apple-darwin",        arm64: "aarch64-apple-darwin" },
  win32:  { x64: "x86_64-pc-windows-msvc" },
};

function log(msg: string) {
  process.stderr.write(`[matrix] ${msg}\n`);
}

async function getVersion(): Promise<string> {
  const pluginJsonPath = join(dirname(import.meta.dir), ".claude-plugin", "plugin.json");
  if (existsSync(pluginJsonPath)) {
    const data = JSON.parse(await Bun.file(pluginJsonPath).text());
    return data.version;
  }
  // Fallback: read from Cargo.toml
  const cargoPath = join(dirname(import.meta.dir), "Cargo.toml");
  if (existsSync(cargoPath)) {
    const text = await Bun.file(cargoPath).text();
    const match = text.match(/^version\s*=\s*"(.+)"/m);
    if (match) return match[1];
  }
  throw new Error("Cannot determine version from plugin.json or Cargo.toml");
}

function getDataDir(): string {
  if (process.env.CLAUDE_PLUGIN_DATA) {
    return process.env.CLAUDE_PLUGIN_DATA;
  }
  // Fallback for development / manual usage
  return join(homedir(), ".claude", "channels", "matrix", "plugin-data");
}

function getTarget(): string {
  const platform = TARGETS[process.platform];
  if (!platform) throw new Error(`Unsupported platform: ${process.platform}`);
  const target = platform[process.arch];
  if (!target) throw new Error(`Unsupported architecture: ${process.arch} on ${process.platform}`);
  return target;
}

function getBinaryPath(dataDir: string, version: string): string {
  const ext = process.platform === "win32" ? ".exe" : "";
  return join(dataDir, "bin", `cc_matrix_channel-v${version}${ext}`);
}

async function downloadBinary(binaryPath: string, version: string): Promise<void> {
  const target = getTarget();
  const isWindows = process.platform === "win32";
  const archiveExt = isWindows ? "zip" : "tar.gz";
  const archiveName = `cc_matrix_channel-v${version}-${target}`;
  const url = `https://github.com/${REPO}/releases/download/v${version}/${archiveName}.${archiveExt}`;
  const sha256Url = `${url}.sha256`;

  log(`Downloading binary v${version} for ${target}...`);

  // Download archive
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`Download failed: ${response.status} ${response.statusText}\nURL: ${url}`);
  }
  const archiveBuffer = await response.arrayBuffer();

  // Verify SHA256 if available
  try {
    const sha256Response = await fetch(sha256Url);
    if (sha256Response.ok) {
      const sha256Text = (await sha256Response.text()).trim();
      const expectedHash = sha256Text.split(/\s+/)[0].toLowerCase();
      const hasher = new Bun.CryptoHasher("sha256");
      hasher.update(new Uint8Array(archiveBuffer));
      const actualHash = hasher.digest("hex");
      if (actualHash !== expectedHash) {
        throw new Error(`SHA256 mismatch: expected ${expectedHash}, got ${actualHash}`);
      }
      log("SHA256 verified.");
    }
  } catch (e: any) {
    if (e.message?.includes("SHA256 mismatch")) throw e;
    // SHA256 file not available — skip verification
  }

  // Create bin directory
  const binDir = dirname(binaryPath);
  mkdirSync(binDir, { recursive: true });

  // Extract binary from archive
  const archivePath = join(binDir, `archive.${archiveExt}`);
  await Bun.write(archivePath, new Uint8Array(archiveBuffer));

  if (isWindows) {
    // Windows: use PowerShell to extract zip
    const extract = spawn([
      "powershell", "-Command",
      `Expand-Archive -Path '${archivePath}' -DestinationPath '${binDir}' -Force`,
    ]);
    await extract.exited;
  } else {
    // Unix: use tar
    const extract = spawn(["tar", "xzf", archivePath, "-C", binDir, "--strip-components=1",
      `${archiveName}/${BINARY_NAME}`]);
    await extract.exited;
  }

  // Rename extracted binary to versioned path
  const extractedPath = join(binDir, BINARY_NAME);
  if (existsSync(extractedPath) && extractedPath !== binaryPath) {
    await Bun.write(binaryPath, Bun.file(extractedPath));
    unlinkSync(extractedPath);
  }

  // Cleanup archive
  unlinkSync(archivePath);

  // chmod +x on Unix
  if (process.platform !== "win32") {
    chmodSync(binaryPath, 0o755);
  }

  // Clean up old versioned binaries
  try {
    for (const file of readdirSync(binDir)) {
      if (file.startsWith("cc_matrix_channel-v") && file !== `cc_matrix_channel-v${version}${process.platform === "win32" ? ".exe" : ""}`) {
        unlinkSync(join(binDir, file));
      }
    }
  } catch {
    // Best-effort cleanup
  }

  log("Binary ready.");
}

async function main() {
  const checkOnly = process.argv.includes("--check");
  const version = await getVersion();
  const dataDir = getDataDir();
  const binaryPath = getBinaryPath(dataDir, version);

  if (!existsSync(binaryPath)) {
    await downloadBinary(binaryPath, version);
  }

  if (checkOnly) {
    process.exit(0);
  }

  // Spawn the binary with inherited stdio (MCP over stdin/stdout)
  const proc = spawn([binaryPath], {
    stdio: ["inherit", "inherit", "inherit"],
    env: { ...process.env },
  });

  const exitCode = await proc.exited;
  process.exit(exitCode);
}

main().catch((e) => {
  log(`Error: ${e.message}`);
  process.exit(1);
});
