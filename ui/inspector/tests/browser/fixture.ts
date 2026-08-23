import { expect, test as base, type Page, type Route } from "@playwright/test";
import type { StreamEvent, VersionedRecord } from "../../src/lib/types";

type DetailResponse = {
  status?: number;
  json?: unknown;
};

type DetailHandler = () => DetailResponse | Promise<DetailResponse>;

type BrowserDiagnostics = {
  consoleErrors: string[];
  pageErrors: string[];
  unexpectedRequests: string[];
};

type BrowserControl = {
  sourceCount(): number;
  sourceUrl(index: number): string | undefined;
  sourceClosed(index: number): boolean | undefined;
  open(index: number): void;
  error(index: number): void;
  emit(index: number, name: string, value: unknown): void;
  emitRaw(index: number, name: string, data: string): void;
};

declare global {
  interface Window {
    __inspectorBrowserTest: BrowserControl;
  }
}

export class InspectorHarness {
  private readonly detailHandlers = new Map<string, DetailHandler>();
  private retainedCount = 0;

  constructor(
    readonly page: Page,
    private readonly diagnostics: BrowserDiagnostics
  ) {}

  async install(): Promise<void> {
    await this.page.addInitScript(() => {
      type Listener = (event: { data: string }) => void;

      class FakeEventSource {
        static readonly instances: FakeEventSource[] = [];
        static readonly CONNECTING = 0;
        static readonly OPEN = 1;
        static readonly CLOSED = 2;

        readonly url: string;
        readonly withCredentials = false;
        readyState = FakeEventSource.CONNECTING;
        onopen: ((event: Event) => void) | null = null;
        onerror: ((event: Event) => void) | null = null;
        private readonly listeners = new Map<string, Set<Listener>>();

        constructor(url: string | URL) {
          this.url = String(url);
          FakeEventSource.instances.push(this);
        }

        addEventListener(name: string, listener: EventListenerOrEventListenerObject): void {
          const callbacks = this.listeners.get(name) ?? new Set<Listener>();
          callbacks.add(listener as Listener);
          this.listeners.set(name, callbacks);
        }

        removeEventListener(name: string, listener: EventListenerOrEventListenerObject): void {
          this.listeners.get(name)?.delete(listener as Listener);
        }

        dispatchEvent(): boolean {
          return true;
        }

        close(): void {
          this.readyState = FakeEventSource.CLOSED;
        }

        open(): void {
          this.readyState = FakeEventSource.OPEN;
          this.onopen?.(new Event("open"));
        }

        fail(): void {
          this.readyState = FakeEventSource.CONNECTING;
          this.onerror?.(new Event("error"));
        }

        emit(name: string, data: string): void {
          for (const listener of this.listeners.get(name) ?? []) listener({ data });
        }
      }

      Object.defineProperty(window, "EventSource", {
        configurable: true,
        writable: true,
        value: FakeEventSource
      });

      window.__inspectorBrowserTest = {
        sourceCount: () => FakeEventSource.instances.length,
        sourceUrl: (index) => FakeEventSource.instances[index]?.url,
        sourceClosed: (index) =>
          FakeEventSource.instances[index]?.readyState === FakeEventSource.CLOSED,
        open: (index) => FakeEventSource.instances[index]?.open(),
        error: (index) => FakeEventSource.instances[index]?.fail(),
        emit: (index, name, value) =>
          FakeEventSource.instances[index]?.emit(name, JSON.stringify(value)),
        emitRaw: (index, name, data) => FakeEventSource.instances[index]?.emit(name, data)
      };
    });

    await this.page.route("**/*", (route) => this.routeRequest(route));
  }

  setRetainedCount(count: number): void {
    this.retainedCount = count;
  }

  setDetail(recordId: string, response: DetailResponse | DetailHandler): void {
    this.detailHandlers.set(
      recordId,
      typeof response === "function" ? response : () => response
    );
  }

