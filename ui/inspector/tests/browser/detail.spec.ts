import type { Download } from "@playwright/test";
import { versionedRecord } from "../../src/lib/test-fixtures";
import { expect, snapshot, test, upsert } from "./fixture";

async function downloadedJson(download: Download): Promise<Record<string, unknown>> {
  const stream = await download.createReadStream();
  expect(stream).not.toBeNull();
  const chunks: Buffer[] = [];
  for await (const chunk of stream!) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
}

test("moves a deep link from its named loading target to versioned ready detail", async ({
  inspector
}) => {
  const item = versionedRecord("deep-link-ready", 7, { status: 207 });
  let release: ((value: { json: unknown }) => void) | undefined;
  const response = new Promise<{ json: unknown }>((resolve) => {
    release = resolve;
  });
  inspector.setDetail(item.record_id, () => response);

  await inspector.open(item.record_id);
  const detail = inspector.page.locator(".detail-pane");
  await expect(detail).toHaveAttribute("data-detail-state", "loading");
  await expect(detail.locator(".detail-record-id")).toHaveText(item.record_id);
  await expect(detail.locator(".empty-detail")).toContainText(item.record_id);
  await expect(detail.locator(".detail-card")).toHaveCount(0);

  release!({ json: item });
  await expect(detail).toHaveAttribute("data-detail-state", "detached-ready");
  await expect(detail.locator(".detail-revision")).toHaveText("revision 7");
  await inspector.openSource();
  await inspector.emit(snapshot(1, [item]));
  await expect(detail).toHaveAttribute("data-detail-state", "ready");
  await expect(detail.locator(".detail-record-id")).toHaveText(item.record_id);
  await expect(detail.locator(".detail-revision")).toHaveText("revision 7");
});

test("shows the requested ID in a not-retained deep-link error", async ({ inspector }) => {
  const recordId = "deep-link-not-retained";
  inspector.expectDetailError(recordId, 404);
  inspector.setDetail(recordId, { status: 404, json: { error: "not retained" } });

  await inspector.open(recordId);
  const detail = inspector.page.locator(".detail-pane");
  await expect(detail).toHaveAttribute("data-detail-state", "error");
  await expect(detail.locator(".detail-record-id")).toHaveText(recordId);
  await expect(detail.locator(".error-text")).toHaveText(`record ${recordId} is not retained`);
  await expect(detail.locator(".detail-actions")).toHaveCount(0);
  await expect(detail.locator("pre")).toHaveCount(0);
});

test("suppresses a delayed older lookup when a newer selection is loading", async ({ inspector }) => {
  const older = versionedRecord("older-selection", 9, { status: 509 });
  const newer = versionedRecord("newer-selection", 4, { status: 204 });
  let releaseOlder: ((value: { json: unknown }) => void) | undefined;
  let releaseNewer: ((value: { json: unknown }) => void) | undefined;
  const olderResponse = new Promise<{ json: unknown }>((resolve) => {
    releaseOlder = resolve;
  });
  const newerResponse = new Promise<{ json: unknown }>((resolve) => {
    releaseNewer = resolve;
  });
  inspector.setDetail(older.record_id, () => olderResponse);
  inspector.setDetail(newer.record_id, () => newerResponse);

  await inspector.open(older.record_id);
  const detail = inspector.page.locator(".detail-pane");
  await expect(detail).toHaveAttribute("data-detail-state", "loading");
  await inspector.page.evaluate((recordId) => {
    window.location.hash = encodeURIComponent(recordId);
  }, newer.record_id);
  await expect(detail).toHaveAttribute("data-detail-state", "loading");
  await expect(detail.locator(".detail-record-id")).toHaveText(newer.record_id);
  await expect(detail.locator(".detail-card")).toHaveCount(0);
  await expect(detail).not.toContainText(older.record_id);

  releaseNewer!({ json: newer });
  await expect(detail).toHaveAttribute("data-detail-state", "detached-ready");
  await expect(detail.locator(".detail-record-id")).toHaveText(newer.record_id);
  await expect(detail.locator(".detail-revision")).toHaveText("revision 4");
  releaseOlder!({ json: older });
  await expect(detail.locator(".detail-record-id")).toHaveText(newer.record_id);
  await expect(detail.locator(".detail-revision")).toHaveText("revision 4");
  await expect(detail.locator(".status-value")).toHaveText("204");
  await expect(detail.locator("pre")).toContainText('"record_id": "newer-selection"');
  await expect(detail.locator("pre")).not.toContainText("older-selection");
});

test("keeps a pinned detached revision outside the bounded client window", async ({ inspector }) => {
  const pinned = versionedRecord("pinned-outside-window", 11, { status: 211 });
  inspector.setDetail(pinned.record_id, { json: pinned });
  await inspector.open(pinned.record_id);

  const detail = inspector.page.locator(".detail-pane");
  await expect(detail).toHaveAttribute("data-detail-state", "detached-ready");
  await expect(detail.locator(".detail-record-id")).toHaveText(pinned.record_id);
  await expect(detail.locator(".detail-revision")).toHaveText("revision 11");
  await expect(
    inspector.page.getByLabel("Detached: pinned revision is outside the current live table window")
  ).toBeVisible();

  const windowItems = Array.from({ length: 1_000 }, (_, index) =>
    versionedRecord(`window-${String(index).padStart(4, "0")}`, 1, {
      started_at_unix_ms: 1_700_000_000_000 + index
    })
  );
  await inspector.openSource();
  await inspector.emit(snapshot(1, windowItems));
  await expect(detail).toHaveAttribute("data-detail-state", "detached-ready");
  await expect(detail.locator(".detail-record-id")).toHaveText(pinned.record_id);
  await expect(detail.locator(".detail-revision")).toHaveText("revision 11");
  await expect(
    inspector.page.getByRole("row", { name: `Inspect request ${pinned.record_id}` })
  ).toHaveCount(0);
  await expect(inspector.page.locator(".status-strip")).toContainText("1,000 loaded");
});

