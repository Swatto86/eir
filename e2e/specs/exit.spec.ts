/**
 * A clean exit: there is no DOM-reachable Quit (only the tray context menu's
 * "Quit Eir", which WebDriver can't reach — see eir-ui/src/main.rs's
 * `"quit" => app.exit(0)`), so this spec ends both processes the way the
 * harness itself does — by PID, never by name — and checks what actually
 * matters: the processes are really gone, and the state files they wrote
 * are still there and intact afterwards.
 */
import { strict as assert } from "node:assert";
import fs from "node:fs";
import path from "node:path";

import { killPid, stopService } from "../service.ts";
import { application, getRun, pidsByPath } from "../wdio.conf.ts";

async function waitGone(exePath: string, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && pidsByPath(exePath).length > 0) {
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  assert.deepEqual(pidsByPath(exePath), [], `${exePath} is still running`);
}

describe("quitting", () => {
  it("ends both processes and leaves the state files intact", async () => {
    await $(".brand h1").waitForExist({ timeout: 30_000 });

    const uiPids = pidsByPath(application);
    assert.ok(uiPids.length > 0, "the e2e UI process was not found by path");
    for (const pid of uiPids) killPid(pid);
    await waitGone(application);

    const run = getRun();
    await stopService(run);
    await waitGone(run.svcExe);

    for (const file of ["config.toml", "policy.toml", "eir.db"]) {
      const full = path.join(run.stateRoot, file);
      assert.ok(fs.existsSync(full), `${file} is missing after exit`);
      assert.ok(fs.statSync(full).size > 0, `${file} is empty after exit`);
    }
    assert.match(
      fs.readFileSync(path.join(run.stateRoot, "config.toml"), "utf8"),
      /\[api]/,
      "config.toml no longer parses as the expected config",
    );

    // A fresh session so the runner's own teardown (and onComplete's
    // tauri-driver kill) has a live app to close, same as the reference
    // harnesses' exit specs.
    await browser.reloadSession();
    await $(".brand h1").waitForExist({ timeout: 30_000 });
  });
});
