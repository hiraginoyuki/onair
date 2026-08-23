import { versionedRecord } from "../../src/lib/test-fixtures";
import { expect, remove, reset, snapshot, test, upsert } from "./fixture";

test("keeps the visible projection frozen while canonical ingestion continues", async ({
  inspector
}) => {
  const initial = versionedRecord("selected", 1, {
    started_at_unix_ms: 1_700_000_000_200,
    status: 200
  });
  const removed = versionedRecord("removed", 1, {
    started_at_unix_ms: 1_700_000_000_100,
    status: 201
  });
  const updated = versionedRecord("selected", 2, {
    started_at_unix_ms: initial.record.started_at_unix_ms,
    status: 202
  });
  const added = versionedRecord("added", 1, {
    started_at_unix_ms: 1_700_000_000_300,
    status: 203
  });
  const authoritative = versionedRecord("selected", 3, {
    started_at_unix_ms: initial.record.started_at_unix_ms,
    status: 204
  });
  const authoritativeAdded = versionedRecord("added", 2, {
    started_at_unix_ms: added.record.started_at_unix_ms,
    status: 205
  });

  inspector.setRetainedCount(2);
  await inspector.open();
  expect(await inspector.sourceUrl()).toMatch(
    /\/_onair\/inspector-next\/events\?snapshot_limit=1000$/
  );
  await inspector.openSource();
  await inspector.emit(snapshot(1, [removed, initial]));

  const selectedRow = inspector.page.getByRole("row", { name: "Inspect request selected" });
  await expect(selectedRow).toContainText("200");
  await expect(inspector.page.locator(".detail-record-id")).toHaveText("selected");
  await expect(inspector.page.locator(".detail-revision")).toHaveText("revision 1");

  const pause = inspector.page.getByRole("button", { name: "Pause table updates" });
  await expect(pause).toHaveAttribute("aria-pressed", "false");
  await pause.click();
  await expect(inspector.page.getByRole("button", { name: "Resume table updates" })).toHaveAttribute(
    "aria-pressed",
    "true"
  );
  await expect(inspector.page.locator(".table-panel")).toHaveAttribute("data-view-state", "frozen");
  await expect(inspector.page.locator(".frozen-view-indicator")).toHaveText("view frozen");
  await expect(inspector.page.locator(".status-strip")).toContainText("stream live");

  await inspector.emit(upsert(2, updated));
  await inspector.emit(upsert(3, added));
  await inspector.emit(remove(4, removed.record_id, 2));
  await inspector.emit(reset(5));
  await inspector.emit(snapshot(5, [authoritative, authoritativeAdded]));

  await expect(inspector.page.locator(".status-strip")).toContainText("5 updates while paused");
  await expect(selectedRow).toContainText("200");
  await expect(inspector.page.getByRole("row", { name: "Inspect request removed" })).toBeVisible();
  await expect(inspector.page.getByRole("row", { name: "Inspect request added" })).toHaveCount(0);
  await expect(inspector.page.locator(".detail-pane")).toContainText("200");
  expect(await inspector.sourceCount()).toBe(1);

  await inspector.page.getByRole("button", { name: "Resume table updates" }).click();
  await expect(inspector.page.locator(".table-panel")).toHaveAttribute("data-view-state", "live");
  await expect(inspector.page.getByRole("row", { name: "Inspect request selected" })).toContainText(
    "204"
  );
  await expect(inspector.page.getByRole("row", { name: "Inspect request added" })).toContainText(
    "205"
  );
  await expect(inspector.page.getByRole("row", { name: "Inspect request removed" })).toHaveCount(0);
  await expect(inspector.page.locator(".detail-pane")).toContainText("204");
  await expect(inspector.page.locator(".notices")).toHaveText("live view resumed");
  await expect(inspector.page.locator(".notices .notice")).toHaveCount(1);
  expect(await inspector.sourceCount()).toBe(1);
});
