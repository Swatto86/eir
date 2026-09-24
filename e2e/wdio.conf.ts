/**
 * Drives the real debug eir.exe + eir-svc.exe through the real webview and
 * named pipe, entirely isolated from the installed Eir (LocalSystem `EirSvc`
 * service, pipe `\\.\pipe\EirSvc`, `%LOCALAPPDATA%\EirPortable`, and any tray
 * `eir.exe` running from `C:\Program Files\Eir`) — see service.ts for how.
 *
 * `onPrepare`/`onComplete` run in the wdio launcher process; specs run in a
 * separately forked worker process. Nothing here relies on JS state crossing
 * that boundary — `getRun()` reads the run's identity back out of an
 * environment variable published in `onPrepare` (inherited by forked
 * workers), and the service's liveness is always rechecked by PID-by-path
 * rather than a remembered handle. See service.ts.
 */
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  getRun,
  type PortableRun,
  pidsByPath,
  prepareRun,
  publishRun,
  removeRunState,
  startService,
  stopService,
  waitForPipe,
} from "./service.ts";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");

const targetDir = path.resolve(root, "target/debug");
export const application =
  process.env["EIR_E2E_APPLICATION"] ?? path.join(targetDir, "eir.exe");
export const svcExe = path.join(targetDir, "eir-svc.exe");
const claudeCliPath = path.resolve(here, "fixtures/fake-claude.cmd");

function newestMtime(files: string[]): number {
  return Math.max(
    0,
    ...files.filter((f) => fs.existsSync(f)).map((f) => fs.statSync(f).mtimeMs),
  );
}

/** Newest mtime of any file under `dir`, recursively (0 if `dir` doesn't exist). */
function newestMtimeInDir(dir: string): number {
  if (!fs.existsSync(dir)) return 0;
  let newest = 0;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    newest = Math.max(newest, entry.isDirectory() ? newestMtimeInDir(full) : fs.statSync(full).mtimeMs);
  }
  return newest;
}

function assertFreshBuild(): void {
  if (!fs.existsSync(application)) {
    throw new Error(`no binary at ${application} — build it with: cargo build -p eir-ui -p eir-svc`);
  }
  if (!fs.existsSync(svcExe)) {
    throw new Error(`no binary at ${svcExe} — build it with: cargo build -p eir-ui -p eir-svc`);
  }
  if (path.dirname(application) !== path.dirname(svcExe)) {
    // eir-ui's pipe-server verification requires eir-svc.exe to sit exactly
    // beside eir.exe (see eir-ui/src/pipe_client.rs::verified_portable_service_process).
    throw new Error(`${application} and ${svcExe} must be in the same directory`);
  }
  const uiBuilt = fs.statSync(application).mtimeMs;
  const svcBuilt = fs.statSync(svcExe).mtimeMs;

  // ui/index.html + ui/main.js are embedded into eir.exe at compile time
  // (tauri.conf.json's frontendDist); a newer source file than the binary
  // means the binary predates the frontend it's supposed to be driving.
  const newestFrontendSource = newestMtime([
    path.resolve(root, "ui/index.html"),
    path.resolve(root, "ui/main.js"),
  ]);
  if (newestFrontendSource > uiBuilt) {
    throw new Error(`${application} is older than ui/ — rebuild with: cargo build -p eir-ui -p eir-svc`);
  }

  // Rust source for the whole workspace (eir-proto is shared by both
  // binaries). Compared conservatively against both binaries rather than
  // trying to map each crate to exactly one binary, so a stale eir.exe or
  // eir-svc.exe built from an older tree never passes silently.
  const newestRustSource = Math.max(
    newestMtimeInDir(path.resolve(root, "eir-proto/src")),
    newestMtimeInDir(path.resolve(root, "eir-svc/src")),
    newestMtimeInDir(path.resolve(root, "eir-ui/src")),
  );
  if (newestRustSource > uiBuilt) {
    throw new Error(`${application} predates the Rust source tree — rebuild with: cargo build -p eir-ui -p eir-svc`);
  }
  if (newestRustSource > svcBuilt) {
    throw new Error(`${svcExe} predates the Rust source tree — rebuild with: cargo build -p eir-ui -p eir-svc`);
  }
}

