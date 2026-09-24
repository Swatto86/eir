/**
 * Drives an isolated, portable EirSvc for the e2e suite.
 *
 * EirSvc's portable mode (`eir-svc.exe portable <sentinel> <pipeName>
 * <stateRoot>`) is what makes this suite safe to run on a desktop that
 * already has the installed LocalSystem `EirSvc` service running its own
 * pipe (`\\.\pipe\EirSvc`) and its own state under
 * `%LOCALAPPDATA%\EirPortable`:
 *
 * - The pipe name is random per run (`\\.\pipe\EirSvcPortable-<32 hex>`) and
 *   validated by both sides (see `eir-ui/src/pipe_client.rs` and
 *   `eir-svc/src/main.rs`), so it can never collide with the installed pipe.
 * - The state root is `<LOCALAPPDATA>\EirPortable`, and the service reads
 *   `LOCALAPPDATA` from its own process environment — so pointing that at a
 *   fresh temp directory keeps this run's config/audit-db/settings away from
 *   both the installed service and any other e2e run.
 * - The service refuses to start unless it is unelevated (Default/Limited
 *   token, never LocalSystem or split-token-full) and the sentinel file is a
 *   regular, non-reparse sibling of `eir-svc.exe` — see
 *   `pipe_server::current_process_portable_allowed` and
 *   `validated_portable_sentinel_at`.
 *
 * Everything here is by-PATH, never by a held process handle: `onPrepare`
 * (which starts the service) runs in the wdio launcher process, but specs
 * — including the settings-persistence spec, which restarts the service —
 * run in a separately forked worker process that does not share the
 * launcher's memory. `pidsByPath` (also used for the UI binary) is the one
 * source of truth for "is it running" from either process.
 *
 * Ending the run is done the way the app itself ends portable mode: delete
 * the sentinel, and the service's own poll (`main.rs`'s portable CLI arm)
 * notices within 250ms and exits on its own. A targeted `taskkill /PID` is
 * only the fallback if that graceful path doesn't land in time — and it is
 * always a PID found by this module's own path lookup, never by image name.
 */
