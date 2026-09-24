/**
 * Pressing "Explain" on a noticed item asks Eir about it and the answer —
 * from the fake CLI, round-tripped through the real Ask pipeline — lands in
 * the Ask Eir history.
 */
import { strict as assert } from "node:assert";

import { ASK_MARKER } from "../fixtures/markers.mjs";
import { endInjectedDialog, findAskEntry, injectErrorDialog } from "./support.ts";

describe("Explain", () => {
  let dialogPid: number | undefined;

  after(() => {
    if (dialogPid) endInjectedDialog(dialogPid);
  });

  it("answers from the fake CLI and records it in Ask Eir", async () => {
    dialogPid = injectErrorDialog(
      "Explain Fixture Error",
      "The explain fixture could not start. (eir-e2e-suite)",
    );

    const item = await browser.waitUntil(
      async () => {
        const items = await $$("#noticed-list .act-item");
        for (const candidate of items) {
          if ((await candidate.getText()).includes("Explain Fixture Error")) return candidate;
        }
        return undefined;
      },
      { timeout: 30_000, timeoutMsg: "the explain fixture's dialog never appeared as noticed" },
    );

    await (await item.$("[data-explain]")).click();

    const entry = await browser.waitUntil(
      async () => findAskEntry("Explain this"),
      {
        timeout: 60_000,
        timeoutMsg: "no Ask entry for the Explain click ever appeared",
      },
    );

    const answer = await entry.$(".ask-a").getText();
    assert.equal(answer.trim(), ASK_MARKER);
  });
});
