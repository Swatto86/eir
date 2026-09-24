/**
 * A real on-screen error dialog is picked up by eir-ui's screen watcher
 * (2s poll) and shows up in "What Eir noticed" with Explain/Fix actions —
 * independent of any AI call, since the tray reports it straight to the
 * service's signal feed (see eir-svc/src/main.rs's ReportScreenError arm).
 */
import { strict as assert } from "node:assert";

import { endInjectedDialog, injectErrorDialog } from "./support.ts";

describe("what Eir noticed", () => {
  let dialogPid: number | undefined;

  afterEach(() => {
    if (dialogPid) {
      endInjectedDialog(dialogPid);
      dialogPid = undefined;
    }
  });

  it("shows a real error dialog with Explain and Fix actions", async () => {
    dialogPid = injectErrorDialog(
      "Noticed Feed Fixture Error",
      "The noticed-feed fixture could not start. (eir-e2e-suite)",
    );

    await $("#noticed-card").waitForDisplayed({ timeout: 30_000 });
    const item = await browser.waitUntil(
      async () => {
        const items = await $$("#noticed-list .act-item");
        for (const candidate of items) {
          if ((await candidate.getText()).includes("Noticed Feed Fixture Error")) return candidate;
        }
        return undefined;
      },
      {
        timeout: 30_000,
        timeoutMsg: "the injected error dialog never appeared in \"What Eir noticed\"",
      },
    );

    const itemText = await item.getText();
    assert.match(itemText, /On screen/i);
    assert.match(itemText, /could not start/i);

    const explainBtn = await item.$("[data-explain]");
    const fixBtn = await item.$("[data-fix]");
    assert.ok(await explainBtn.isExisting(), "no Explain action on the noticed item");
    assert.ok(await fixBtn.isExisting(), "no Fix action on the noticed item");
  });
});