import { execFileSync, spawn, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const SENTINEL_NAME = "eir-portable.running";

export interface PortableRun {
  pipeName: string;
  sentinelPath: string;
  stateRoot: string;
  isolatedLocalAppData: string;
  runRoot: string;
  svcExe: string;
}

/** PIDs of running copies of exactly this executable, matched by full path — never by name. */
export function pidsByPath(exePath: string): number[] {
  const script =
    "$p = $env:EIR_E2E_EXE; Get-CimInstance Win32_Process | " +
    "Where-Object { $_.ExecutablePath -eq $p } | ForEach-Object { $_.ProcessId }";
  const result = execFileSync(
    "powershell.exe",
    ["-NoProfile", "-NonInteractive", "-Command", script],
    { encoding: "utf8", env: { ...process.env, EIR_E2E_EXE: exePath } },
  );
  return result.split(/\s+/).filter(Boolean).map(Number);
}

export function killPid(pid: number): void {
  spawnSync("taskkill", ["/pid", String(pid), "/T", "/F"], { stdio: "pipe" });
}

/** A random, validated portable pipe name: `\\.\pipe\EirSvcPortable-<32 hex>`. */
function randomPipeName(): string {
  return `\\\\.\\pipe\\EirSvcPortable-${crypto.randomBytes(16).toString("hex")}`;
}

const CONFIG_TOML = (claudeCliPath: string): string => `[api]
provider = "claude_cli"
model = ""
claude_cli_path = ${JSON.stringify(claudeCliPath)}

[monitoring]
decision_interval_secs = 600
event_log_poll_interval_secs = 30
wmi_poll_interval_secs = 300
confidence_threshold = 0.80
game_mode_auto = true
game_mode_power_boost = false
# The suite drives the tray directly, so require_tray=true is what makes a
# decision cycle run as soon as the tray connects instead of waiting out
# decision_interval_secs.
require_tray = true
watch_screen_errors = true

[advisor]
enabled = false
low_confidence_threshold = 0.6

[updater]
# Never let the autonomous app-updater run against this machine.
enabled = false
interval_secs = 86400

[persistence]
audit_db = "eir.db"
`;

/**
 * Sets up one run's isolated state: a fresh `LOCALAPPDATA` with
 * `EirPortable\{config.toml,policy.toml}` pre-created (EirSvc requires the
 * state root to already exist — see `validated_portable_state_root_at`), a
 * random pipe name, and the sentinel path next to the debug binaries.
 *
 * Fails fast if a sentinel already exists there: `target/debug` is shared
 * across runs, so a leftover sentinel means a previous run's service is
 * still claiming to be live (or didn't clean up), and this run must not
 * silently take over its identity.
 */
export function prepareRun(svcExe: string, claudeCliPath: string): PortableRun {
  const sentinelPath = path.join(path.dirname(svcExe), SENTINEL_NAME);
  if (fs.existsSync(sentinelPath)) {
    throw new Error(
      `${sentinelPath} already exists — a previous e2e run's service looks ` +
        "like it's still live (or didn't clean up). Not starting over it.",
    );
  }

  const runRoot = fs.realpathSync.native(
    fs.mkdtempSync(path.join(os.tmpdir(), "eir-e2e-")),
  );
  const isolatedLocalAppData = path.join(runRoot, "AppData", "Local");
  const stateRoot = path.join(isolatedLocalAppData, "EirPortable");
  fs.mkdirSync(stateRoot, { recursive: true });

  fs.writeFileSync(path.join(stateRoot, "config.toml"), CONFIG_TOML(claudeCliPath));
  const policySrc = path.resolve(path.dirname(svcExe), "..", "..", "policy.toml");
  fs.copyFileSync(policySrc, path.join(stateRoot, "policy.toml"));

  // The sentinel must exist before the service starts — it's what tells the
  // service (and, via EIR_PORTABLE, the UI) that this is a trusted portable
  // run, and deleting it is how the run is torn down.
  fs.writeFileSync(sentinelPath, "running");

  return { pipeName: randomPipeName(), sentinelPath, stateRoot, isolatedLocalAppData, runRoot, svcExe };
}

/**
 * Spawns `eir-svc.exe portable <sentinel> <pipe> <stateRoot>` with an
 * isolated LOCALAPPDATA. Detached and unreferenced: this run's lifetime is
 * tracked by PID-lookup-by-path (see `pidsByPath`/`stopService`), not by
 * holding on to this handle, which would be useless from another process.
 */
export function startService(run: PortableRun): void {
  const proc = spawn(
    run.svcExe,
    ["portable", run.sentinelPath, run.pipeName, run.stateRoot],
    {
      env: { ...process.env, LOCALAPPDATA: run.isolatedLocalAppData },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
      detached: true,
    },
  );
  proc.stdout?.on("data", (chunk: Buffer) => process.stdout.write(`[eir-svc] ${chunk}`));
  proc.stderr?.on("data", (chunk: Buffer) => process.stderr.write(`[eir-svc] ${chunk}`));
  proc.unref();
}

/** Waits until the named pipe accepts a connection, or the deadline passes. */
export async function waitForPipe(run: PortableRun, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError = "no attempt made";
  while (Date.now() < deadline) {
    if (pidsByPath(run.svcExe).length === 0) {
      throw new Error(`${run.svcExe} is not running (exited before opening its pipe?)`);
    }
    const ok = await new Promise<boolean>((resolve) => {
      const socket = net.connect(run.pipeName);
      socket.once("connect", () => {
        socket.destroy();
        resolve(true);
      });
      socket.once("error", (error) => {
        lastError = String(error);
        resolve(false);
      });
    });
    if (ok) return;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`${run.pipeName} never accepted a connection within ${timeoutMs}ms (last: ${lastError})`);
}

/**
 * Stops the service gracefully: delete the sentinel (the service polls for
 * this every 250ms and exits on its own), wait for its PID (found by exact
 * path, never by name) to disappear, and only fall back to a targeted
 * `taskkill /PID` if it hasn't gone within the deadline.
 */
export async function stopService(run: PortableRun, timeoutMs = 15_000): Promise<void> {
  try {
    fs.rmSync(run.sentinelPath, { force: true });
  } catch {
    // already gone
  }
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && pidsByPath(run.svcExe).length > 0) {
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  for (const pid of pidsByPath(run.svcExe)) killPid(pid);
}

/** Restarts the service against the same pipe name and state root. */
export async function restartService(run: PortableRun): Promise<void> {
  await stopService(run);
  // stopService deleted the sentinel; the service won't start without it.
  fs.writeFileSync(run.sentinelPath, "running");
  startService(run);
  await waitForPipe(run);
}

/** Best-effort cleanup of this run's temp state (kept on disk if the caller wants to inspect it). */
export function removeRunState(run: PortableRun): void {
  try {
    fs.rmSync(run.runRoot, { recursive: true, force: true });
  } catch {
    // best-effort
  }
}

// ── Cross-process handoff ───────────────────────────────────────────────────
//
// onPrepare runs in the wdio launcher process; specs run in a separately
// forked worker process (and onComplete is back in the launcher). None of
// them share JS memory, so the run's (immutable, decided-once) identity is
// published as an environment variable in onPrepare — set before it returns,
// so every process @wdio/local-runner forks afterwards inherits it — rather
// than held in a module-level variable that only the process which set it
// could see.
const RUN_ENV_VAR = "EIR_E2E_RUN";

export function publishRun(run: PortableRun): void {
  process.env[RUN_ENV_VAR] = JSON.stringify(run);
}

export function getRun(): PortableRun {
  const raw = process.env[RUN_ENV_VAR];
  if (!raw) throw new Error(`${RUN_ENV_VAR} is not set — onPrepare did not run in this process tree`);
  return JSON.parse(raw) as PortableRun;
}
