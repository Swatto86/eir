/**
 * "Investigate & fix" runs a real analysis cycle (through the fake CLI) for
 * a user-described problem and records it in Ask Eir as
 * "Investigate & fix: <description>" — see eir-svc/src/main.rs's
 * `question: format!("Investigate & fix: {description}")`.
 */
import { strict as assert } from "node:assert";

import { ANALYSIS_MARKER } from "../fixtures/markers.mjs";
import { findAskEntry, showView } from "./support.ts";

const DESCRIPTION = "e2e-suite investigate fixture — nothing is actually wrong";

describe("Investigate & fix", () => {
  it("records an Investigate & fix entry with the fake CLI's analysis", async () => {
    await showView("ask");
    const input = await $("#ask-input");
    await input.waitForExist({ timeout: 15_000 });
    await input.setValue(DESCRIPTION);

    const button = await $("#ask-investigate");
    await button.waitForEnabled({ timeout: 15_000 });
    await button.click();

    const entry = await browser.waitUntil(
      async () => findAskEntry(`Investigate & fix: ${DESCRIPTION}`),
      {
        timeout: 90_000,
        timeoutMsg: "no \"Investigate & fix\" Ask entry ever appeared",
      },
    );

    const answer = await entry.$(".ask-a").getText();
    assert.ok(answer.includes(ANALYSIS_MARKER), `answer missing the fake analysis: ${answer}`);
    assert.match(
      answer,
      /Eir found nothing it can safely fix automatically for this\.$/,
      answer,
    );
  });
});
