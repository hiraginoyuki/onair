import type { Locator, Page } from "@playwright/test";
import { versionedRecord } from "../../src/lib/test-fixtures";
import { expect, snapshot, test } from "./fixture";

function headerFor(page: Page, label: string): Locator {
  return page
    .getByRole("columnheader")
    .filter({ has: page.getByRole("button", { name: `Sort by ${label}` }) });
}

async function rowOrder(page: Page): Promise<string[]> {
  return page.locator("tbody tr[aria-label]").evaluateAll((rows) =>
    rows.map((row) => row.getAttribute("aria-label")?.replace("Inspect request ", "") ?? "")
  );
}

async function widthOf(locator: Locator): Promise<number> {
  return locator.evaluate((element) => element.getBoundingClientRect().width);
}

test("keeps sorting, row focus, and selected styling aligned", async ({ inspector }) => {
  const records = [
    versionedRecord("alpha", 1, {
      started_at_unix_ms: 1_700_000_000_100,
      route: "charlie",
      status: 503
    }),
    versionedRecord("bravo", 1, {
      started_at_unix_ms: 1_700_000_000_200,
      route: "alpha",
      status: 201
    }),
    versionedRecord("charlie", 1, {
      started_at_unix_ms: 1_700_000_000_300,
      route: "bravo",
      status: 404
    })
  ];

  await inspector.open();
  await inspector.openSource();
  await inspector.emit(snapshot(1, records));
  expect(await rowOrder(inspector.page)).toEqual(["charlie", "bravo", "alpha"]);

  const httpHeader = headerFor(inspector.page, "HTTP");
  const httpSort = inspector.page.getByRole("button", { name: "Sort by HTTP" });
  await httpSort.click();
  await expect(httpHeader).toHaveAttribute("aria-sort", "ascending");
  await expect(httpHeader.locator(".sort-indicator")).toHaveText("↑");
  await expect(httpHeader.locator(".sort-indicator")).toHaveAttribute("aria-hidden", "true");
  expect(await rowOrder(inspector.page)).toEqual(["bravo", "charlie", "alpha"]);

  const arrowPlacement = await httpHeader.evaluate((header) => {
    const label = header.querySelector<HTMLElement>(".sort-label")!.getBoundingClientRect();
    const arrow = header.querySelector<HTMLElement>(".sort-indicator")!.getBoundingClientRect();
    return { labelRight: label.right, arrowLeft: arrow.left };
  });
  expect(arrowPlacement.arrowLeft).toBeGreaterThanOrEqual(arrowPlacement.labelRight);

  const routeHeader = headerFor(inspector.page, "route");
  const routeSort = inspector.page.getByRole("button", { name: "Sort by route" });
  await routeSort.press("Enter");
  await expect(routeHeader).toHaveAttribute("aria-sort", "ascending");
  expect(await rowOrder(inspector.page)).toEqual(["bravo", "charlie", "alpha"]);
  await routeSort.press("Space");
  await expect(routeHeader).toHaveAttribute("aria-sort", "descending");
  await expect(routeHeader.locator(".sort-indicator")).toHaveText("↓");
  expect(await rowOrder(inspector.page)).toEqual(["alpha", "charlie", "bravo"]);

  const alpha = inspector.page.getByRole("row", { name: "Inspect request alpha" });
  const alphaCell = alpha.locator("td").first();
  const idleBackground = await alphaCell.evaluate((element) => getComputedStyle(element).backgroundColor);
  await alpha.hover();
  await expect
    .poll(() => alphaCell.evaluate((element) => getComputedStyle(element).backgroundColor))
    .not.toBe(idleBackground);

  await alpha.press("Enter");
  await expect(alpha).toHaveAttribute("aria-selected", "true");
  await expect(alpha).toHaveClass(/selected/);

  const bravo = inspector.page.getByRole("row", { name: "Inspect request bravo" });
  await bravo.press("Space");
  await expect(bravo).toHaveAttribute("aria-selected", "true");
  await expect(alpha).toHaveAttribute("aria-selected", "false");
  expect(
    await bravo.locator("td").first().evaluate((element) => getComputedStyle(element).outlineWidth)
  ).toBe("2px");
});