let tauriDriver: ChildProcess | undefined;

function stopDriver(): void {
  if (tauriDriver?.pid) {
    spawnSync("taskkill", ["/pid", String(tauriDriver.pid), "/T", "/F"], { stdio: "pipe" });
  }
  tauriDriver = undefined;
}

const driverStatus = () =>
  fetch("http://127.0.0.1:4444/status", { signal: AbortSignal.timeout(1_000) });

async function assertDriverPortFree(): Promise<void> {
  try {
    await driverStatus();
  } catch {
    return;
  }
  throw new Error(
    "port 4444 is already held by a WebDriver process; stop the stale tauri-driver before rerunning",
  );
}

async function waitForDriver(): Promise<void> {
  const deadline = Date.now() + 30_000;
  let lastError = "no attempt made";
  while (Date.now() < deadline) {
    if (tauriDriver?.exitCode !== null && tauriDriver?.exitCode !== undefined) {
      throw new Error(`tauri-driver exited with code ${tauriDriver.exitCode} before binding 4444`);
    }
    try {
      await driverStatus();
      return;
    } catch (error) {
      lastError = String(error);
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  throw new Error(`tauri-driver never accepted a connection on 127.0.0.1:4444 within 30s (last: ${lastError})`);
}

/**
 * The only verified path in: scripts/run-e2e.ps1's Find-Msedgedriver checks
 * the installed WebView2 runtime version against the driver's own
 * `--version` output and the driver's Microsoft signature before setting
 * this. Running wdio directly (e.g. `npm run e2e` — see e2e/package.json)
 * must not silently skip that verification by falling back to an unverified
 * candidate path, so this trusts nothing else.
 */
function locateMsedgedriver(): string {
  const fromEnv = process.env["EIR_E2E_MSEDGEDRIVER"];
  if (!fromEnv || !fs.existsSync(fromEnv)) {
    throw new Error(
      "EIR_E2E_MSEDGEDRIVER is not set to an existing msedgedriver. Run the suite via " +
        "scripts\\run-e2e.ps1, which locates one matching the installed WebView2 runtime, " +
        "verifies its Microsoft signature, and sets this for you.",
    );
  }
  return fromEnv;
}

// Run as one ordered, shared-session group (exit.spec kills both processes;
// settings-persist.spec restarts the service), never a glob — a glob would
// alphabetize to boot, exit, explain, investigate, noticed-feed,
// settings-persist, running exit.spec second. assertSpecsMatchOrder guards
// against specs/ drifting out of sync with this order (a renamed, added, or
// removed spec file) failing loudly instead of via a confusing timeout.
const SPEC_ORDER = ["boot", "noticed-feed", "explain", "investigate", "settings-persist", "exit"];

function assertSpecsMatchOrder(): void {
  const actual = fs
    .readdirSync(path.resolve(here, "specs"))
    .filter((f) => f.endsWith(".spec.ts"))
    .map((f) => f.slice(0, -".spec.ts".length))
    .sort();
  const expected = [...SPEC_ORDER].sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(
      `e2e/specs contains [${actual.join(", ")}] but SPEC_ORDER in wdio.conf.ts expects ` +
        `[${expected.join(", ")}] — update SPEC_ORDER to match (order matters, see this file's header).`,
    );
  }
}
assertSpecsMatchOrder();

export const config: WebdriverIO.Config = {
  runner: "local",
  framework: "mocha",
  specs: [SPEC_ORDER.map((name) => path.resolve(here, `specs/${name}.spec.ts`))],
  maxInstances: 1,
  logLevel: "error",
  reporters: ["spec"],
  // Give a spec that makes a (fake) model call a realistic deadline — see
  // ~/.agents/tauri-webdriver.md on flaky short timeouts.
  mochaOpts: { ui: "bdd", timeout: 120_000 },
  hostname: "127.0.0.1",
  port: 4444,
  capabilities: [
    {
      // @ts-expect-error tauri:options is a tauri-driver capability.
      "tauri:options": { application },
      browserName: "wry",
      // WebdriverIO 9 otherwise requests a BiDi session, which tauri-driver
      // proxies verbatim to msedgedriver on a path nobody tests, producing a
      // connected session that reports a blank about:blank page.
      "wdio:enforceWebDriverClassic": true,
    },
  ],

  onPrepare: async () => {
    // Declared outside the try so the catch below can tear down the service
    // (and its temp state) for every failure after prepareRun() succeeds —
    // a driver/build/timeout failure must never leave an isolated eir-svc.exe
    // plus its sentinel and %TEMP% state root running on the machine.
    let run: PortableRun | undefined;
    try {
      assertFreshBuild();
      await assertDriverPortFree();
      if (pidsByPath(application).length > 0) {
        throw new Error("the e2e UI binary is already running; the suite will not terminate it");
      }
      if (pidsByPath(svcExe).length > 0) {
        throw new Error("the e2e service binary is already running; the suite will not terminate it");
      }

      run = prepareRun(svcExe, claudeCliPath);
      publishRun(run); // before onPrepare returns, so forked workers inherit it
      console.log(`E2E state: ${run.stateRoot}`);
      console.log(`E2E pipe: ${run.pipeName}`);
      startService(run);
      await waitForPipe(run);

      const nativeDriver = locateMsedgedriver();
      // EIR_PORTABLE / EIR_PORTABLE_PIPE reach eir.exe because tauri-driver
      // spawns the app with its own inherited environment — verified by the
      // boot spec (invoke('is_portable') + the fake analysis text appearing).
      // scripts/run-e2e.ps1 passes the driver it located (PATH, .webdriver or
      // ~/.cargo/bin); a direct `wdio run` falls back to PATH.
      const tauriDriverBin = process.env.EIR_E2E_TAURI_DRIVER || "tauri-driver";
      tauriDriver = spawn(tauriDriverBin, ["--native-driver", nativeDriver], {
        stdio: [null, process.stdout, process.stderr],
        shell: false,
        env: {
          ...process.env,
          EIR_PORTABLE: "1",
          EIR_PORTABLE_PIPE: run.pipeName,
        },
      });
      process.once("exit", stopDriver);
      await waitForDriver();
    } catch (error) {
      if (tauriDriver?.pid) tauriDriver.kill();
      console.error(error);
      if (run) {
        try {
          // PID-by-path, same as onComplete — the service may have come up
          // even though a later step (driver locate/spawn/handshake) failed.
          await stopService(run);
          removeRunState(run);
        } catch (cleanupError) {
          console.error(`[cleanup] ${String(cleanupError)}`);
        }
      }
      process.exit(1);
    }
  },

  afterTest: async (_test, _context, { passed }) => {
    if (passed) return;
    try {
      const run = getRun();
      const evidence = path.join(run.runRoot, `eir-e2e-failure-${Date.now()}`);
      await browser.saveScreenshot(`${evidence}.png`);
      const url = await browser.getUrl();
      const source = (await browser.getPageSource()).length;
      console.error(`[diagnostic] url=${url} source=${source} chars`);
      console.error(`[diagnostic] screenshot: ${evidence}.png`);
    } catch (error) {
      console.error(`[diagnostic] no session to capture: ${String(error)}`);
    }
  },

  onComplete: async (exitCode) => {
    stopDriver();
    try {
      const run = getRun();
      // PID-by-path, not a remembered handle: a spec (settings-persist) may
      // have restarted the service in a different process since onPrepare.
      await stopService(run);
      if (exitCode === 0) {
        removeRunState(run);
      } else {
        console.log(`E2E evidence kept at: ${run.runRoot}`);
      }
    } catch (error) {
      console.error(`[cleanup] ${String(error)}`);
    }
  },
};

// Re-exported for the specs, which need the exact debug binary paths and the
// run's identity without duplicating how they're computed.
export { getRun, pidsByPath };
