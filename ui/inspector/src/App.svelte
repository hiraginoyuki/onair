<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import {
    applyEvent,
    CLIENT_LIMIT,
    columns,
    columnWidth,
    eventSequence,
    PENDING_LIMIT,
    ROW_HEIGHT
  } from "./lib/store";
  import type {
    ColumnKey,
    InspectorAttempt,
    InspectorRecord,
    StreamEvent,
    Timeline
  } from "./lib/types";

  type StoredRecord = { revision: number; record: InspectorRecord };
  type Widths = Partial<Record<ColumnKey, number>>;
  type TimelineField = { key: keyof Timeline; label: string };
  type PhaseField = { key: keyof InspectorAttempt; label: string };

  const widthKey = "onair.inspector.v4.widths";
  const sequenceKey = "onair.inspector.v2.seq";
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

  let records = new Map<string, StoredRecord>();
  let pending: StreamEvent[] = [];
  let selectedId = readHashRecordId();
  let detachedSelected: StoredRecord | undefined;
  let filter = "";
  let sortKey: ColumnKey = "time";
  let sortDescending = true;
  let paused = false;
  let connected = false;
  let malformedStream = false;
  let droppedPending = false;
  let recoveryNotice = "";
  let actionNotice = "";
  let detailLoading = false;
  let detailError = "";
  let retainedCount: number | undefined;
  let lastSequence = readLastSequence();
  let widths: Widths = loadWidths();
  let viewportTop = 0;
  let viewportHeight = 480;
  let source: EventSource | undefined;
  let resizeStart: { key: ColumnKey; x: number; width: number } | undefined;
  let operatorTimer: number | undefined;
  let noticeTimer: number | undefined;
  let expandedAttempts = new Set<string>();
  const downloadUrls = new Set<string>();

  $: filtered = Array.from(records.values())
    .map((entry) => entry.record)
    .filter(matchesFilter)
    .sort((a, b) => {
      const left = valueFor(a, sortKey);
      const right = valueFor(b, sortKey);
      const order = left < right ? -1 : left > right ? 1 : 0;
      return sortDescending ? -order : order;
    });
  $: visibleStart = Math.max(0, Math.floor(viewportTop / ROW_HEIGHT) - 8);
  $: visibleEnd = Math.min(filtered.length, Math.ceil((viewportTop + viewportHeight) / ROW_HEIGHT) + 8);
  $: visible = filtered.slice(visibleStart, visibleEnd);
  $: selected = detachedSelected?.record ?? (selectedId ? records.get(selectedId)?.record : undefined);
  $: selectedIsDetached = Boolean(detachedSelected && detachedSelected.record.record_id === selectedId);
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
      viewportHeight = Math.max(260, window.innerHeight - 246);
    };
    resize();
    window.addEventListener("resize", resize);
    window.addEventListener("hashchange", handleHashChange);
    connect();
    void refreshRuntime();
    operatorTimer = window.setInterval(() => void refreshRuntime(), 15_000);
    if (selectedId) void loadSelectedById(selectedId, false);
    return () => {
      window.removeEventListener("resize", resize);
      window.removeEventListener("hashchange", handleHashChange);
      window.clearInterval(operatorTimer);
      window.clearTimeout(noticeTimer);
      source?.close();
      stopResize();
      for (const url of downloadUrls) URL.revokeObjectURL(url);
    };
  });

  onDestroy(() => {
    source?.close();
    stopResize();
  });

  function readLastSequence(): number {
    try {
      const value = Number(sessionStorage.getItem(sequenceKey) ?? "0");
      return Number.isFinite(value) && value > 0 ? value : 0;
    } catch {
      return 0;
    }
  }

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

  function connect() {
    source?.close();
    source = new EventSource(`/_onair/inspector-next/events?snapshot_limit=${CLIENT_LIMIT}`);
    source.onopen = () => {
      connected = true;
      malformedStream = false;
    };
    source.onerror = () => {
      connected = false;
    };
    for (const name of ["snapshot", "record_upsert", "record_removed", "reset", "keepalive"]) {
      source.addEventListener(name, (message) => {
        const data = (message as MessageEvent).data;
        try {
          const event: unknown = normalizeStreamEvent(JSON.parse(data));
          if (!isStreamEvent(event)) throw new Error("invalid event envelope");
          handleEvent(event);
        } catch {
          malformedStream = true;
          showNotice("stream warning: malformed event ignored", false);
        }
      });
    }
  }

  function normalizeStreamEvent(event: unknown): unknown {
    if (!event || typeof event !== "object") return event;
    const candidate = event as Record<string, unknown>;
    if (candidate.kind === "snapshot" && Array.isArray(candidate.records)) {
      return {
        ...candidate,
        records: candidate.records.map((entry) => {
          if (!entry || typeof entry !== "object") return entry;
          const snapshot = entry as Record<string, unknown>;
          return { ...snapshot, record: normalizeInspectorRecord(snapshot.record) };
        })
      };
    }
    if (candidate.kind === "record_upsert") {
      return { ...candidate, record: normalizeInspectorRecord(candidate.record) };
    }
    return event;
  }

  function normalizeInspectorRecord(record: unknown): unknown {
    if (!record || typeof record !== "object") return record;
    const candidate = record as Record<string, unknown>;
    return {
      ...candidate,
      backend_attempts: Array.isArray(candidate.backend_attempts) ? candidate.backend_attempts : [],
      retried_attempts: Array.isArray(candidate.retried_attempts) ? candidate.retried_attempts : [],
      exposed_backend_error:
        candidate.exposed_backend_error === undefined ? false : candidate.exposed_backend_error
    };
  }

  function isStreamEvent(event: unknown): event is StreamEvent {
    if (!event || typeof event !== "object") return false;
    const candidate = event as { kind?: unknown; stream_seq?: unknown };
    if (
      typeof candidate.kind !== "string" ||
      !Number.isFinite(candidate.stream_seq) ||
      !["snapshot", "record_upsert", "record_removed", "reset", "keepalive"].includes(candidate.kind)
    ) {
      return false;
    }
    if (candidate.kind === "snapshot") {
      const records = (event as { records?: unknown }).records;
      return (
        Array.isArray(records) &&
        records.every((entry) => {
          if (!entry || typeof entry !== "object") return false;
          const snapshot = entry as { record_id?: unknown; revision?: unknown; record?: unknown };
          return (
            typeof snapshot.record_id === "string" &&
            typeof snapshot.revision === "number" &&
            Number.isFinite(snapshot.revision) &&
            isInspectorRecord(snapshot.record)
          );
        })
      );
    }
    if (candidate.kind === "record_upsert") {
      const upsert = event as {
        record_id?: unknown;
        revision?: unknown;
        phase?: unknown;
        record?: unknown;
      };
      return (
        typeof upsert.record_id === "string" &&
        typeof upsert.revision === "number" &&
        Number.isFinite(upsert.revision) &&
        ["initial", "live", "terminal"].includes(upsert.phase as string) &&
        isInspectorRecord(upsert.record)
      );
    }
    if (candidate.kind === "record_removed") {
      const removed = event as { record_id?: unknown; revision?: unknown };
      return (
        typeof removed.record_id === "string" &&
        typeof removed.revision === "number" &&
        Number.isFinite(removed.revision)
      );
    }
    if (candidate.kind === "reset") {
      return ["resume_unavailable", "lagged", "server_restarted"].includes(
        (event as { reason?: unknown }).reason as string
      );
    }
    return true;
  }

  function isInspectorRecord(record: unknown): record is InspectorRecord {
    if (!record || typeof record !== "object") return false;
    const candidate = record as Partial<InspectorRecord>;
    const timeline = candidate.timeline;
    const outcome = candidate.outcome;
    return Boolean(
      typeof candidate.record_id === "string" &&
        typeof candidate.started_at_unix_ms === "number" &&
        typeof candidate.method === "string" &&
        typeof candidate.path === "string" &&
        typeof candidate.route === "string" &&
        typeof candidate.identity === "string" &&
        typeof candidate.requested_model === "string" &&
        typeof candidate.public_model === "string" &&
        typeof candidate.backend_model === "string" &&
        typeof candidate.backend === "string" &&
        typeof candidate.backend_target === "string" &&
        typeof candidate.stream === "boolean" &&
        typeof candidate.peer_addr === "string" &&
        typeof candidate.effective_client_addr === "string" &&
        typeof candidate.trusted_proxy_addr === "string" &&
        typeof candidate.forwarded_for === "string" &&
        typeof candidate.user_agent === "string" &&
        typeof candidate.request_body_bytes === "number" &&
        typeof candidate.exposed_backend_error === "boolean" &&
        typeof candidate.status === "number" &&
        Array.isArray(candidate.backend_attempts) &&
        Array.isArray(candidate.retried_attempts) &&
        typeof candidate.input_tokens === "number" &&
        typeof candidate.cached_input_tokens === "number" &&
        typeof candidate.output_tokens === "number" &&
        typeof candidate.completed_at_unix_ms === "number" &&
        timeline &&
        typeof timeline.started_unix_ms === "number" &&
        typeof timeline.total_us === "number" &&
        typeof timeline.proxy_entry_us === "number" &&
        outcome &&
        typeof outcome.kind === "string"
    );
  }

  function handleEvent(event: StreamEvent) {
    const previousSelected = selectedId ? records.get(selectedId) : undefined;
    lastSequence = Math.max(lastSequence, eventSequence(event));
    try {
      sessionStorage.setItem(sequenceKey, String(lastSequence));
    } catch {
      // Session persistence is a resume hint, not a source of truth.
    }

    if (event.kind === "reset") {
      pending = [];
      droppedPending = false;
      recoveryNotice = `stream reset: ${event.reason.replaceAll("_", " ")}`;
      showNotice("stream reset; snapshot reloaded", false);
      void refreshRuntime();
    }
    const result = applyEvent(records, event, paused, pending);
    if (event.kind === "snapshot") {
      recoveryNotice = "";
      if (selectedId && !records.has(selectedId) && !detachedSelected) {
        void loadSelectedById(selectedId, false);
      }
    }
    if (result.droppedPending) droppedPending = true;
    pending = [...pending];
    records = new Map(records);

    if (event.kind === "snapshot" && selectedId && records.has(selectedId)) {
      detachedSelected = undefined;
      detailError = "";
    }
    if (event.kind === "record_upsert") {
      if (
        event.record_id === selectedId &&
        detachedSelected &&
        event.revision > detachedSelected.revision
      ) {
        detachedSelected = undefined;
        detailError = "";
      } else if (!selectedId) {
        select(event.record, true);
      }
      if (
        detachedSelected &&
        event.record_id === selectedId &&
        event.revision <= detachedSelected.revision
      ) {
        records.delete(event.record_id);
        records = new Map(records);
      }
    }
    if (selectedId && !records.has(selectedId) && previousSelected && !detachedSelected) {
      detachedSelected = previousSelected;
    }
  }

  function togglePause() {
    paused = !paused;
    if (!paused) {
      const queued = pending.splice(0);
      for (const event of queued) applyEvent(records, event, false, pending);
      pending = [];
      records = new Map(records);
      droppedPending = false;
    }
  }

  async function refreshRuntime() {
    try {
      const response = await fetch("/_onair/operator/runtime", { cache: "no-store" });
      if (!response.ok) throw new Error(`runtime unavailable: HTTP ${response.status}`);
      const runtime = (await response.json()) as { inspector_retained_requests?: number };
      retainedCount = runtime.inspector_retained_requests;
    } catch {
      retainedCount = undefined;
    }
  }

  function matchesFilter(record: InspectorRecord): boolean {
    const needle = filter.trim().toLowerCase();
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
    return timestamp ? new Date(timestamp).toLocaleTimeString() : "-";
  }

  function formatDate(timestamp: number): string {
    return timestamp ? new Date(timestamp).toISOString() : "-";
  }

  function formatMs(microseconds: number | undefined): string {
    if (microseconds === undefined) return "-";
    return `${Math.round(microseconds / 1000)} ms`;
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
    return columns.map((column) => `${columnWidth(column.key, currentWidths)}px`).join(" ");
  }

  function tableMinimumWidth(currentWidths: Widths): number {
    return columns.reduce((total, column) => total + columnWidth(column.key, currentWidths), 0);
  }

  function select(record: InspectorRecord, updateHash = true) {
    selectedId = record.record_id;
    detachedSelected = undefined;
    detailError = "";
    expandedAttempts = new Set();
    if (updateHash) history.replaceState(null, "", `#${encodeURIComponent(record.record_id)}`);
  }

  async function handleHashChange() {
    const target = readHashRecordId();
    if (!target) {
      selectedId = "";
      detachedSelected = undefined;
      detailError = "";
      return;
    }
    await loadSelectedById(target, false);
  }

  async function loadSelectedById(recordId: string, updateHash: boolean) {
    if (!recordId) return;
    selectedId = recordId;
    detachedSelected = undefined;
    detailError = "";
    if (records.has(recordId)) {
      select(records.get(recordId)!.record, updateHash);
      return;
    }
    detailLoading = true;
    try {
      const response = await fetch(`/_onair/inspector/requests/${encodeURIComponent(recordId)}`, {
        cache: "no-store"
      });
      if (!response.ok) throw new Error(`record ${recordId} is not retained`);
      detachedSelected = { revision: 0, record: (await response.json()) as InspectorRecord };
      if (updateHash) history.replaceState(null, "", `#${encodeURIComponent(recordId)}`);
    } catch (error) {
      detailError = error instanceof Error ? error.message : "record lookup failed";
    } finally {
      detailLoading = false;
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
    resizeStart = {
      key,
      x: event.clientX,
      width: element.parentElement?.getBoundingClientRect().width ?? columnWidth(key, widths)
    };
    window.addEventListener("pointermove", moveResize);
    window.addEventListener("pointerup", stopResize, { once: true });
  }

  function moveResize(event: PointerEvent) {
    if (!resizeStart) return;
    const column = columns.find((item) => item.key === resizeStart!.key)!;
    persistWidths({
      ...widths,
      [column.key]: Math.max(
        column.minWidth,
        Math.min(column.maxWidth, resizeStart!.width + event.clientX - resizeStart!.x)
      )
    });
  }

  function stopResize() {
    resizeStart = undefined;
    window.removeEventListener("pointermove", moveResize);
    window.removeEventListener("pointerup", stopResize);
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
      <p class:offline={!connected} class:warning={malformedStream}>
        <span class:online={connected} class="status-dot"></span>
        {malformedStream ? "stream warning" : connected ? "connected" : "reconnecting"}
      </p>
    </div>
    <div class="actions">
      <label class="filter-wrap">
        <span class="sr-only">Filter records</span>
        <input aria-label="filter records" placeholder="filter records" bind:value={filter} />
      </label>
      <button type="button" class:active={paused} on:click={togglePause}>{paused ? "resume" : "pause"}</button>
      <button type="button" on:click={() => void refreshRuntime()}>refresh</button>
      <button type="button" on:click={resetWidths}>reset widths</button>
    </div>
  </header>

  <section class="status-strip" aria-label="inspector status">
    <span><strong>{records.size.toLocaleString()}</strong> loaded</span>
    <span><strong>{retainedCount === undefined ? "—" : retainedCount.toLocaleString()}</strong> retained</span>
    <span><strong>{pending.length.toLocaleString()}</strong> pending</span>
    <span class:status-live={connected} class:status-offline={!connected}>{connected ? "live" : "reconnecting"}</span>
    {#if recoveryNotice}<span class="status-recovery">{recoveryNotice}</span>{/if}
  </section>

  {#if recoveryNotice || actionNotice || droppedPending}
    <div class="notices" aria-live="polite">
      {#if recoveryNotice}<div class="notice notice-warning">{recoveryNotice}</div>{/if}
      {#if actionNotice}<div class="notice notice-info">{actionNotice}</div>{/if}
      {#if droppedPending}<div class="notice notice-warning">pending buffer capped at {PENDING_LIMIT}; oldest updates dropped</div>{/if}
    </div>
  {/if}

  <section class="workspace">
    <div
      class="table-wrap"
      on:scroll={(event) => {
        const target = event.currentTarget as HTMLElement;
        viewportTop = target.scrollTop;
        viewportHeight = target.clientHeight;
      }}
    >
      <table
        aria-label="Inspector requests"
        style={`--column-template: ${columnTemplate(widths)}; --table-min-width: ${tableMinimumWidth(widths)}px`}
      >
        <thead>
          <tr>
            {#each columns as column}
              <th
                class:right={column.align === "right"}
                aria-sort={sortKey === column.key ? (sortDescending ? "descending" : "ascending") : "none"}
              >
                <button
                  type="button"
                  class="sort"
                  aria-label={`Sort by ${column.label}`}
                  on:click={() => sort(column.key)}
                >
                  {column.label}{sortKey === column.key ? (sortDescending ? " ↓" : " ↑") : ""}
                </button>
                <button
                  type="button"
                  class="resize"
                  aria-label={`Resize ${column.label}`}
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
              </th>
            {/each}
          </tr>
        </thead>
        <tbody style={`height: ${filtered.length * ROW_HEIGHT}px`}>
          <tr class="spacer" aria-hidden="true" style={`height: ${visibleStart * ROW_HEIGHT}px`}>
            <td colspan={columns.length}></td>
          </tr>
          {#each visible as record (record.record_id)}
            <tr
              class:selected={record.record_id === selectedId}
              class:detached={record.record_id === selectedId && selectedIsDetached}
              aria-selected={record.record_id === selectedId}
              on:click={() => select(record)}
              on:keydown={(event) => (event.key === "Enter" || event.key === " ") && select(record)}
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
      {#if filtered.length === 0}<div class="empty-state">No records match the current filter.</div>{/if}
    </div>

    <aside class="detail-pane" aria-live="polite">
      <div class="detail-heading">
        <div>
          <div class="eyebrow">selected request</div>
          <h2>record detail</h2>
        </div>
        {#if selectedIsDetached}<span class="detached-badge">detached</span>{/if}
      </div>

      {#if detailLoading}
        <p class="empty-detail">Loading record…</p>
      {:else if detailError}
        <p class="empty-detail error-text">{detailError}</p>
      {:else if selected}
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
                    <button type="button" class="compact-action" on:click={() => toggleAttempt(attempt, index)}>
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
                    <div class="attempt-details">
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