test("resizes variable columns at exact bounds and persists reversible widths", async ({
  inspector
}) => {
  await inspector.open();

  const timeHeader = headerFor(inspector.page, "time");
  const handle = inspector.page.getByRole("button", { name: "Resize time" });
  await expect(handle).toBeVisible();
  expect(await widthOf(timeHeader)).toBe(164);
  expect(await widthOf(handle)).toBe(40);
  const initialGeometry = await timeHeader.evaluate((header) => {
    const cell = header.getBoundingClientRect();
    const handle = header.querySelector<HTMLElement>(".resize")!.getBoundingClientRect();
    return { reserved: cell.right - handle.left };
  });
  expect(initialGeometry.reserved).toBe(40);

  let box = await handle.boundingBox();
  expect(box).not.toBeNull();
  await inspector.page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await inspector.page.mouse.down();
  await inspector.page.mouse.move(box!.x + 500, box!.y + box!.height / 2);
  await inspector.page.mouse.up();
  expect(await widthOf(timeHeader)).toBe(240);

  box = await handle.boundingBox();
  await inspector.page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await inspector.page.mouse.down();
  await inspector.page.mouse.move(box!.x - 500, box!.y + box!.height / 2);
  await inspector.page.mouse.up();
  expect(await widthOf(timeHeader)).toBe(148);

  await handle.dblclick();
  expect(await widthOf(timeHeader)).toBe(164);
  await handle.press("ArrowRight");
  expect(await widthOf(timeHeader)).toBe(172);

  await inspector.page.reload();
  await inspector.expectSourceCount(1);
  expect(await widthOf(headerFor(inspector.page, "time"))).toBe(172);
  const reloadedHandle = inspector.page.getByRole("button", { name: "Resize time" });
  await reloadedHandle.press("Enter");
  expect(await widthOf(headerFor(inspector.page, "time"))).toBe(164);

  for (let index = 0; index < 20; index += 1) await reloadedHandle.press("ArrowRight");
  expect(await widthOf(headerFor(inspector.page, "time"))).toBe(240);
  for (let index = 0; index < 20; index += 1) await reloadedHandle.press("ArrowLeft");
  expect(await widthOf(headerFor(inspector.page, "time"))).toBe(148);

  await inspector.page.getByRole("button", { name: "reset widths" }).click();
  expect(await widthOf(headerFor(inspector.page, "time"))).toBe(164);
  await expect(inspector.page.getByRole("button", { name: "Resize HTTP" })).toHaveCount(0);
  await expect(inspector.page.getByRole("button", { name: "Resize exposed" })).toHaveCount(0);

  for (const label of ["HTTP", "exposed"]) {
    const header = headerFor(inspector.page, label);
    const sort = inspector.page.getByRole("button", { name: `Sort by ${label}` });
    expect(await widthOf(sort)).toBe(await widthOf(header));
  }
});

