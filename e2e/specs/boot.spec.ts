/**
 * Boot to the main window on the isolated portable pipe, and see the fake
 * AI's analysis land on the dashboard — proof that EIR_PORTABLE/
 * EIR_PORTABLE_PIPE really reached eir.exe (not the installed pipe) and that
 * the fake CLI's response made it all the way from cmd.exe through the
 * service to the webview.
 */
import { strict as assert } from "node:assert";

import { ANALYSIS_MARKER } from "../fixtures/markers.mjs";
import { invoke, logWebviewDiagnostics, text, waitForAiNow } from "./support.ts";

describe("Eir boots on the isolated portable pipe", () => {
  it("shows the dashboard and reports itself as portable", async () => {
    await logWebviewDiagnostics();
    await $(".brand h1").waitForExist({ timeout: 30_000 });
    assert.equal((await text(".brand h1")).trim(), "Eir");

    const portable = await invoke<{ ok: boolean; value?: boolean }>("is_portable");
    assert.ok(portable.ok, `is_portable failed: ${JSON.stringify(portable)}`);
    assert.equal(portable.value, true, "the e2e UI is not on the isolated portable pipe");
  });

  it("runs a decision cycle through the fake CLI and shows its analysis", async () => {
    // require_tray=true means the cycle runs as soon as the tray (this UI)
    // connects, not on the 600s decision_interval_secs tick — but the first
    // cycle plus the fake CLI round-trip still takes a few seconds.
    await waitForAiNow(ANALYSIS_MARKER, 90_000);
  });
});
