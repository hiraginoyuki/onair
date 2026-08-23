import { describe, expect, it } from "vitest";

import {
  PROTOCOL_EVENT_NAMES,
  RECOVERY_DELAY_MS,
  StreamSupervisor,
  type ConnectionState,
  type StreamEventSource,
  type TimerHandle,
  type TimerScheduler
} from "./stream";
import type { StreamEvent } from "./types";
import { decodeStreamEvent } from "./wire";

class FakeSource implements StreamEventSource {
  onopen: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  closed = false;
  private readonly listeners = new Map<string, ((event: { data: string }) => void)[]>();

  addEventListener(name: string, listener: (event: { data: string }) => void): void {
    const listeners = this.listeners.get(name) ?? [];
    listeners.push(listener);
    this.listeners.set(name, listeners);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.({});
  }

  error(): void {
    this.onerror?.({});
  }

  emit(event: StreamEvent): void {
    this.emitRaw(event.kind, JSON.stringify(event));
  }

  emitRaw(name: string, data: string): void {
    for (const listener of this.listeners.get(name) ?? []) listener({ data });
  }

  get listenerNames(): string[] {
    return [...this.listeners.keys()];
  }
}

class FakeScheduler implements TimerScheduler {
  readonly delays: number[] = [];
  private nextId = 1;
  private readonly tasks: { id: number; callback: () => void; cancelled: boolean }[] = [];

  schedule(callback: () => void, delayMs: number): TimerHandle {
    const id = this.nextId++;
    this.delays.push(delayMs);
    this.tasks.push({ id, callback, cancelled: false });
    return id as unknown as TimerHandle;
  }

  cancel(handle: TimerHandle): void {
    const id = handle as unknown as number;
    const task = this.tasks.find((candidate) => candidate.id === id);
    if (task) task.cancelled = true;
  }

  get activeCount(): number {
    return this.tasks.filter((task) => !task.cancelled).length;
  }

  runNext(): void {
    const task = this.tasks.find((candidate) => !candidate.cancelled);
    if (!task) throw new Error("expected a pending timer");
    task.cancelled = true;
    task.callback();
  }
}

function snapshot(streamSeq = 0): StreamEvent {
  return { kind: "snapshot", stream_seq: streamSeq, records: [] };
}

function reset(streamSeq = 1): StreamEvent {
  return { kind: "reset", stream_seq: streamSeq, reason: "lagged" };
}

function makeSupervisor(
  apply: (event: StreamEvent) => void = () => undefined,
  overrides: Partial<ConstructorParameters<typeof StreamSupervisor>[0]> = {}
) {
  const sources: FakeSource[] = [];
  const scheduler = new FakeScheduler();
  const states: ConnectionState[] = [];
  const supervisor = new StreamSupervisor(
    {
      createSource: () => {
        const source = new FakeSource();
        sources.push(source);
        return source;
      },
      decode: decodeStreamEvent,
      apply,
      onStateChange: (state) => states.push(state),
      ...overrides
    },
    scheduler
  );
  return { sources, scheduler, states, supervisor };
}

