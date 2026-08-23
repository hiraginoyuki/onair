<script lang="ts">
  import { onMount } from "svelte";

  import RecordDetail from "./RecordDetail.svelte";
  import RecordTable from "./RecordTable.svelte";
  import {
    applyEvent,
    eventSequence,
    freezeRecords,
    saturatingIncrement,
    shouldAcceptEventSequence,
    shouldFlushCoalesced
  } from "./lib/records";
  import type { RecordMap } from "./lib/records";
  import { deriveStreamPresentation, streamStripLabel } from "./lib/presentation";
  import {
    fetchVersionedRecord,
    isSelectionResponseCurrent,
    markSelectionDetached,
    reconcileSelection,
    selectionRecordId
  } from "./lib/selection";
  import { CLIENT_LIMIT } from "./lib/store";
  import { StreamSupervisor } from "./lib/stream";
  import type { ConnectionState, StreamEventSource } from "./lib/stream";
  import type { SelectionState, StreamEvent, VersionedRecord } from "./lib/types";
  import { decodeStreamEvent } from "./lib/wire";

  let liveRecords: RecordMap = new Map();
  let displayRecords: ReadonlyMap<string, VersionedRecord> = liveRecords;
  let frozenRecords: ReadonlyMap<string, VersionedRecord> | undefined;
  let displayEpoch = 0;
  let selection: SelectionState = { kind: "none" };
  let selectionRequestToken = 0;
  let projectionEpoch = 0;
  let streamReady = false;
  let filter = "";
  let paused = false;
  let connectionState: ConnectionState = "connecting";
  let malformedStream = false;
  let resettingProjection = false;
  let pausedUpdateCount = 0;
  let actionNotice = "";
  let retainedCount: number | undefined;
  let lastSequence = 0;
  let recordTable: RecordTable | undefined;
  let streamSupervisor: StreamSupervisor | undefined;
  let queuedLiveEvents = new Map<
    string,
    Extract<StreamEvent, { kind: "record_upsert" }>
  >();
  let liveFlushFrame: number | undefined;
  let liveFlushTimer: number | undefined;
  let operatorTimer: number | undefined;
  let noticeTimer: number | undefined;

  $: selectedId = selectionRecordId(selection);
  $: selectedIsDetached = selection.kind === "ready" && selection.detached;
  $: streamPresentation = deriveStreamPresentation({
    connectionState,
    paused,
    resetting: resettingProjection,
    warning: malformedStream
  });
  $: streamStatusLabel = streamStripLabel(streamPresentation);

  onMount(() => {
    window.addEventListener("hashchange", handleHashChange);
    streamSupervisor = new StreamSupervisor({
      createSource: () =>
        new EventSource(
          `/_onair/inspector-next/events?snapshot_limit=${CLIENT_LIMIT}`
        ) as unknown as StreamEventSource,
      decode: decodeStreamEvent,
      apply: dispatchEvent,
      onSourceStart: () => {
        cancelLiveFlush();
        beginProjection();
      },
      onStateChange: (state) => (connectionState = state),
      onMalformed: () => {
        console.warn("inspector stream warning: malformed event ignored");
        malformedStream = true;
      },
      onApplied: () => {
        malformedStream = false;
      }
    });
    streamSupervisor.start();
    void refreshRuntime();
    operatorTimer = window.setInterval(() => void refreshRuntime(), 15_000);
    const initialSelectedId = readHashRecordId();
    if (initialSelectedId) {
      void loadSelectedById(initialSelectedId, false, nextSelectionRequest());
    }
    return () => {
      window.removeEventListener("hashchange", handleHashChange);
      window.clearInterval(operatorTimer);
      window.clearTimeout(noticeTimer);
      streamSupervisor?.dispose();
      cancelLiveFlush();
    };
  });

  function readHashRecordId(): string {
    if (typeof window === "undefined") return "";
    const hash = window.location.hash.slice(1);
    if (!hash) return "";
    try {
      return decodeURIComponent(hash);
    } catch {
      return hash;
    }
  }

  function nextSelectionRequest(): number {
    selectionRequestToken += 1;
    return selectionRequestToken;
  }

  function retargetUnresolvedSelectionForEpoch() {
    const recordId = selectionRecordId(selection);
    if (!recordId || selection.kind === "ready") return;
    selection = {
      kind: "loading",
      recordId,
      requestToken: selectionRequestToken,
      epoch: projectionEpoch
    };
  }

  function beginProjection() {
    projectionEpoch += 1;
    nextSelectionRequest();
    lastSequence = 0;
    streamReady = false;
    resettingProjection = false;
    liveRecords = new Map();
    retargetUnresolvedSelectionForEpoch();
    if (!paused) {
      displayRecords = liveRecords;
      displayEpoch = projectionEpoch;
      selection = markSelectionDetached(selection, displayRecords);
    }
  }

  function beginResetProjection() {
    projectionEpoch += 1;
    nextSelectionRequest();
    streamReady = false;
    resettingProjection = true;
    retargetUnresolvedSelectionForEpoch();
  }

  function publishLiveRecords() {
    liveRecords = new Map(liveRecords);
    if (paused) return;
    displayRecords = liveRecords;
    displayEpoch = projectionEpoch;
    selection = markSelectionDetached(selection, displayRecords);
  }

  function handleEvent(event: StreamEvent, publish = true): boolean {
    const sequence = eventSequence(event);
    if (!shouldAcceptEventSequence(lastSequence, streamReady, event)) return false;
    lastSequence = sequence;

    if (event.kind === "reset") {
      beginResetProjection();
      void refreshRuntime();
    }
    const result = applyEvent(liveRecords, event);
    if (paused && result.accepted) {
      pausedUpdateCount = saturatingIncrement(pausedUpdateCount);
    }
    if (publish && (result.changed || result.reset)) publishLiveRecords();

    if (event.kind === "snapshot") {
      streamReady = true;
      resettingProjection = false;
      const recordId = selectionRecordId(selection);
      const canonical = recordId ? liveRecords.get(recordId) : undefined;
      if (!paused && recordId) {
        selection = reconcileSelection(
          selection,
          recordId,
          canonical,
          projectionEpoch,
          displayRecords
        );
        if (!canonical || (selection.kind === "ready" && selection.detached)) {
          void loadSelectedById(recordId, false, nextSelectionRequest());
        }
      } else if (!paused && !recordId && liveRecords.size) {
        const newest = [...liveRecords.values()].reduce((current, candidate) =>
          candidate.record.started_at_unix_ms > current.record.started_at_unix_ms
            ? candidate
            : current
        );
        nextSelectionRequest();
        selectVersioned(newest, false, projectionEpoch);
      }
    }
    if (!paused && event.kind === "record_upsert" && result.changed) {
      const canonical = liveRecords.get(event.record_id);
      const recordId = selectionRecordId(selection);
      if (event.record_id === recordId && canonical) {
        selection = reconcileSelection(
          selection,
          recordId,
          canonical,
          projectionEpoch,
          displayRecords
        );
      } else if (!recordId && canonical) {
        nextSelectionRequest();
        selectVersioned(canonical, true, projectionEpoch);
      }
    }
    if (!paused && result.changed) {
      selection = markSelectionDetached(selection, displayRecords);
    }
    if (event.kind !== "reset") {
      streamReady = true;
      malformedStream = false;
    }
    return result.accepted;
  }

  function dispatchEvent(event: StreamEvent) {
    if (event.kind === "record_upsert" && event.phase === "live") {
      if (shouldFlushCoalesced(queuedLiveEvents, event.record_id)) flushLiveEvents();
      const changed = handleEvent(event, false);
      if (!changed) return;
      const current = queuedLiveEvents.get(event.record_id);
      if (
        !current ||
        event.revision > current.revision ||
        (event.revision === current.revision && event.stream_seq > current.stream_seq)
      ) {
        queuedLiveEvents.set(event.record_id, event);
      }
      scheduleLiveFlush();
      return;
    }

    flushLiveEvents();
    handleEvent(event);
  }

  function scheduleLiveFlush() {
    if (liveFlushFrame !== undefined || liveFlushTimer !== undefined) return;
    liveFlushFrame = window.requestAnimationFrame(() => {
      liveFlushFrame = undefined;
      if (liveFlushTimer !== undefined) {
        window.clearTimeout(liveFlushTimer);
        liveFlushTimer = undefined;
      }
      flushLiveEvents();
    });
    liveFlushTimer = window.setTimeout(() => {
      liveFlushTimer = undefined;
      if (liveFlushFrame !== undefined) {
        window.cancelAnimationFrame(liveFlushFrame);
        liveFlushFrame = undefined;
      }
      flushLiveEvents();
    }, 100);
  }

  function flushLiveEvents() {
    clearLiveFlushSchedule();
    if (!queuedLiveEvents.size) return;
    queuedLiveEvents.clear();
    publishLiveRecords();
  }

  function clearLiveFlushSchedule() {
    if (liveFlushFrame !== undefined) {
      window.cancelAnimationFrame(liveFlushFrame);
      liveFlushFrame = undefined;
    }
    if (liveFlushTimer !== undefined) {
      window.clearTimeout(liveFlushTimer);
      liveFlushTimer = undefined;
    }
  }

  function cancelLiveFlush() {
    clearLiveFlushSchedule();
    queuedLiveEvents.clear();
  }

  function togglePause() {
    if (!paused) {
      flushLiveEvents();
      nextSelectionRequest();
      paused = true;
      frozenRecords = freezeRecords(liveRecords);
      displayRecords = frozenRecords;
      displayEpoch = projectionEpoch;
      pausedUpdateCount = 0;
      return;
    }

    flushLiveEvents();
    paused = false;
    frozenRecords = undefined;
    displayRecords = liveRecords;
    displayEpoch = projectionEpoch;
    const recordId = selectionRecordId(selection);
    const canonical = recordId ? liveRecords.get(recordId) : undefined;
    if (recordId) {
      selection = reconcileSelection(
        selection,
        recordId,
        canonical,
        projectionEpoch,
        displayRecords
      );
    }
    pausedUpdateCount = 0;
    if (recordId && (!canonical || (selection.kind === "ready" && selection.detached))) {
      void loadSelectedById(recordId, false, nextSelectionRequest());
    }
    showNotice("live view resumed");
  }

  function updateFilter(event: Event) {
    filter = (event.currentTarget as HTMLInputElement).value;
  }

  function refreshInspector() {
    showNotice("refreshing inspector");
    malformedStream = false;
    resettingProjection = false;
    streamSupervisor?.manualRefresh();
    void refreshRuntime();
  }

  async function refreshRuntime() {
    try {
      const response = await fetch("/_onair/operator/runtime", { cache: "no-store" });
      if (!response.ok) throw new Error(`runtime unavailable: HTTP ${response.status}`);
      const runtime = (await response.json()) as {
        inspector_retained_requests?: unknown;
      };
      retainedCount =
        typeof runtime.inspector_retained_requests === "number" &&
        Number.isFinite(runtime.inspector_retained_requests) &&
        runtime.inspector_retained_requests >= 0
          ? runtime.inspector_retained_requests
          : undefined;
    } catch {
      retainedCount = undefined;
    }
  }

  function selectVersioned(
    item: VersionedRecord,
    updateHash: boolean,
    epoch: number
  ) {
    selection = reconcileSelection(selection, item.record_id, item, epoch, displayRecords);
    if (updateHash) {
      history.replaceState(null, "", `#${encodeURIComponent(item.record_id)}`);
    }
  }

  function selectDisplayed(recordId: string) {
    nextSelectionRequest();
    const item = displayRecords.get(recordId);
    if (item) selectVersioned(item, true, displayEpoch);
  }

  async function handleHashChange() {
    const requestToken = nextSelectionRequest();
    const target = readHashRecordId();
    if (!target) {
      selection = { kind: "none" };
      return;
    }
    await loadSelectedById(target, false, requestToken);
  }

  async function loadSelectedById(
    recordId: string,
    updateHash: boolean,
    requestToken: number
  ) {
    if (!recordId) return;
    const prior = selection;
    const priorItem =
      prior.kind === "ready" && prior.item.record_id === recordId ? prior.item : undefined;
    if (!priorItem) {
      selection = {
        kind: "loading",
        recordId,
        requestToken,
        epoch: projectionEpoch
      };
    }
    if (updateHash) history.replaceState(null, "", `#${encodeURIComponent(recordId)}`);

    const displayed = displayRecords.get(recordId);
    if (displayed) {
      selectVersioned(displayed, false, displayEpoch);
      if (selection.kind !== "ready" || !selection.detached) return;
    }
    if (paused) return;

    const requestEpoch = projectionEpoch;
    try {
      const fetched = await fetchVersionedRecord(recordId);
      if (
        !isSelectionResponseCurrent(
          requestToken,
          selectionRequestToken,
          requestEpoch,
          projectionEpoch,
          recordId,
          selection,
          paused
        )
      ) {
        return;
      }
      selection = reconcileSelection(
        selection,
        recordId,
        fetched,
        requestEpoch,
        displayRecords
      );
      selection = reconcileSelection(
        selection,
        recordId,
        liveRecords.get(recordId),
        projectionEpoch,
        displayRecords
      );
    } catch (error) {
      if (
        !isSelectionResponseCurrent(
          requestToken,
          selectionRequestToken,
          requestEpoch,
          projectionEpoch,
          recordId,
          selection,
          paused
        )
      ) {
        return;
      }
      const message = error instanceof Error ? error.message : "record lookup failed";
      if (selection.kind === "ready" && selection.item.record_id === recordId) {
        selection = markSelectionDetached(selection, displayRecords);
        showNotice(message, false);
      } else {
        selection = { kind: "error", recordId, message, epoch: projectionEpoch };
      }
    }
  }

  function showNotice(message: string, persistent = false) {
    actionNotice = message;
    window.clearTimeout(noticeTimer);
    if (!persistent) noticeTimer = window.setTimeout(() => (actionNotice = ""), 3600);
  }