  async open(hash = ""): Promise<void> {
    await this.page.goto(`/_onair/inspector-next${hash ? `#${encodeURIComponent(hash)}` : ""}`);
    await this.expectSourceCount(1);
  }

  async expectSourceCount(count: number): Promise<void> {
    await expect.poll(() => this.sourceCount()).toBe(count);
  }

  async sourceCount(): Promise<number> {
    return this.page.evaluate(() => window.__inspectorBrowserTest.sourceCount());
  }

  async sourceUrl(index = 0): Promise<string | undefined> {
    return this.page.evaluate((value) => window.__inspectorBrowserTest.sourceUrl(value), index);
  }

  async sourceClosed(index = 0): Promise<boolean | undefined> {
    return this.page.evaluate((value) => window.__inspectorBrowserTest.sourceClosed(value), index);
  }

  async openSource(index = 0): Promise<void> {
    await this.page.evaluate((value) => window.__inspectorBrowserTest.open(value), index);
  }

  async errorSource(index = 0): Promise<void> {
    await this.page.evaluate((value) => window.__inspectorBrowserTest.error(value), index);
  }

  async emit(event: StreamEvent, index = 0): Promise<void> {
    await this.page.evaluate(
      ({ eventValue, sourceIndex }) =>
        window.__inspectorBrowserTest.emit(sourceIndex, eventValue.kind, eventValue),
      { eventValue: event, sourceIndex: index }
    );
  }

  async emitRaw(name: string, data: string, index = 0): Promise<void> {
    await this.page.evaluate(
      ({ eventName, eventData, sourceIndex }) =>
        window.__inspectorBrowserTest.emitRaw(sourceIndex, eventName, eventData),
      { eventName: name, eventData: data, sourceIndex: index }
    );
  }

  async failNextMapClear(): Promise<void> {
    await this.page.evaluate(() => {
      const original = Map.prototype.clear;
      Map.prototype.clear = function clearWithSyntheticFailure() {
        Map.prototype.clear = original;
        throw new Error("synthetic projection failure");
      };
    });
  }

  private async routeRequest(route: Route): Promise<void> {
    const request = route.request();
    const url = new URL(request.url());
    if (url.origin !== "http://127.0.0.1:4179") {
      this.diagnostics.unexpectedRequests.push(request.url());
      await route.abort();
      return;
    }

    if (request.isNavigationRequest() && url.pathname === "/_onair/inspector-next") {
      await route.continue();
      return;
    }

    if (url.pathname === "/_onair/operator/runtime") {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ inspector_retained_requests: this.retainedCount })
      });
      return;
    }

    const prefix = "/_onair/inspector-next/requests/";
    if (url.pathname.startsWith(prefix)) {
      const recordId = decodeURIComponent(url.pathname.slice(prefix.length));
      const response = await this.detailHandlers.get(recordId)?.();
      await route.fulfill({
        status: response?.status ?? 404,
        contentType: "application/json",
        body: JSON.stringify(response?.json ?? { error: "not retained" })
      });
      return;
    }

    this.diagnostics.unexpectedRequests.push(request.url());
    await route.fulfill({ status: 404, contentType: "text/plain", body: "unexpected request\n" });
  }
}

type Fixtures = {
  inspector: InspectorHarness;
};

export const test = base.extend<Fixtures>({
  inspector: async ({ page }, use) => {
    const diagnostics: BrowserDiagnostics = {
      consoleErrors: [],
      pageErrors: [],
      unexpectedRequests: []
    };
    page.on("console", (message) => {
      if (message.type() === "error") diagnostics.consoleErrors.push(message.text());
    });
    page.on("pageerror", (error) => diagnostics.pageErrors.push(error.message));

    const inspector = new InspectorHarness(page, diagnostics);
    await inspector.install();
    await use(inspector);

    expect.soft(diagnostics.consoleErrors, "browser console errors").toEqual([]);
    expect.soft(diagnostics.pageErrors, "uncaught page errors").toEqual([]);
    expect.soft(diagnostics.unexpectedRequests, "unexpected or external requests").toEqual([]);
  }
});

export { expect };

export function snapshot(streamSeq: number, records: VersionedRecord[]): StreamEvent {
  return { kind: "snapshot", stream_seq: streamSeq, records };
}

export function upsert(
  streamSeq: number,
  item: VersionedRecord,
  phase: "initial" | "live" | "terminal" = "live"
): StreamEvent {
  return {
    kind: "record_upsert",
    stream_seq: streamSeq,
    record_id: item.record_id,
    revision: item.revision,
    phase,
    record: item.record
  };
}

export function remove(streamSeq: number, recordId: string, revision: number): StreamEvent {
  return {
    kind: "record_removed",
    stream_seq: streamSeq,
    record_id: recordId,
    revision,
    reason: "retention_evicted"
  };
}

export function reset(streamSeq: number): StreamEvent {
  return { kind: "reset", stream_seq: streamSeq, reason: "lagged" };
}