test("contains scrolling and virtualization at desktop and narrow widths", async ({ inspector }) => {
  const records = Array.from({ length: 1_000 }, (_, index) =>
    versionedRecord(`record-${String(index).padStart(4, "0")}`, 1, {
      started_at_unix_ms: 1_700_000_000_000 + index,
      route: `route-${index % 7}`,
      status: 200 + (index % 5)
    })
  );

  await inspector.page.setViewportSize({ width: 1280, height: 844 });
  await inspector.open();
  await inspector.openSource();
  await inspector.emit(snapshot(1, records));

  const tableWrap = inspector.page.locator(".table-wrap");
  const scrollGeometry = await tableWrap.evaluate((element) => ({
    clientWidth: element.clientWidth,
    scrollWidth: element.scrollWidth
  }));
  expect(scrollGeometry.scrollWidth).toBeGreaterThan(scrollGeometry.clientWidth);
  const actualScrollLeft = await tableWrap.evaluate((element) => {
    element.scrollLeft = 180;
    element.dispatchEvent(new Event("scroll"));
    return element.scrollLeft;
  });
  expect(actualScrollLeft).toBeGreaterThan(0);
  await expect
    .poll(() =>
      inspector.page.locator(".table-header").evaluate((element) => {
        const transform = getComputedStyle(element).transform;
        return transform === "none" ? 0 : new DOMMatrix(transform).m41;
      })
    )
    .toBe(-actualScrollLeft);

  const rows = inspector.page.locator("tbody tr[aria-label]");
  expect(await rows.count()).toBeLessThanOrEqual(40);
  const rowHeights = await rows.evaluateAll((elements) =>
    elements.map((element) => element.getBoundingClientRect().height)
  );
  expect(new Set(rowHeights)).toEqual(new Set([40]));
  expect(await inspector.page.locator("tbody").evaluate((body) => body.getBoundingClientRect().height)).toBe(
    40_000
  );

  const targetSizes = await inspector.page
    .locator("button:visible, input:visible")
    .evaluateAll((elements) =>
      elements.map((element) => {
        const rect = element.getBoundingClientRect();
        return { name: element.getAttribute("aria-label") ?? element.textContent, width: rect.width, height: rect.height };
      })
    );
  expect(targetSizes.filter((target) => target.width < 40 || target.height < 40)).toEqual([]);
  expect(
    await inspector.page.evaluate(
      () => document.documentElement.scrollWidth <= document.documentElement.clientWidth
    )
  ).toBe(true);

  await inspector.page.setViewportSize({ width: 390, height: 844 });
  expect(
    await inspector.page.evaluate(
      () => document.documentElement.scrollWidth <= document.documentElement.clientWidth
    )
  ).toBe(true);
  const stacked = await inspector.page.locator(".workspace").evaluate((workspace) => {
    const table = workspace.querySelector<HTMLElement>(".table-panel")!.getBoundingClientRect();
    const detail = workspace.querySelector<HTMLElement>(".detail-pane")!.getBoundingClientRect();
    return { tableBottom: table.bottom, detailTop: detail.top };
  });
  expect(stacked.detailTop).toBeGreaterThanOrEqual(stacked.tableBottom);
  const narrowScroll = await tableWrap.evaluate((element) => ({
    clientWidth: element.clientWidth,
    scrollWidth: element.scrollWidth
  }));
  expect(narrowScroll.scrollWidth).toBeGreaterThan(narrowScroll.clientWidth);
});

test("removes row motion when reduced motion is requested", async ({ inspector }) => {
  await inspector.page.emulateMedia({ reducedMotion: "reduce" });
  await inspector.open();
  await inspector.openSource();
  await inspector.emit(snapshot(1, [versionedRecord("reduced", 1)]));

  const motion = await inspector.page
    .getByRole("row", { name: "Inspect request reduced" })
    .evaluate((row) => {
      const style = getComputedStyle(row);
      return {
        animationName: style.animationName,
        transitionDuration: style.transitionDuration
      };
    });
  expect(motion.animationName).toBe("none");
  expect(Number.parseFloat(motion.transitionDuration)).toBeLessThanOrEqual(0.001);
});

test("marks a selected row detached when HTTP detail is newer than its table revision", async ({
  inspector
}) => {
  let releaseDetail: ((value: { json: unknown }) => void) | undefined;
  const detail = new Promise<{ json: unknown }>((resolve) => {
    releaseDetail = resolve;
  });
  const tableItem = versionedRecord("detached-row", 2, { status: 202 });
  const pinnedItem = versionedRecord("detached-row", 3, { status: 203 });
  inspector.setDetail("detached-row", () => detail);

  await inspector.open("detached-row");
  await inspector.openSource();
  await inspector.emit(snapshot(1, [tableItem]));
  releaseDetail!({ json: pinnedItem });

  const row = inspector.page.getByRole("row", { name: "Inspect request detached-row" });
  await expect(row).toHaveClass(/selected/);
  await expect(row).toHaveClass(/detached/);
  await expect(row).toHaveAttribute("aria-selected", "true");
  expect(
    await row.locator("td").first().evaluate((element) => getComputedStyle(element).backgroundColor)
  ).toBe("rgba(79, 101, 77, 0.35)");
});
