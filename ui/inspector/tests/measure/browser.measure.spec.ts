import { versionedRecord } from "../../src/lib/test-fixtures";
import type { VersionedRecord } from "../../src/lib/types";
import { test } from "../browser/fixture";

const warmups = 10;
const recordedSamples = 50;

type BrowserMeasurement = {
  record_count: number;
  serialized_snapshot_bytes: number;
  pre_serialized_decode_reducer_ms: { p50: number; p95: number };
  publication_plus_two_animation_frames_ms: { p50: number; p95: number };
  rendered_row_count: number;
  heap_delta_bytes: number | null;
};

function measurementRecords(recordCount: number): VersionedRecord[] {
  return Array.from({ length: recordCount }, (_, index) =>
    versionedRecord(`measurement-${String(index).padStart(4, "0")}`, 1, {
      started_at_unix_ms: 1_700_000_000_000 + index,
      completed_at_unix_ms: 1_700_000_000_001 + index,
      route: `fixture-route-${index % 8}`,
      public_model: `fixture-model-${index % 4}`,
      backend: `fixture-backend-${index % 3}`,
      timeline: {
        started_unix_ms: 1_700_000_000_000 + index,
        total_us: 1_000 + index,
        proxy_entry_us: 0
      }
    })
  );
}

test("reports exact-artifact snapshot decode, publication, DOM, and heap observations", async ({
  inspector
}) => {
  await inspector.page.setViewportSize({ width: 1280, height: 844 });
  await inspector.open();
  await inspector.openSource();

  const environment = await inspector.page.evaluate(() => {
    const memory = (
      performance as Performance & {
        memory?: { usedJSHeapSize: number };
      }
    ).memory;
    return {
      user_agent: navigator.userAgent,
      platform: navigator.platform,
      hardware_concurrency: navigator.hardwareConcurrency,
      viewport: { width: window.innerWidth, height: window.innerHeight },
      performance_memory_available: Boolean(memory)
    };
  });

  const corpora: BrowserMeasurement[] = [];
  for (const recordCount of [100, 1_000]) {
    const payload = JSON.stringify({
      kind: "snapshot",
      stream_seq: 1,
      records: measurementRecords(recordCount)
    });
    const measurement = await inspector.page.evaluate(
      async ({ payload, recordCount, warmups, recordedSamples }) => {
        const source = window.__inspectorBrowserTest;
        const twoAnimationFrames = () =>
          new Promise<void>((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(() => resolve()))
          );
        const percentile = (samples: number[], value: number) => {
          const sorted = [...samples].sort((left, right) => left - right);
          const rank = Math.ceil((sorted.length * value) / 100);
          return sorted[Math.max(0, Math.min(sorted.length - 1, rank - 1))];
        };
        const summarize = (samples: number[]) => ({
          p50: percentile(samples, 50),
          p95: percentile(samples, 95)
        });
        const usedHeap = () =>
          (
            performance as Performance & {
              memory?: { usedJSHeapSize: number };
            }
          ).memory?.usedJSHeapSize;

        const heapBefore = usedHeap();
        for (let index = 0; index < warmups; index += 1) {
          source.emitRaw(0, "snapshot", payload);
        }
        await twoAnimationFrames();

        const decodeReducerSamples: number[] = [];
        for (let index = 0; index < recordedSamples; index += 1) {
          const started = performance.now();
          source.emitRaw(0, "snapshot", payload);
          decodeReducerSamples.push(performance.now() - started);
        }
        await twoAnimationFrames();

        for (let index = 0; index < warmups; index += 1) {
          source.emitRaw(0, "snapshot", payload);
          await twoAnimationFrames();
        }
        const publicationSamples: number[] = [];
        for (let index = 0; index < recordedSamples; index += 1) {
          const started = performance.now();
          source.emitRaw(0, "snapshot", payload);
          await twoAnimationFrames();
          publicationSamples.push(performance.now() - started);
        }

        const heapAfter = usedHeap();
        return {
          record_count: recordCount,
          serialized_snapshot_bytes: new TextEncoder().encode(payload).byteLength,
          pre_serialized_decode_reducer_ms: summarize(decodeReducerSamples),
          publication_plus_two_animation_frames_ms: summarize(publicationSamples),
          rendered_row_count: document.querySelectorAll("tbody tr[aria-label]").length,
          heap_delta_bytes:
            heapBefore === undefined || heapAfter === undefined ? null : heapAfter - heapBefore
        };
      },
      { payload, recordCount, warmups, recordedSamples }
    );
    corpora.push(measurement);
  }

  const report = {
    schema: "onair-inspector-browser-measurement-v1",
    artifact_path: "ui/inspector/dist/index.html",
    samples: { warmups, recorded: recordedSamples },
    environment,
    snapshot_corpora: corpora,
    production_observations: {
      reconnect_frequency: "unavailable_without_production_telemetry",
      replay_hit_rate: "unavailable_without_production_telemetry",
      directed_synthetic_cases_are_production_frequency: false
    }
  };
  console.log(`INSPECTOR_BROWSER_MEASUREMENT\n${JSON.stringify(report, null, 2)}`);
});
