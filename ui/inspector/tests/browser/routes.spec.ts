import { versionedRecord } from "../../src/lib/test-fixtures";
import { expect, snapshot, test } from "./fixture";

for (const path of ["/_onair/inspector", "/_onair/inspector-next"] as const) {
  test(`runs the Svelte artifact at ${path} on desktop and narrow viewports`, async ({
    inspector
  }) => {
    const item = versionedRecord(`route-${path.endsWith("-next") ? "alias" : "primary"}`, 1, {
      started_at_unix_ms: 1_700_000_000_100
    });

    await inspector.page.setViewportSize({ width: 1280, height: 844 });
    await inspector.openAt(path);
    expect(new URL(inspector.page.url()).pathname).toBe(path);
    expect(await inspector.sourceUrl()).toMatch(
      /\/_onair\/inspector-next\/events\?snapshot_limit=1000$/
    );
    await inspector.openSource();
    await inspector.emit(snapshot(1, [item]));

    await expect(
      inspector.page.getByRole("row", { name: `Inspect request ${item.record_id}` })
    ).toBeVisible();
    await expect(inspector.page.locator(".detail-record-id")).toHaveText(item.record_id);
    expect(
      await inspector.page.evaluate(
        () => document.documentElement.scrollWidth <= document.documentElement.clientWidth
      )
    ).toBe(true);

    await inspector.page.setViewportSize({ width: 390, height: 844 });
    const stacked = await inspector.page.locator(".workspace").evaluate((workspace) => {
      const table = workspace.querySelector<HTMLElement>(".table-panel")!.getBoundingClientRect();
      const detail = workspace.querySelector<HTMLElement>(".detail-pane")!.getBoundingClientRect();
      return { tableBottom: table.bottom, detailTop: detail.top };
    });
    expect(stacked.detailTop).toBeGreaterThanOrEqual(stacked.tableBottom);
    expect(
      await inspector.page.evaluate(
        () => document.documentElement.scrollWidth <= document.documentElement.clientWidth
      )
    ).toBe(true);
  });
}
