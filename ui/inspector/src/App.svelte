<script lang="ts">
  import { onMount } from "svelte";
  import { CLIENT_LIMIT, columns, columnWidth, ROW_HEIGHT } from "./lib/store";
  import {
    applyEvent,
    eventSequence,
    freezeRecords,
    isSelectionResponseCurrent,
    markSelectionDetached,
    reconcileSelection,
    saturatingIncrement,
    selectionItem,
    selectionRecordId,
    shouldAcceptEventSequence,
    shouldFlushCoalesced
  } from "./lib/records";
  import type { RecordMap } from "./lib/records";
  import { deriveStreamPresentation, streamStripLabel } from "./lib/presentation";
  import { StreamSupervisor } from "./lib/stream";
  import type { ConnectionState, StreamEventSource } from "./lib/stream";
  import { decodeStreamEvent, decodeVersionedRecord } from "./lib/wire";
  import type {
    ColumnKey,
    InspectorAttempt,
    InspectorRecord,
    SelectionState,
    StreamEvent,
    Timeline,
    VersionedRecord
  } from "./lib/types";

  type Widths = Partial<Record<ColumnKey, number>>;
  type TimelineField = { key: keyof Timeline; label: string };
  type PhaseField = { key: keyof InspectorAttempt; label: string };

  const widthKey = "onair.inspector.v4.widths";
  const timeFormatter = new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
    fractionalSecondDigits: 3
  });
  const timelineFields: TimelineField[] = [
    { key: "proxy_entry_us", label: "proxy entry" },
    { key: "auth_done_us", label: "auth done" },
    { key: "request_inspected_us", label: "request inspected" },
    { key: "route_selected_us", label: "route selected" },
    { key: "request_rewritten_us", label: "request rewritten" },
    { key: "debug_capture_done_us", label: "debug capture done" },
    { key: "backend_forward_start_us", label: "backend forward start" },
    { key: "backend_headers_received_us", label: "backend headers" },
    { key: "backend_body_first_chunk_us", label: "first body chunk" },
    { key: "backend_body_complete_us", label: "body complete" },
    { key: "response_rewritten_us", label: "response rewritten" },
    { key: "client_response_ready_us", label: "client response ready" },
    { key: "stream_complete_us", label: "stream complete" }
  ];
  const phaseFields: PhaseField[] = [
    { key: "request_rewritten_us", label: "rewrite" },
    { key: "debug_capture_done_us", label: "capture" },
    { key: "backend_forward_start_us", label: "send" },
    { key: "backend_headers_received_us", label: "headers" },
    { key: "backend_body_first_chunk_us", label: "first byte" },
    { key: "backend_body_complete_us", label: "body done" },
    { key: "stream_complete_us", label: "stream done" }
  ];

  let liveRecords: RecordMap = new Map();
  let displayRecords: ReadonlyMap<string, VersionedRecord> = liveRecords;
  let frozenRecords: ReadonlyMap<string, VersionedRecord> | undefined;
  let displayEpoch = 0;
  let selection: SelectionState = { kind: "none" };
  let selectionRequestToken = 0;
  let projectionEpoch = 0;
  let streamReady = false;
  let filter = "";
  let sortKey: ColumnKey = "time";
  let sortDescending = true;
  let paused = false;
  let connectionState: ConnectionState = "connecting";
  let malformedStream = false;
  let resettingProjection = false;
  let pausedUpdateCount = 0;
  let actionNotice = "";
  let retainedCount: number | undefined;
  let lastSequence = 0;
  let widths: Widths = loadWidths();
  let viewportTop = 0;
  let viewportHeight = 480;
  let viewportLeft = 0;
  let tableWrap: HTMLElement | undefined;
  let streamSupervisor: StreamSupervisor | undefined;
  let resizeStart: { key: ColumnKey; x: number; width: number; pointerId: number; element: HTMLElement } | undefined;
  let queuedLiveEvents = new Map<string, Extract<StreamEvent, { kind: "record_upsert" }>>();
  let liveFlushFrame: number | undefined;
  let liveFlushTimer: number | undefined;
  let operatorTimer: number | undefined;
  let noticeTimer: number | undefined;
  let expandedAttempts = new Set<string>();
  const downloadUrls = new Set<string>();

  $: selectedId = selectionRecordId(selection);
  $: selectedItem = selectionItem(selection);
  $: detailViewState =
    selection.kind === "ready" ? (selection.detached ? "detached-ready" : "ready") : selection.kind;
  $: streamPresentation = deriveStreamPresentation({
    connectionState,
    paused,
    resetting: resettingProjection,
    warning: malformedStream
  });
  $: streamStatusLabel = streamStripLabel(streamPresentation);
  $: filtered = Array.from(displayRecords.values())
    .map((entry) => entry.record)
    .filter((record) => matchesFilter(record, filterNeedle))
    .sort((a, b) => {
      const left = valueFor(a, sortKey);
      const right = valueFor(b, sortKey);
      const order = left < right ? -1 : left > right ? 1 : 0;
      return sortDescending ? -order : order;
    });
  $: visibleStart = Math.max(0, Math.floor(viewportTop / ROW_HEIGHT) - 8);
  $: visibleEnd = Math.min(filtered.length, Math.ceil((viewportTop + viewportHeight) / ROW_HEIGHT) + 8);
  $: visible = filtered.slice(visibleStart, visibleEnd);
  $: filterNeedle = filter.trim().toLowerCase();
  $: selected = selectedItem?.record;
  $: selectedIsDetached = selection.kind === "ready" && selection.detached;
  $: selectedAttempts = selected ? attemptRecords(selected) : [];
  $: waterfallTotal = selected
    ? Math.max(selected.timeline.total_us, ...selectedAttempts.map((attempt) => attempt.ended_us || 0))
    : 0;
  $: timelineEntries = selected
    ? timelineFields
        .map((field) => ({ ...field, value: timelineValue(selected.timeline, field.key) }))
        .filter((entry): entry is TimelineField & { value: number } => entry.value !== undefined)
    : [];

  onMount(() => {
    widths = loadWidths();
    const resize = () => {
      viewportHeight = Math.max(260, tableWrap?.clientHeight ?? window.innerHeight - 168);
    };
    const tableResizeObserver = new ResizeObserver(resize);
    resize();
    if (tableWrap) tableResizeObserver.observe(tableWrap);
    window.addEventListener("resize", resize);
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
      tableResizeObserver.disconnect();
      window.removeEventListener("resize", resize);
      window.removeEventListener("hashchange", handleHashChange);
      window.clearInterval(operatorTimer);
      window.clearTimeout(noticeTimer);
      streamSupervisor?.dispose();
      stopResize();
      cancelLiveFlush();
      for (const url of downloadUrls) URL.revokeObjectURL(url);
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

  function loadWidths(): Widths {
    try {
      const parsed = JSON.parse(localStorage.getItem(widthKey) ?? "{}") as Widths;
      return parsed && typeof parsed === "object" ? parsed : {};
    } catch {
      return {};
    }
  }

  function persistWidths(next: Widths) {
    widths = next;
    try {
      localStorage.setItem(widthKey, JSON.stringify(next));
    } catch {
      // Width persistence is optional and must not interrupt inspection.
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
      resetTableViewport();
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
      resetTableViewport();
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
          candidate.record.started_at_unix_ms > current.record.started_at_unix_ms ? candidate : current
        );
        selectVersioned(newest, false, projectionEpoch, nextSelectionRequest());
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
        selectVersioned(canonical, true, projectionEpoch, nextSelectionRequest());
      }
    }
    if (!paused && result.changed) selection = markSelectionDetached(selection, displayRecords);
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
    resetTableViewport();
  }

  function resetTableViewport() {
    viewportTop = 0;
    if (tableWrap) tableWrap.scrollTop = 0;
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
      const runtime = (await response.json()) as { inspector_retained_requests?: unknown };
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

  function matchesFilter(record: InspectorRecord, needle: string): boolean {
    if (!needle) return true;
    return [
      record.record_id,
      record.status,
      record.route,
      record.identity,
      record.requested_model,
      record.public_model,
      record.backend_model,
      record.backend,
      record.outcome.kind,
      record.outcome.stage,
      record.user_agent,
      record.error_kind,
      record.exposed_backend_error ? "exposed" : ""
    ]
      .filter(Boolean)
      .join(" ")
      .toLowerCase()
      .includes(needle);
  }

  function valueFor(record: InspectorRecord, key: ColumnKey): string | number {
    if (key === "time") return record.started_at_unix_ms;
    if (key === "status") return record.status;
    if (key === "total") return record.timeline.total_us;
    if (key === "route") return record.route;
    if (key === "model") return record.public_model || record.requested_model;
    if (key === "backend") return record.backend;
    if (key === "exposed") return record.exposed_backend_error ? 1 : 0;
    return formatOutcome(record);
  }

  function formatTime(timestamp: number): string {
    if (!timestamp) return "-";
    const date = new Date(timestamp);
    return Number.isNaN(date.getTime()) ? "-" : timeFormatter.format(date);
  }

  function formatDate(timestamp: number): string {
    if (!timestamp) return "-";
    const date = new Date(timestamp);
    return Number.isNaN(date.getTime()) ? "-" : date.toISOString();
  }

  function formatMs(microseconds: number | undefined): string {
    if (microseconds === undefined) return "-";
    const milliseconds = microseconds / 1000;
    if (milliseconds < 10) return `${milliseconds.toFixed(2)} ms`;
    if (milliseconds < 100) return `${milliseconds.toFixed(1)} ms`;
    return `${Math.round(milliseconds).toLocaleString()} ms`;
  }

  function formatBytes(bytes: number | undefined): string {
    return bytes === undefined ? "-" : `${bytes.toLocaleString()} B`;
  }

  function formatOutcome(record: InspectorRecord): string {
    return record.outcome.stage ? `${record.outcome.kind}:${record.outcome.stage}` : record.outcome.kind;
  }

  function statusTone(status: number): string {
    if (status >= 500) return "bad";
    if (status >= 400) return "warn";
    if (status >= 200 && status < 300) return "good";
    return "neutral";
  }

  function columnTemplate(currentWidths: Widths): string {
    return columns
      .map((column) => {
        const width = `${columnWidth(column.key, currentWidths)}px`;
        return column.key === "outcome" ? `minmax(${width}, 1fr)` : width;
      })
      .join(" ");
  }

  function tableMinimumWidth(currentWidths: Widths): number {
    return columns.reduce((total, column) => total + columnWidth(column.key, currentWidths), 0);
  }

  function selectVersioned(
    item: VersionedRecord,
    updateHash: boolean,
    epoch: number,
    _requestToken: number
  ) {
    selection = reconcileSelection(
      selection,
      item.record_id,
      item,
      epoch,
      displayRecords
    );
    expandedAttempts = new Set();
    if (updateHash) history.replaceState(null, "", `#${encodeURIComponent(item.record_id)}`);
  }

  function selectDisplayed(recordId: string) {
    const requestToken = nextSelectionRequest();
    const item = displayRecords.get(recordId);
    if (item) selectVersioned(item, true, displayEpoch, requestToken);
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
      expandedAttempts = new Set();
    }
    if (updateHash) history.replaceState(null, "", `#${encodeURIComponent(recordId)}`);

    const displayed = displayRecords.get(recordId);
    if (displayed) {
      selectVersioned(displayed, false, displayEpoch, requestToken);
      if (selection.kind !== "ready" || !selection.detached) return;
    }
    if (paused) return;

    const requestEpoch = projectionEpoch;
    try {
      const response = await fetch(
        `/_onair/inspector-next/requests/${encodeURIComponent(recordId)}`,
        { cache: "no-store" }
      );
      if (!response.ok) throw new Error(`record ${recordId} is not retained`);
      const fetched = decodeVersionedRecord(await response.json());
      if (!fetched || fetched.record_id !== recordId) {
        throw new Error(`record ${recordId} has an invalid shape`);
      }
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

  function sort(key: ColumnKey) {
    if (sortKey === key) sortDescending = !sortDescending;
    else {
      sortKey = key;
      sortDescending = key === "time";
    }
  }

  function resetWidths() {
    widths = {};
    try {
      localStorage.removeItem(widthKey);
    } catch {
      // Width persistence is optional.
    }
  }

  function resetColumn(key: ColumnKey) {
    const next = { ...widths };
    delete next[key];
    persistWidths(next);
  }

  function adjustWidth(key: ColumnKey, delta: number) {
    const column = columns.find((item) => item.key === key)!;
    persistWidths({
      ...widths,
      [key]: Math.max(column.minWidth, Math.min(column.maxWidth, columnWidth(key, widths) + delta))
    });
  }

  function startResize(event: PointerEvent, key: ColumnKey) {
    event.preventDefault();
    const element = event.currentTarget as HTMLElement;
    element.setPointerCapture(event.pointerId);
    resizeStart = {
      key,
      x: event.clientX,
      width: element.parentElement?.getBoundingClientRect().width ?? columnWidth(key, widths),
      pointerId: event.pointerId,
      element
    };
    document.body.classList.add("resizing-columns");
    window.addEventListener("pointermove", moveResize);
    window.addEventListener("pointerup", stopResize);
    window.addEventListener("pointercancel", stopResize);
  }

  function moveResize(event: PointerEvent) {
    if (!resizeStart) return;
    const column = columns.find((item) => item.key === resizeStart!.key)!;
    widths = {
      ...widths,
      [column.key]: Math.max(
        column.minWidth,
        Math.min(column.maxWidth, resizeStart!.width + event.clientX - resizeStart!.x)
      )
    };
  }

  function stopResize() {
    if (resizeStart) {
      persistWidths(widths);
      try {
        if (resizeStart.element.hasPointerCapture(resizeStart.pointerId)) {
          resizeStart.element.releasePointerCapture(resizeStart.pointerId);
        }
      } catch {
        // Pointer capture may already be released by the browser.
      }
    }
    resizeStart = undefined;
    window.removeEventListener("pointermove", moveResize);
    window.removeEventListener("pointerup", stopResize);
    window.removeEventListener("pointercancel", stopResize);
    document.body.classList.remove("resizing-columns");
  }

  function showNotice(message: string, persistent = false) {
    actionNotice = message;
    window.clearTimeout(noticeTimer);
    if (!persistent) noticeTimer = window.setTimeout(() => (actionNotice = ""), 3600);
  }

  function timelineValue(timeline: Timeline, key: keyof Timeline): number | undefined {
    const value = timeline[key];
    return typeof value === "number" ? value : undefined;
  }

  function timelinePercent(value: number, total: number): number {
    return total > 0 ? Math.max(1, Math.min(100, (value / total) * 100)) : 1;
  }

  function attemptRecords(record: InspectorRecord): InspectorAttempt[] {
    return record.backend_attempts;
  }

  function phaseValue(attempt: InspectorAttempt, key: keyof InspectorAttempt): number | undefined {
    const value = attempt[key];
    return typeof value === "number" ? value : undefined;
  }

  function attemptKey(attempt: InspectorAttempt, index: number): string {
    return `${attempt.attempt || index + 1}:${attempt.backend}`;
  }

  function isAttemptExpanded(attempt: InspectorAttempt, index: number): boolean {
    return expandedAttempts.has(attemptKey(attempt, index));
  }

  function toggleAttempt(attempt: InspectorAttempt, index: number) {
    const next = new Set(expandedAttempts);
    const key = attemptKey(attempt, index);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    expandedAttempts = next;
  }

  function toggleAllAttempts(expanded: boolean) {
    expandedAttempts = expanded
      ? new Set(selectedAttempts.map((attempt, index) => attemptKey(attempt, index)))
      : new Set();
  }

  function attemptDetails(attempt: InspectorAttempt): [string, string][] {
    return [
      ["backend target", attempt.backend_target],
      ["backend remote", attempt.backend_remote_addr ?? "-"],
      ["status", String(attempt.status)],
      ["upstream status", attempt.upstream_status === undefined ? "-" : String(attempt.upstream_status)],
      ["outcome", attempt.outcome],
      ["error kind", attempt.error_kind ?? "-"],
      ["elapsed", formatMs(attempt.elapsed_us)],
      ["debug capture", attempt.debug_capture_id ?? "-"]
    ];
  }

  function attemptPhases(attempt: InspectorAttempt): { label: string; value: number }[] {
    return phaseFields
      .map((field) => ({ label: field.label, value: phaseValue(attempt, field.key) }))
      .filter((phase): phase is { label: string; value: number } => phase.value !== undefined);
  }

  function prettyJson(record: InspectorRecord): string {
    return `${JSON.stringify(record, null, 2)}\n`;
  }

  async function copyRecord(record: InspectorRecord) {
    const text = prettyJson(record);
    try {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(text);
      } else {
        const textarea = document.createElement("textarea");
        textarea.value = text;
        textarea.readOnly = true;
        textarea.style.position = "fixed";
        textarea.style.opacity = "0";
        document.body.appendChild(textarea);
        try {
          textarea.select();
          if (!document.execCommand("copy")) throw new Error("copy command failed");
        } finally {
          textarea.remove();
        }
      }
      showNotice("record JSON copied");
    } catch (error) {
      showNotice(error instanceof Error ? error.message : "copy failed", true);
    }
  }

  function downloadRecord(record: InspectorRecord) {
    const blob = new Blob([prettyJson(record)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    downloadUrls.add(url);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `onair-request-${record.record_id}.json`;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
    window.setTimeout(() => {
      URL.revokeObjectURL(url);
      downloadUrls.delete(url);
    }, 0);
    showNotice("record JSON downloaded");
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
        <input aria-label="filter records" placeholder="filter records" value={filter} on:input={updateFilter} />
      </label>
      <button
        type="button"
        class="pause-action"
        class:active={paused}
        aria-label={paused ? "Resume table updates" : "Pause table updates"}
        aria-pressed={paused}
        on:click={togglePause}
      >{paused ? "resume" : "pause"}</button>
      <button type="button" on:click={refreshInspector}>refresh</button>
      <button type="button" on:click={resetWidths}>reset widths</button>
    </div>
  </header>

  <section class="status-strip" aria-label="inspector status">
    <span><strong>{displayRecords.size.toLocaleString()}</strong> loaded</span>
    <span><strong>{retainedCount === undefined ? "—" : retainedCount.toLocaleString()}</strong> retained</span>
    {#if paused}<span><strong>view frozen</strong></span>{/if}
    {#if paused && pausedUpdateCount > 0}
      <span><strong>{pausedUpdateCount.toLocaleString()}</strong> {pausedUpdateCount === 1 ? "update" : "updates"} while paused</span>
    {/if}
    <span
      class:status-live={streamPresentation.tone === "good"}
      class:status-offline={streamPresentation.tone !== "good"}
      class:status-error={streamPresentation.tone === "error"}
    >{streamStatusLabel}</span>
  </section>

  {#if actionNotice}
    <div class="notices" aria-live="polite">
      <div class="notice notice-info">{actionNotice}</div>
    </div>
  {/if}

  <section class="workspace">
    <div
      class="table-panel"
      class:view-frozen={paused}
      data-view-state={paused ? "frozen" : "live"}
      aria-label={paused ? "Request table, view frozen" : "Request table"}
      aria-describedby={paused ? "frozen-view-status" : undefined}
      style={`--column-template: ${columnTemplate(widths)}; --table-min-width: ${tableMinimumWidth(widths)}px`}
    >
      <div class="table-header-viewport">
        <table class="table-header" style={`transform: translateX(${-viewportLeft}px)`}>
        <thead>
          <tr>
            {#each columns as column}
              <th
                class:right={column.align === "right"}
                class:resizable={column.minWidth < column.maxWidth}
                aria-sort={sortKey === column.key ? (sortDescending ? "descending" : "ascending") : "none"}
              >
                <button
                  type="button"
                  class="sort"
                  aria-label={`Sort by ${column.label}`}
                  on:click={() => sort(column.key)}
                >
                  <span class="sort-label">{column.label}</span>
                  {#if sortKey === column.key}
                    <span class="sort-indicator" aria-hidden="true">{sortDescending ? "↓" : "↑"}</span>
                  {/if}
                </button>
                {#if column.minWidth < column.maxWidth}
                  <button
                    type="button"
                    class="resize"
                    aria-label={`Resize ${column.label}`}
                    aria-keyshortcuts="ArrowLeft ArrowRight Enter"
                    title={`Resize ${column.label}; double-click to reset`}
                    on:pointerdown={(event) => startResize(event, column.key)}
                    on:keydown={(event) => {
                      if (event.key === "ArrowLeft") {
                        event.preventDefault();
                        adjustWidth(column.key, -8);
                      } else if (event.key === "ArrowRight") {
                        event.preventDefault();
                        adjustWidth(column.key, 8);
                      } else if (event.key === "Enter") {
                        event.preventDefault();
                        resetColumn(column.key);
                      }
                    }}
                    on:dblclick={() => resetColumn(column.key)}
                  ></button>
                {/if}
              </th>
            {/each}
          </tr>
        </thead>
        </table>
      </div>
      {#if paused}
        <div class="frozen-view-indicator" id="frozen-view-status">view frozen</div>
      {/if}
      <div
        class="table-wrap"
        bind:this={tableWrap}
        on:scroll={(event) => {
          const target = event.currentTarget as HTMLElement;
          viewportTop = target.scrollTop;
          viewportHeight = target.clientHeight;
          viewportLeft = target.scrollLeft;
        }}
      >
        <table aria-label="Inspector requests" aria-busy={!streamReady}>
        <tbody style={`height: ${filtered.length * ROW_HEIGHT}px`}>
          <tr class="spacer" aria-hidden="true" style={`height: ${visibleStart * ROW_HEIGHT}px`}>
            <td colspan={columns.length}></td>
          </tr>
          {#each visible as record (record.record_id)}
            <tr
              class:selected={record.record_id === selectedId}
              class:detached={record.record_id === selectedId && selectedIsDetached}
              aria-selected={record.record_id === selectedId}
              aria-label={`Inspect request ${record.record_id}`}
              on:click={() => selectDisplayed(record.record_id)}
              on:keydown={(event) => {
                if (event.key === "Enter" || event.key === " ") {
                  event.preventDefault();
                  selectDisplayed(record.record_id);
                }
              }}
              tabindex="0"
            >
              <td title={formatDate(record.started_at_unix_ms)}>{formatTime(record.started_at_unix_ms)}</td>
              <td class={`right status-cell ${statusTone(record.status)}`}>{record.status || "-"}</td>
              <td class="right numeric">{formatMs(record.timeline.total_us)}</td>
              <td>{record.route || "-"}</td>
              <td title={record.public_model || record.requested_model}>{record.public_model || record.requested_model || "-"}</td>
              <td title={record.backend}>{record.backend || "-"}</td>
              <td title={formatOutcome(record)}>{formatOutcome(record)}</td>
              <td class="right exposed-cell">
                {#if record.exposed_backend_error}<span class="exposed-marker" title="upstream error body exposed">yes</span>{:else}<span class="muted">—</span>{/if}
              </td>
            </tr>
          {/each}
        </tbody>
        </table>
        {#if filtered.length === 0}
          <div class="empty-state">
            {#if !streamReady}Waiting for inspector stream…{:else if filter.trim()}No records match the current filter.{:else}No retained records.{/if}
          </div>
        {/if}
      </div>
    </div>

    <aside
      class="detail-pane"
      aria-label="Selected request details"
      data-detail-state={detailViewState}
    >
      <div class="detail-heading">
        <div>
          <div class="eyebrow">
            {selection.kind === "loading"
              ? "loading request"
              : selection.kind === "error"
                ? "request lookup error"
                : "selected request"}
          </div>
          {#if selection.kind === "none"}
            <h2>record detail</h2>
          {:else if selection.kind === "loading" || selection.kind === "error"}
            <h2 class="detail-record-id" title={selection.recordId}>{selection.recordId}</h2>
          {:else}
            <h2 class="detail-record-id" title={selection.item.record_id}>{selection.item.record_id}</h2>
            <p class="detail-revision">revision <span class="numeric">{selection.item.revision}</span></p>
          {/if}
        </div>
        {#if selectedIsDetached}
          <span
            class="detached-badge"
            aria-label="Detached: pinned revision is outside the current live table window"
            title="Pinned revision is outside the current live table window"
          >detached</span>
        {/if}
      </div>

      {#if selection.kind === "loading"}
        <p class="empty-detail">Loading record <span class="detail-target-id">{selection.recordId}</span>…</p>
      {:else if selection.kind === "error"}
        <p class="empty-detail error-text">{selection.message}</p>
      {:else if selection.kind === "ready" && selected}
        <div class="detail-actions">
          <button type="button" on:click={() => void copyRecord(selected)}>copy JSON</button>
          <button type="button" on:click={() => downloadRecord(selected)}>download JSON</button>
        </div>

        <section class="detail-card">
          <h3>request</h3>
          <dl class="kv">
            <dt>record id</dt><dd class="break">{selected.record_id}</dd>
            <dt>client request id</dt><dd class="break">{selected.client_request_id || "—"}</dd>
            <dt>started</dt><dd class="numeric">{formatDate(selected.started_at_unix_ms)}</dd>
            <dt>completed</dt><dd class="numeric">{formatDate(selected.completed_at_unix_ms)}</dd>
            <dt>method</dt><dd>{selected.method}</dd>
            <dt>path</dt><dd class="break">{selected.query ? `${selected.path}?${selected.query}` : selected.path}</dd>
            <dt>route</dt><dd>{selected.route || "—"}</dd>
            <dt>identity</dt><dd class="break">{selected.identity || "—"}</dd>
            <dt>status</dt><dd class={`status-value ${statusTone(selected.status)}`}>{selected.status}</dd>
            <dt>outcome</dt><dd class="break">{formatOutcome(selected)}</dd>
            <dt>error kind</dt><dd class="break">{selected.error_kind || "—"}</dd>
            <dt>exposed error</dt><dd>{selected.exposed_backend_error ? "yes" : "no"}</dd>
          </dl>
        </section>

        <section class="detail-card">
          <h3>routing</h3>
          <dl class="kv">
            <dt>requested model</dt><dd class="break">{selected.requested_model || "—"}</dd>
            <dt>public model</dt><dd class="break">{selected.public_model || "—"}</dd>
            <dt>backend model</dt><dd class="break">{selected.backend_model || "—"}</dd>
            <dt>backend</dt><dd>{selected.backend || "—"}</dd>
            <dt>target</dt><dd class="break">{selected.backend_target || "—"}</dd>
            <dt>remote</dt><dd class="break">{selected.backend_remote_addr || "—"}</dd>
            <dt>stream</dt><dd>{selected.stream ? "yes" : "no"}</dd>
          </dl>
        </section>

        <section class="detail-card">
          <h3>client</h3>
          <dl class="kv">
            <dt>peer</dt><dd class="break">{selected.peer_addr || "—"}</dd>
            <dt>effective client</dt><dd class="break">{selected.effective_client_addr || "—"}</dd>
            <dt>trusted proxy</dt><dd class="break">{selected.trusted_proxy_addr || "—"}</dd>
            <dt>forwarded for</dt><dd class="break">{selected.forwarded_for || "—"}</dd>
            <dt>user-agent</dt><dd class="break">{selected.user_agent || "—"}</dd>
          </dl>
        </section>

        <section class="detail-card">
          <h3>sizes and usage</h3>
          <dl class="kv">
            <dt>request body</dt><dd class="numeric">{formatBytes(selected.request_body_bytes)}</dd>
            <dt>response body</dt><dd class="numeric">{formatBytes(selected.response_body_bytes)}</dd>
            <dt>input tokens</dt><dd class="numeric">{selected.input_tokens.toLocaleString()}</dd>
            <dt>cached input</dt><dd class="numeric">{selected.cached_input_tokens.toLocaleString()}</dd>
            <dt>output tokens</dt><dd class="numeric">{selected.output_tokens.toLocaleString()}</dd>
            <dt>debug capture</dt><dd class="break">{selected.debug_capture_id || "—"}</dd>
          </dl>
        </section>

        <section class="detail-card">
          <div class="card-heading-row"><h3>timeline</h3><span class="numeric">{formatMs(selected.timeline.total_us)}</span></div>
          <div class="timeline">
            {#each timelineEntries as entry}
              <div class="timeline-row">
                <span class="timeline-label">{entry.label}</span>
                <span class="timeline-value numeric">{formatMs(entry.value)}</span>
                <span class="timeline-track"><span class="timeline-fill" style={`width: ${timelinePercent(entry.value, selected.timeline.total_us)}%`}></span></span>
              </div>
            {/each}
          </div>
        </section>

        {#if selectedAttempts.length}
          <section class="detail-card">
            <div class="card-heading-row">
              <h3>attempt waterfall</h3>
              <span class="muted">{selectedAttempts.length} attempt{selectedAttempts.length === 1 ? "" : "s"}</span>
            </div>
            <div class="waterfall-actions">
              <button type="button" on:click={() => toggleAllAttempts(true)}>expand all</button>
              <button type="button" on:click={() => toggleAllAttempts(false)}>collapse all</button>
            </div>
            <div class="waterfall-scale"><span>request</span><span class="numeric">0 ms · {formatMs(waterfallTotal / 2)} · {formatMs(waterfallTotal)}</span></div>
            <div class="waterfall">
              {#each selectedAttempts as attempt, index}
                <div class:expanded={isAttemptExpanded(attempt, index)} class="attempt">
                  <div class="attempt-heading">
                    <div>
                      <strong>#{attempt.attempt} {attempt.backend || "unknown"}</strong>
                      <span class="attempt-summary">{attempt.outcome} · status {attempt.status} · {formatMs(attempt.elapsed_us)}</span>
                    </div>
                    <button
                      type="button"
                      class="compact-action"
                      aria-expanded={isAttemptExpanded(attempt, index)}
                      aria-controls={`attempt-details-${index}`}
                      on:click={() => toggleAttempt(attempt, index)}
                    >
                      {isAttemptExpanded(attempt, index) ? "hide details" : "details"}
                    </button>
                  </div>
                  <div class="attempt-track">
                    <span
                      class={`attempt-span ${statusTone(attempt.status)}`}
                      style={`left: ${timelinePercent(attempt.started_us, waterfallTotal)}%; width: ${Math.max(0.8, timelinePercent(Math.max(0, attempt.ended_us - attempt.started_us), waterfallTotal))}%`}
                      title={`${attempt.backend_target} · ${formatMs(attempt.elapsed_us)}`}
                    ></span>
                    {#each attemptPhases(attempt) as phase}
                      <span class="attempt-tick" style={`left: ${timelinePercent(phase.value, waterfallTotal)}%`} title={`${phase.label}: ${formatMs(phase.value)}`}></span>
                    {/each}
                  </div>
                  {#if isAttemptExpanded(attempt, index)}
                    <div class="attempt-details" id={`attempt-details-${index}`}>
                      <dl class="kv">
                        {#each attemptDetails(attempt) as pair}
                          <dt>{pair[0]}</dt><dd class="break">{pair[1]}</dd>
                        {/each}
                      </dl>
                      {#if attemptPhases(attempt).length}
                        <div class="phase-list">
                          {#each attemptPhases(attempt) as phase}<span>{phase.label} {formatMs(phase.value)}</span>{/each}
                        </div>
                      {/if}
                    </div>
                  {/if}
                </div>
              {/each}
            </div>
          </section>
        {/if}

        {#if selected.retried_attempts.length}
          <section class="detail-card">
            <h3>retried attempts</h3>
            <dl class="kv">
              {#each selected.retried_attempts as attempt}
                <dt>#{attempt.attempt} {attempt.backend}</dt>
                <dd class="break">{attempt.outcome} · status {attempt.status} · {formatMs(attempt.elapsed_us)}{attempt.error_kind ? ` · ${attempt.error_kind}` : ""}</dd>
              {/each}
            </dl>
          </section>
        {/if}

        <section class="detail-card">
          <div class="card-heading-row"><h3>raw JSON</h3><span class="muted">record only</span></div>
          <pre>{prettyJson(selected)}</pre>
        </section>
      {:else}
        <p class="empty-detail">Select a record to inspect its metadata and timings.</p>
      {/if}
    </aside>
  </section>
</main>
