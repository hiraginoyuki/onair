import { versionedRecord } from "../../src/lib/test-fixtures";
import { expect, reset, snapshot, test, upsert } from "./fixture";

const initial = versionedRecord("initial", 1, {
  started_at_unix_ms: 1_700_000_000_100
});

test("presents connecting, transport reconnect, reset, and warning without duplicate notices", async ({
  inspector
}) => {
  inspector.setDetail(initial.record_id, { json: initial });
  await inspector.open();
  const indicator = inspector.page.locator(".stream-indicator");
  await expect(indicator).toHaveText("connecting");
  await expect(indicator).toHaveAttribute("data-state", "connecting");
  await expect(indicator).toHaveAttribute("role", "status");
  await expect(indicator).toHaveAttribute("aria-live", "polite");
  await expect(indicator).toHaveAttribute("aria-atomic", "true");

  await inspector.openSource();
  await inspector.emit(snapshot(1, [initial]));
  await expect(indicator).toHaveText("live");
  await expect(inspector.page.locator(".status-strip")).toContainText("stream live");

  await inspector.errorSource();
  await expect(indicator).toHaveText("reconnecting");
  await expect(indicator).not.toContainText("live");
  expect(await inspector.sourceCount()).toBe(1);
  await inspector.openSource();
  await expect(indicator).toHaveText("live");

  await inspector.emit(reset(2));
  await expect(indicator).toHaveText("resetting projection");
  await expect(inspector.page.locator("[role=status]")).toHaveCount(1);
  await expect(inspector.page.locator(".notices")).toHaveCount(0);
  await inspector.emit(snapshot(2, [versionedRecord("replacement", 1)]));
  await expect(indicator).toHaveText("live");

  await inspector.emitRaw("record_upsert", "not-json");
  await expect(indicator).toHaveText("stream warning");
  await expect(indicator).not.toContainText("live");
  await expect(inspector.page.locator(".status-strip")).not.toContainText("stream live");
  await expect(inspector.page.locator(".notices")).toHaveCount(0);
  await inspector.emit(upsert(3, versionedRecord("replacement", 2)));
  await expect(indicator).toHaveText("live");
});

test("shows paused stream health through reconnect and controlled replacement", async ({
  inspector
}) => {
  inspector.setDetail(initial.record_id, { json: initial });
  await inspector.open();
  await inspector.openSource();
  await inspector.emit(snapshot(1, [initial]));
  const indicator = inspector.page.locator(".stream-indicator");

  await inspector.page.getByRole("button", { name: "Pause table updates" }).click();
  await expect(indicator).toHaveText("paused · stream live");
  await inspector.errorSource();
  await expect(indicator).toHaveText("reconnecting");
  await inspector.openSource();
  await expect(indicator).toHaveText("paused · stream live");

  await inspector.failNextMapClear();
  await inspector.emit(reset(2));
  await expect(indicator).toHaveText("recovering");
  await inspector.expectSourceCount(2);
  expect(await inspector.sourceClosed(0)).toBe(true);

  await inspector.emit(snapshot(3, [versionedRecord("stale", 1)]), 0);
  await expect(inspector.page.getByRole("row", { name: "Inspect request stale" })).toHaveCount(0);
  await inspector.openSource(1);
  await inspector.emit(snapshot(3, [versionedRecord("current", 1)]), 1);
  await expect(indicator).toHaveText("paused · stream live");
  await expect(inspector.page.locator("[role=status]")).toHaveCount(1);
  await expect(inspector.page.locator(".notices")).toHaveCount(0);

  await inspector.page.getByRole("button", { name: "Resume table updates" }).click();
  await expect(inspector.page.getByRole("row", { name: "Inspect request current" })).toBeVisible();
  await expect(inspector.page.getByRole("row", { name: "Inspect request initial" })).toHaveCount(0);
});

test("fails closed after the replacement budget and requires manual refresh", async ({
  inspector
}) => {
  inspector.setDetail(initial.record_id, { json: initial });
  await inspector.open();
  await inspector.openSource();
  await inspector.emit(snapshot(1, [initial]));
  const indicator = inspector.page.locator(".stream-indicator");

  await inspector.failNextMapClear();
  await inspector.emit(reset(2));
  await inspector.expectSourceCount(2);
  await inspector.openSource(1);
  await inspector.failNextMapClear();
  await inspector.emit(snapshot(3, [versionedRecord("replacement", 1)]), 1);

  await expect(indicator).toHaveText("stream error · refresh required");
  await expect(indicator).toHaveAttribute("data-state", "failed");
  await expect(indicator).not.toContainText("live");
  await expect(inspector.page.locator(".status-strip")).not.toContainText("stream live");
  await expect(inspector.page.locator(".notices")).toHaveCount(0);

  await inspector.page.getByRole("button", { name: "refresh" }).click();
  await inspector.expectSourceCount(3);
  await expect(indicator).toHaveText("connecting");
  await inspector.openSource(2);
  await inspector.emit(snapshot(4, [versionedRecord("manual", 1)]), 2);
  await expect(indicator).toHaveText("live");
});
