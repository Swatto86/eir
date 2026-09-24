/**
 * "Watch on-screen errors" is saved to config.toml and survives a real
 * restart of both EirSvc (config.toml is only read at startup) and the UI
 * (settings are only pushed to a freshly-connected tray).
 */
import { strict as assert } from "node:assert";
import fs from "node:fs";
import path from "node:path";

import { restartService } from "../service.ts";
import { getRun } from "../wdio.conf.ts";
import { showView, text } from "./support.ts";

function configText(): string {
  const run = getRun();
  return fs.readFileSync(path.join(run.stateRoot, "config.toml"), "utf8");
}

describe("settings persistence", () => {
  it("saves \"Watch on-screen errors\" and keeps it after a service+UI restart", async () => {
    await showView("settings");
    const checkbox = await $("#set-watch-screen");
    await checkbox.waitForEnabled({ timeout: 30_000 });

    const before = await checkbox.isSelected();
    const target = !before;
    await checkbox.click();
    assert.equal(await checkbox.isSelected(), target);

    const saveBtn = await $("#set-save");
    await saveBtn.waitForEnabled({ timeout: 5_000 });
    await saveBtn.click();

    const status = await $("#set-status");
    await browser.waitUntil(
      async () => {
        const t = await status.getText();
        return /applied/i.test(t) || /^failed/i.test(t);
      },
      { timeout: 15_000, timeoutMsg: "settings save never resolved" },
    );
    assert.match(await status.getText(), /applied/i, "settings save failed");

    // config.toml is only ever written by the service, on its own state
    // root — proof this reached disk, not just the in-memory UI state.
    await browser.waitUntil(
      () => new RegExp(`watch_screen_errors\\s*=\\s*${target}`).test(configText()),
      { timeout: 10_000, timeoutMsg: "config.toml never recorded the new value" },
    );

    await restartService(getRun());
    await browser.reloadSession();

    await $(".brand h1").waitForExist({ timeout: 30_000 });
    await showView("settings");
    const checkboxAfter = await $("#set-watch-screen");
    await checkboxAfter.waitForEnabled({ timeout: 30_000 });
    assert.equal(
      await checkboxAfter.isSelected(),
      target,
      `"Watch on-screen errors" did not persist across the restart (ai-now: "${await text("#ai-now-text")}")`,
    );
  });
});