describe("StreamSupervisor", () => {
  it("becomes live only after a successfully processed snapshot", () => {
    let applied = 0;
    const { sources, supervisor } = makeSupervisor(() => {
      applied += 1;
    });

    supervisor.start();
    expect(supervisor.connectionState).toBe("connecting");
    expect(sources).toHaveLength(1);
    expect(sources[0].listenerNames).toEqual([...PROTOCOL_EVENT_NAMES]);
    sources[0].open();
    expect(supervisor.connectionState).toBe("connecting");

    sources[0].emit(snapshot());
    expect(applied).toBe(1);
    expect(supervisor.connectionState).toBe("live");
    expect(supervisor.remainingRecoveryBudget).toBe(1);
  });

  it("leaves ordinary transport recovery to the same EventSource", () => {
    let applied = 0;
    const { scheduler, sources, supervisor } = makeSupervisor(() => {
      applied += 1;
    });

    supervisor.start();
    sources[0].emit(snapshot());
    sources[0].error();
    expect(supervisor.connectionState).toBe("reconnecting");
    expect(sources[0].closed).toBe(false);
    expect(sources).toHaveLength(1);
    expect(scheduler.activeCount).toBe(0);

    sources[0].open();
    expect(supervisor.connectionState).toBe("live");
    sources[0].emit(reset());
    expect(applied).toBe(2);
    expect(supervisor.connectionState).toBe("live");
  });

  it("closes one failed source and schedules exactly one 250 ms replacement", () => {
    let applications = 0;
    const { scheduler, sources, supervisor } = makeSupervisor(() => {
      applications += 1;
      throw new Error("synthetic reducer failure");
    });

    supervisor.start();
    const stale = sources[0];
    stale.emit(snapshot());
    expect(stale.closed).toBe(true);
    expect(supervisor.connectionState).toBe("recovering");
    expect(supervisor.remainingRecoveryBudget).toBe(0);
    expect(scheduler.activeCount).toBe(1);
    expect(scheduler.delays).toEqual([RECOVERY_DELAY_MS]);

    stale.emit(reset());
    stale.open();
    stale.error();
    expect(applications).toBe(1);
    expect(scheduler.activeCount).toBe(1);

    scheduler.runNext();
    expect(sources).toHaveLength(2);
    expect(supervisor.connectionState).toBe("recovering");
  });

  it("fails closed when the one replacement also throws before its snapshot", () => {
    let refreshRequired = 0;
    const { scheduler, sources, supervisor } = makeSupervisor(
      () => {
        throw new Error("synthetic reducer failure");
      },
      { onRefreshRequired: () => (refreshRequired += 1) }
    );

    supervisor.start();
    sources[0].emit(snapshot());
    scheduler.runNext();
    sources[1].emit(snapshot(2));

    expect(sources[1].closed).toBe(true);
    expect(supervisor.connectionState).toBe("failed");
    expect(supervisor.hasPendingRecovery).toBe(false);
    expect(scheduler.activeCount).toBe(0);
    expect(scheduler.delays).toEqual([RECOVERY_DELAY_MS]);
    expect(refreshRequired).toBe(1);
  });

  it("restores one recovery budget after a replacement snapshot succeeds", () => {
    let fail = true;
    let recovered = 0;
    const { scheduler, sources, supervisor } = makeSupervisor(
      () => {
        if (fail) throw new Error("synthetic reducer failure");
      },
      { onRecovered: () => (recovered += 1) }
    );

    supervisor.start();
    sources[0].emit(snapshot());
    scheduler.runNext();
    fail = false;
    sources[1].emit(snapshot(2));
    expect(recovered).toBe(1);
    expect(supervisor.connectionState).toBe("live");
    expect(supervisor.remainingRecoveryBudget).toBe(1);

    fail = true;
    sources[1].emit(reset(3));
    expect(supervisor.connectionState).toBe("recovering");
    expect(scheduler.delays).toEqual([RECOVERY_DELAY_MS, RECOVERY_DELAY_MS]);
  });

  it("manual refresh resets a failed supervisor and invalidates stale callbacks", () => {
    let fail = true;
    let applications = 0;
    const { scheduler, sources, supervisor } = makeSupervisor(() => {
      applications += 1;
      if (fail) throw new Error("synthetic reducer failure");
    });

    supervisor.start();
    sources[0].emit(snapshot());
    scheduler.runNext();
    sources[1].emit(snapshot(2));
    expect(supervisor.connectionState).toBe("failed");

    const stale = sources[1];
    supervisor.manualRefresh();
    expect(sources).toHaveLength(3);
    expect(supervisor.connectionState).toBe("connecting");
    expect(supervisor.remainingRecoveryBudget).toBe(1);
    stale.emit(reset(3));
    expect(applications).toBe(2);

    fail = false;
    sources[2].emit(snapshot(4));
    expect(supervisor.connectionState).toBe("live");
  });

  it("warns for malformed wire data without closing or replacing the source", () => {
    let malformed = 0;
    let applied = 0;
    const { scheduler, sources, supervisor } = makeSupervisor(
      () => {
        applied += 1;
      },
      { onMalformed: () => (malformed += 1) }
    );

    supervisor.start();
    sources[0].emitRaw("snapshot", "not-json");
    expect(malformed).toBe(1);
    expect(applied).toBe(0);
    expect(sources[0].closed).toBe(false);
    expect(sources).toHaveLength(1);
    expect(scheduler.activeCount).toBe(0);
  });

  it("teardown cancels recovery and closes the active source", () => {
    const { scheduler, sources, supervisor } = makeSupervisor(() => {
      throw new Error("synthetic reducer failure");
    });

    supervisor.start();
    sources[0].emit(snapshot());
    expect(scheduler.activeCount).toBe(1);
    supervisor.dispose();
    expect(scheduler.activeCount).toBe(0);
    expect(supervisor.hasPendingRecovery).toBe(false);

    sources[0].emit(reset());
    expect(sources).toHaveLength(1);

    const active = makeSupervisor();
    active.supervisor.start();
    expect(active.sources[0].closed).toBe(false);
    active.supervisor.dispose();
    expect(active.sources[0].closed).toBe(true);
  });
});
