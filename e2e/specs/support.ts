/**
 * Shared helpers for the specs. Not named `*.spec.ts`: the runner globs for
 * that suffix and a helper picked up as a spec would open an app session
 * that asserts nothing.
 */
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));

/**
 * The DOM text, not the rendered text: `getText()` applies CSS, so wording
 * styled with `text-transform` cannot be asserted through it.
 */
export async function text(selector: string): Promise<string> {
  return browser.execute(
    (sel: string) => document.querySelector(sel)?.textContent?.trim() ?? "",
    selector,
  );
}

export async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  return browser.executeAsync(
    (cmd: string, cmdArgs: Record<string, unknown> | undefined, done: (value: unknown) => void) => {
      // @ts-expect-error window.__TAURI__ exists because withGlobalTauri is set.
      window.__TAURI__.core
        .invoke(cmd, cmdArgs)
        .then((value: unknown) => done({ ok: true, value }))
        .catch((error: unknown) => done({ ok: false, error: String(error) }));
    },
    command,
    args,
  ) as unknown as Promise<T>;
}

/**
 * What the webview is actually showing, printed before a wait rather than
 * guessed at afterwards. A `tauri://` URL is a production binary;
 * `about:blank` with an empty document is a driver problem, not an app bug.
 */
export async function logWebviewDiagnostics(): Promise<void> {
  const url = await browser.getUrl().catch((e: unknown) => `getUrl failed: ${String(e)}`);
  const source = await browser.getPageSource().catch(() => "");
  console.log(`[diagnostic] url=${url} source=${source.length} chars`);
  const handles = await browser.getWindowHandles().catch(() => [] as string[]);
  console.log(`[diagnostic] window handles: ${handles.length}`);
}

export async function screenshot(dataDir: string, name: string): Promise<void> {
  await browser.saveScreenshot(path.join(dataDir, `${name}.png`));
}

export function readJson<T>(dataDir: string, name: string): T | undefined {
  const file = path.join(dataDir, name);
  if (!fs.existsSync(file)) return undefined;
  return JSON.parse(fs.readFileSync(file, "utf8")) as T;
}

export async function waitForAiNow(matcher: RegExp | string, timeout = 60_000): Promise<void> {
  await browser.waitUntil(
    async () => {
      const current = await text("#ai-now-text");
      return typeof matcher === "string" ? current.includes(matcher) : matcher.test(current);
    },
    {
      timeout,
      timeoutMsg: `#ai-now-text never matched ${matcher} (it reads "${await text("#ai-now-text")}")`,
    },
  );
}

export async function showView(name: string): Promise<void> {
  const btn = await $(`.nav-btn[data-view="${name}"]`);
  await btn.waitForExist({ timeout: 15_000 });
  await btn.click();
}

/**
 * Shows a real Windows error dialog (see fixtures/inject-error-dialog.ps1)
 * and returns the PID of the process that's blocking on it — the only thing
 * `endInjectedDialog` ever kills.
 */
export function injectErrorDialog(title: string, text: string): number {
  const script = path.resolve(here, "../fixtures/inject-error-dialog.ps1");
  const out = execFileSync(
    "powershell.exe",
    ["-NoProfile", "-NonInteractive", "-File", script, "-Title", title, "-Text", text],
    { encoding: "utf8" },
  );
  const pid = Number(out.trim().split(/\r?\n/).pop());
  if (!Number.isInteger(pid) || pid <= 0) {
    throw new Error(`inject-error-dialog.ps1 did not print a PID (got: ${JSON.stringify(out)})`);
  }
  return pid;
}

/** Ends a dialog started by `injectErrorDialog`, strictly by its own PID. */
export function endInjectedDialog(pid: number): void {
  spawnSync("taskkill", ["/pid", String(pid), "/T", "/F"], { stdio: "pipe" });
}

/** The newest `.ask-entry` whose question matches `substring` (case-insensitive), if any. */
export async function findAskEntry(substring: string) {
  const needle = substring.toLowerCase();
  const entries = await $$("#ask-list .ask-entry");
  for (const entry of entries) {
    const q = (await entry.$(".ask-q").getText()).toLowerCase();
    if (q.includes(needle)) return entry;
  }
  return undefined;
}
