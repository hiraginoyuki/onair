import type { StreamEvent } from "./types";

export const RECOVERY_DELAY_MS = 250;
export const PROTOCOL_EVENT_NAMES = ["snapshot", "record_upsert", "record_removed", "reset"] as const;

export type ConnectionState =
  | "connecting"
  | "live"
  | "reconnecting"
  | "recovering"
  | "failed";

export type TimerHandle = ReturnType<typeof globalThis.setTimeout>;

export interface TimerScheduler {
  schedule(callback: () => void, delayMs: number): TimerHandle;
  cancel(handle: TimerHandle): void;
}

export interface StreamEventSource {
  onopen: ((event: unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
  addEventListener(name: string, listener: (event: { data: string }) => void): void;
  close(): void;
}

export type StreamSupervisorCallbacks = {
  createSource(): StreamEventSource;
  decode(data: string): StreamEvent | undefined;
  apply(event: StreamEvent): void;
  onSourceStart?(replacement: boolean): void;
  onStateChange?(state: ConnectionState): void;
  onMalformed?(): void;
  onApplied?(event: StreamEvent): void;
  onRecovering?(): void;
  onRecovered?(): void;
  onRefreshRequired?(): void;
};

const browserScheduler: TimerScheduler = {
  schedule: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
  cancel: (handle) => globalThis.clearTimeout(handle)
};

/** Owns the EventSource lifecycle, but no record or rendering state. */
export class StreamSupervisor {
  private activeSource: StreamEventSource | undefined;
  private activeSourceIsRecovery = false;
  private activeSourceHasSnapshot = false;
  private recoveryTimer: TimerHandle | undefined;
  private recoveryTimerToken = 0;
  private connectionGeneration = 0;
  private processingFailureBudget = 1;
  private disposed = true;
  private state: ConnectionState = "connecting";

  constructor(
    private readonly callbacks: StreamSupervisorCallbacks,
    private readonly scheduler: TimerScheduler = browserScheduler
  ) {}

  start(): void {
    this.disposed = false;
    this.manualRefresh();
  }

  manualRefresh(): void {
    if (this.disposed) this.disposed = false;
    this.clearRecoveryTimer();
    this.invalidateSource();
    this.processingFailureBudget = 1;
    this.activeSourceIsRecovery = false;
    this.activeSourceHasSnapshot = false;
    this.setState("connecting");
    this.openSource(false);
  }

  dispose(): void {
    this.disposed = true;
    this.clearRecoveryTimer();
    this.invalidateSource();
    this.processingFailureBudget = 1;
    this.activeSourceIsRecovery = false;
    this.activeSourceHasSnapshot = false;
  }

  get currentGeneration(): number {
    return this.connectionGeneration;
  }

  get connectionState(): ConnectionState {
    return this.state;
  }

  get remainingRecoveryBudget(): number {
    return this.processingFailureBudget;
  }

  get hasPendingRecovery(): boolean {
    return this.recoveryTimer !== undefined;
  }

  private openSource(replacement: boolean): void {
    if (this.disposed) return;
    const generation = ++this.connectionGeneration;

    let nextSource: StreamEventSource;
    try {
      nextSource = this.callbacks.createSource();
    } catch {
      this.handleProcessingFailure(generation);
      return;
    }
    if (this.disposed || generation !== this.connectionGeneration) {
      nextSource.close();
      return;
    }

    this.activeSource = nextSource;
    this.activeSourceIsRecovery = replacement;
    this.activeSourceHasSnapshot = false;
    this.callbacks.onSourceStart?.(replacement);
    this.setState(replacement ? "recovering" : "connecting");

    nextSource.onopen = () => {
      if (!this.isCurrent(generation, nextSource)) return;
      this.setState(
        this.activeSourceHasSnapshot
          ? "live"
          : this.activeSourceIsRecovery
            ? "recovering"
            : "connecting"
      );
    };
    nextSource.onerror = () => {
      if (!this.isCurrent(generation, nextSource)) return;
      this.setState("reconnecting");
    };

    for (const name of PROTOCOL_EVENT_NAMES) {
      nextSource.addEventListener(name, (message) => {
        if (!this.isCurrent(generation, nextSource)) return;

        let event: StreamEvent | undefined;
        try {
          event = this.callbacks.decode(message.data);
        } catch {
          event = undefined;
        }
        if (!event) {
          this.callbacks.onMalformed?.();
          return;
        }

        try {
          this.callbacks.apply(event);
          this.callbacks.onApplied?.(event);
        } catch {
          this.handleProcessingFailure(generation);
          return;
        }

        if (event.kind === "snapshot") {
          const recovered = this.activeSourceIsRecovery;
          this.activeSourceIsRecovery = false;
          this.activeSourceHasSnapshot = true;
          this.processingFailureBudget = 1;
          this.setState("live");
          if (recovered) this.callbacks.onRecovered?.();
        } else if (this.activeSourceHasSnapshot) {
          this.setState("live");
        }
      });
    }
  }

  private handleProcessingFailure(generation: number): void {
    if (this.disposed || generation !== this.connectionGeneration) return;
    this.invalidateSource();

    if (this.processingFailureBudget === 0) {
      this.clearRecoveryTimer();
      this.activeSourceIsRecovery = false;
      this.activeSourceHasSnapshot = false;
      this.setState("failed");
      this.callbacks.onRefreshRequired?.();
      return;
    }

    this.processingFailureBudget -= 1;
    this.activeSourceIsRecovery = true;
    this.setState("recovering");
    this.callbacks.onRecovering?.();
    this.scheduleRecovery();
  }

  private scheduleRecovery(): void {
    if (this.disposed || this.recoveryTimer !== undefined) return;
    const token = ++this.recoveryTimerToken;
    this.recoveryTimer = this.scheduler.schedule(() => {
      if (this.disposed || token !== this.recoveryTimerToken) return;
      this.recoveryTimer = undefined;
      this.openSource(true);
    }, RECOVERY_DELAY_MS);
  }

  private clearRecoveryTimer(): void {
    this.recoveryTimerToken += 1;
    if (this.recoveryTimer === undefined) return;
    this.scheduler.cancel(this.recoveryTimer);
    this.recoveryTimer = undefined;
  }

  private invalidateSource(): void {
    const current = this.activeSource;
    this.activeSource = undefined;
    this.connectionGeneration += 1;
    current?.close();
  }

  private isCurrent(generation: number, source: StreamEventSource): boolean {
    return (
      !this.disposed &&
      generation === this.connectionGeneration &&
      source === this.activeSource
    );
  }

  private setState(next: ConnectionState): void {
    if (this.state === next) return;
    this.state = next;
    this.callbacks.onStateChange?.(next);
  }
}