test("keeps row, heading, fields, raw JSON, copy, and download on one record revision", async ({
  inspector
}) => {
  const recordId = `consistent-${"x".repeat(96)}`;
  const selected = versionedRecord(recordId, 5, {
    route: "consistent-route",
    status: 208,
    started_at_unix_ms: 1_700_000_000_500
  });
  const other = versionedRecord("other-record", 2, {
    route: "other-route",
    status: 202,
    started_at_unix_ms: 1_700_000_000_200
  });
  await inspector.page.context().grantPermissions(["clipboard-read", "clipboard-write"], {
    origin: "http://127.0.0.1:4179"
  });
  await inspector.page.setViewportSize({ width: 1280, height: 844 });
  await inspector.open();
  const detail = inspector.page.locator(".detail-pane");
  await expect(detail).toHaveAttribute("data-detail-state", "none");
  await expect(detail.locator(".empty-detail")).toContainText("Select a record");
  await inspector.openSource();
  await inspector.emit(snapshot(1, [other, selected]));

  const row = inspector.page.getByRole("row", { name: `Inspect request ${recordId}` });
  await row.click();
  await expect(row).toHaveAttribute("aria-selected", "true");
  await expect(detail).toHaveAttribute("data-detail-state", "ready");
  await expect(detail.locator(".detail-record-id")).toHaveText(recordId);
  await expect(detail.locator(".detail-revision")).toHaveText("revision 5");
  await expect(detail.locator(".status-value")).toHaveText("208");
  await expect(detail).toContainText("consistent-route");

  const raw = JSON.parse((await detail.locator("pre").textContent()) ?? "{}") as Record<
    string,
    unknown
  >;
  expect(raw.record_id).toBe(recordId);
  expect(raw.status).toBe(208);
  expect(raw.route).toBe("consistent-route");
  expect(raw.revision).toBeUndefined();
  expect(raw.record).toBeUndefined();

  await inspector.page.getByRole("button", { name: "copy JSON" }).click();
  await expect(inspector.page.locator(".notices")).toHaveText("record JSON copied");
  const copied = JSON.parse(await inspector.page.evaluate(() => navigator.clipboard.readText())) as Record<
    string,
    unknown
  >;
  expect(copied).toEqual(raw);

  const downloadPromise = inspector.page.waitForEvent("download");
  await inspector.page.getByRole("button", { name: "download JSON" }).click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe(`onair-request-${recordId}.json`);
  expect(await downloadedJson(download)).toEqual(raw);
  await expect(inspector.page.locator(".notices")).toHaveText("record JSON downloaded");

  const desktop = await inspector.page.locator(".workspace").evaluate((workspace) => {
    const table = workspace.querySelector<HTMLElement>(".table-panel")!.getBoundingClientRect();
    const detail = workspace.querySelector<HTMLElement>(".detail-pane")!.getBoundingClientRect();
    return { tableRight: table.right, detailLeft: detail.left };
  });
  expect(desktop.detailLeft).toBeGreaterThanOrEqual(desktop.tableRight);
  expect(
    await inspector.page.evaluate(
      () => document.documentElement.scrollWidth <= document.documentElement.clientWidth
    )
  ).toBe(true);

  await inspector.page.setViewportSize({ width: 390, height: 844 });
  const narrow = await inspector.page.locator(".workspace").evaluate((workspace) => {
    const table = workspace.querySelector<HTMLElement>(".table-panel")!.getBoundingClientRect();
    const detail = workspace.querySelector<HTMLElement>(".detail-pane")!.getBoundingClientRect();
    return { tableBottom: table.bottom, detailTop: detail.top };
  });
  expect(narrow.detailTop).toBeGreaterThanOrEqual(narrow.tableBottom);
  expect(
    await inspector.page.evaluate(
      () => document.documentElement.scrollWidth <= document.documentElement.clientWidth
    )
  ).toBe(true);
  await expect(detail.locator(".detail-record-id")).toHaveText(recordId);
  await expect(detail.locator(".detail-revision")).toHaveText("revision 5");
});

test("resets attempt expansion when the selected revision changes", async ({ inspector }) => {
  const attempt = {
    attempt: 1,
    backend: "attempt-backend",
    backend_target: "attempt-target",
    status: 502,
    outcome: "upstream_non_success",
    started_us: 100,
    ended_us: 900,
    elapsed_us: 800,
    elapsed_ms: 0
  };
  const first = versionedRecord("attempt-reset", 1, { backend_attempts: [attempt] });
  const second = versionedRecord("attempt-reset", 2, {
    backend_attempts: [{ ...attempt, status: 503, ended_us: 1_000, elapsed_us: 900 }]
  });

  await inspector.open();
  await inspector.openSource();
  await inspector.emit(snapshot(1, [first]));

  const detail = inspector.page.locator(".detail-pane");
  const expansion = detail.getByRole("button", { name: "details" });
  await expansion.click();
  await expect(detail.getByRole("button", { name: "hide details" })).toBeVisible();
  await expect(detail.locator(".attempt-details")).toHaveCount(1);

  await inspector.emit(upsert(2, second));
  await expect(detail.locator(".detail-revision")).toHaveText("revision 2");
  await expect(detail.getByRole("button", { name: "details" })).toBeVisible();
  await expect(detail.locator(".attempt-details")).toHaveCount(0);
});