</script>

<svelte:head><title>onair inspector</title></svelte:head>

<main>
  <header class="toolbar">
    <div class="title-group">
      <div class="eyebrow">operator surface · v2</div>
      <h1>onair inspector</h1>
      <p
        class={`stream-indicator stream-${streamPresentation.tone}`}
        role="status"
        aria-live="polite"
        aria-atomic="true"
        data-state={streamPresentation.state}
      >
        <span
          class:online={streamPresentation.tone === "good"}
          class:failed={streamPresentation.tone === "error"}
          class="status-dot"
          aria-hidden="true"
        ></span>
        {streamPresentation.label}
      </p>
    </div>
    <div class="actions">
      <label class="filter-wrap">
        <span class="sr-only">Filter records</span>
        <input
          aria-label="filter records"
          placeholder="filter records"
          value={filter}
          on:input={updateFilter}
        />
      </label>
      <button
        type="button"
        class="pause-action"
        class:active={paused}
        aria-label={paused ? "Resume table updates" : "Pause table updates"}
        aria-pressed={paused}
        on:click={togglePause}>{paused ? "resume" : "pause"}</button
      >
      <button type="button" on:click={refreshInspector}>refresh</button>
      <button type="button" on:click={() => recordTable?.resetWidths()}>reset widths</button>
    </div>
  </header>

  <section class="status-strip" aria-label="inspector status">
    <span><strong>{displayRecords.size.toLocaleString()}</strong> loaded</span>
    <span
      ><strong>{retainedCount === undefined ? "—" : retainedCount.toLocaleString()}</strong>
      retained</span
    >
    {#if paused}<span><strong>view frozen</strong></span>{/if}
    {#if paused && pausedUpdateCount > 0}
      <span
        ><strong>{pausedUpdateCount.toLocaleString()}</strong>
        {pausedUpdateCount === 1 ? "update" : "updates"} while paused</span
      >
    {/if}
    <span
      class:status-live={streamPresentation.tone === "good"}
      class:status-offline={streamPresentation.tone !== "good"}
      class:status-error={streamPresentation.tone === "error"}>{streamStatusLabel}</span
    >
  </section>

  {#if actionNotice}
    <div class="notices" aria-live="polite">
      <div class="notice notice-info">{actionNotice}</div>
    </div>
  {/if}

  <section class="workspace">
    <RecordTable
      bind:this={recordTable}
      {displayRecords}
      {filter}
      {selectedId}
      selectedDetached={selectedIsDetached}
      {streamReady}
      {paused}
      onSelect={selectDisplayed}
    />
    <RecordDetail {selection} onNotice={showNotice} />
  </section>
</main>
