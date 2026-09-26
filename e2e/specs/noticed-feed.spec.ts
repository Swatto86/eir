/**
 * A real on-screen error dialog is picked up by eir-ui's screen watcher
 * (2s poll) and shows up in "What Eir noticed" with Explain/Fix actions —
 * independent of any AI call, since the tray reports it straight to the
 * service's signal feed (see eir-svc/src/main.rs's ReportScreenError arm).
 * Dismiss removes one item and Clear empties the list, through the real
 * service command; a dialog still on screen is not listed again.
 */
import { strict as assert } from "node:assert";

import { endInjectedDialog, injectErrorDialog } from "./support.ts";

async function noticedItem(title: string): Promise<WebdriverIO.Element | undefined> {
  for (const candidate of await $$("#noticed-list .act-item")) {
    if ((await candidate.getText()).includes(title)) return candidate;
  }
  return undefined;
}

async function waitForNoticed(title: string): Promise<WebdriverIO.Element> {
  return browser.waitUntil(async () => noticedItem(title), {
    timeout: 30_000,
    timeoutMsg: `the injected "${title}" dialog never appeared in "What Eir noticed"`,
  });
}

async function waitUntilGone(title: string, action: string): Promise<void> {
  await browser.waitUntil(async () => (await noticedItem(title)) === undefined, {
    timeout: 15_000,
    timeoutMsg: `"${title}" was still listed after ${action}`,
  });
}

describe("what Eir noticed", () => {
  const dialogPids: number[] = [];

  afterEach(() => {
    for (const pid of dialogPids.splice(0)) endInjectedDialog(pid);
  });

  it("shows a real error dialog with Explain, Fix and Dismiss actions", async () => {
    dialogPids.push(
      injectErrorDialog(
        "Noticed Feed Fixture Error",
        "The noticed-feed fixture could not start. (eir-e2e-suite)",
      ),
    );

    await $("#noticed-card").waitForDisplayed({ timeout: 30_000 });
    const item = await waitForNoticed("Noticed Feed Fixture Error");

    const itemText = await item.getText();
    assert.match(itemText, /On screen/i);
    assert.match(itemText, /could not start/i);

    assert.ok(await (await item.$("[data-explain]")).isExisting(), "no Explain action");
    assert.ok(await (await item.$("[data-fix]")).isExisting(), "no Fix action");

    // Dismiss removes this item while its dialog is still on screen, and the
    // screen watcher does not list the same window again.
    await (await item.$("[data-dismiss]")).click();
    await waitUntilGone("Noticed Feed Fixture Error", "Dismiss");
    await browser.pause(5_000);
    assert.equal(
      await noticedItem("Noticed Feed Fixture Error"),
      undefined,
      "a dismissed item came back while its dialog was still open",
    );
  });

  it("clears the whole list with Clear", async () => {
    dialogPids.push(
      injectErrorDialog(
        "Noticed Clear Fixture Error",
        "The clear fixture could not start. (eir-e2e-suite)",
      ),
    );
    await waitForNoticed("Noticed Clear Fixture Error");

    const clear = await $("#clear-noticed");
    assert.ok(await clear.isDisplayed(), "Clear is not offered on the noticed card");
    await clear.click();
    await waitUntilGone("Noticed Clear Fixture Error", "Clear");
  });
});
