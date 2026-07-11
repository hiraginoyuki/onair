<script lang="ts">
  import { onMount } from "svelte";
  import { applyEvent, CLIENT_LIMIT, columns, columnWidth, eventSequence, PENDING_LIMIT, ROW_HEIGHT } from "./lib/store";
  import type { ColumnKey, InspectorRecord, StreamEvent } from "./lib/types";

  const widthKey = "onair.inspector.v3.widths";
  let records = new Map<string, { revision: number; record: InspectorRecord }>();
  let pending: StreamEvent[] = [];
  let selectedId = "";
  let filter = "";
  let sortKey: ColumnKey = "time";
  let sortDescending = true;
  let paused = false;
  let connected = false;
  let lagged = false;
  let droppedPending = false;
  let lastSequence = Number(sessionStorage.getItem("onair.inspector.v2.seq") ?? "0");
  let widths: Partial<globalThis.Record<ColumnKey, number>> = loadWidths();
  let viewportTop = 0;
  let viewportHeight = 480;
  let source: EventSource | undefined;
  let resizeStart: { key: ColumnKey; x: number; width: number } | undefined;

  $: filtered = Array.from(records.values())
    .map((entry) => entry.record)
    .filter((record) => {
      const needle = filter.trim().toLowerCase();
      return !needle || [record.record_id, record.status, record.route, record.public_model, record.backend, record.outcome.kind]
        .join(" ")
        .toLowerCase()
        .includes(needle);
    })
    .sort((a, b) => {
      const left = valueFor(a, sortKey);
      const right = valueFor(b, sortKey);
      const order = left < right ? -1 : left > right ? 1 : 0;
      return sortDescending ? -order : order;
    });
  $: visibleStart = Math.max(0, Math.floor(viewportTop / ROW_HEIGHT) - 8);
  $: visibleEnd = Math.min(filtered.length, Math.ceil((viewportTop + viewportHeight) / ROW_HEIGHT) + 8);
  $: visible = filtered.slice(visibleStart, visibleEnd);
  $: selected = selectedId ? records.get(selectedId)?.record : undefined;

  onMount(() => {
    const stored = localStorage.getItem(widthKey);
    if (stored) {
      try { widths = JSON.parse(stored); } catch { widths = {}; }
    }
    const resize = () => { viewportHeight = Math.max(240, window.innerHeight - 238); };
    resize();
    window.addEventListener("resize", resize);
    connect();
    return () => {
      window.removeEventListener("resize", resize);
      source?.close();
    };
  });

  function loadWidths(): Partial<globalThis.Record<ColumnKey, number>> {
    try { return JSON.parse(localStorage.getItem(widthKey) ?? "{}"); } catch { return {}; }
  }

  function connect() {
    source?.close();
    source = new EventSource(`/_onair/inspector-next/events?snapshot_limit=${CLIENT_LIMIT}`);
    source.onopen = () => { connected = true; lagged = false; };
    source.onerror = () => { connected = false; };
    for (const name of ["snapshot", "record_upsert", "record_removed", "reset", "keepalive"]) {
      source.addEventListener(name, (message) => handleEvent(JSON.parse((message as MessageEvent).data) as StreamEvent));
    }
  }

  function handleEvent(event: StreamEvent) {
    lastSequence = eventSequence(event);
    sessionStorage.setItem("onair.inspector.v2.seq", String(lastSequence));
    const result = applyEvent(records, event, paused, pending);
    if (result.reset) lagged = event.kind === "reset";
    if (result.droppedPending) droppedPending = true;
    records = new Map(records);
    if (event.kind === "record_upsert" && !selectedId) selectedId = event.record_id;
  }

  function togglePause() {
    paused = !paused;
    if (!paused) {
      const queued = pending.splice(0);
      for (const event of queued) applyEvent(records, event, false, pending);
      records = new Map(records);
      droppedPending = false;
    }
  }

  function valueFor(record: InspectorRecord, key: ColumnKey): string | number {
    if (key === "time") return record.started_at_unix_ms;
    if (key === "status") return record.status;
    if (key === "total") return Math.round((record.timeline?.total_us ?? 0) / 1000);
    if (key === "route") return record.route;
    if (key === "model") return record.public_model || record.requested_model;
    if (key === "backend") return record.backend;
    return record.outcome.kind;
  }

  function formatTime(timestamp: number) { return timestamp ? new Date(timestamp).toLocaleTimeString() : "-"; }
  function formatOutcome(record: InspectorRecord) { return record.outcome.stage ? `${record.outcome.kind}:${record.outcome.stage}` : record.outcome.kind; }
  function select(record: InspectorRecord) { selectedId = record.record_id; }
  function sort(key: ColumnKey) { if (sortKey === key) sortDescending = !sortDescending; else { sortKey = key; sortDescending = key === "time"; } }
  function resetWidths() { widths = {}; localStorage.removeItem(widthKey); }
  function startResize(event: PointerEvent, key: ColumnKey) {
    const element = event.currentTarget as HTMLElement;
    resizeStart = { key, x: event.clientX, width: element.parentElement?.getBoundingClientRect().width ?? columnWidth(key, widths) };
    window.addEventListener("pointermove", moveResize);
    window.addEventListener("pointerup", stopResize, { once: true });
  }
  function moveResize(event: PointerEvent) {
    if (!resizeStart) return;
    const column = columns.find((item) => item.key === resizeStart!.key)!;
    widths = { ...widths, [column.key]: Math.max(column.minWidth, Math.min(column.maxWidth, resizeStart!.width + event.clientX - resizeStart!.x)) };
    localStorage.setItem(widthKey, JSON.stringify(widths));
  }
  function stopResize() { resizeStart = undefined; window.removeEventListener("pointermove", moveResize); }
</script>

<svelte:head><title>onair inspector</title></svelte:head>

<main>
  <header class="toolbar">
    <div>
      <h1>onair inspector</h1>
      <p class:offline={!connected}>{connected ? "connected" : "reconnecting"} · {records.size} loaded · {pending.length} pending</p>
    </div>
    <div class="actions">
      <input aria-label="filter records" placeholder="filter records" bind:value={filter} />
      <button type="button" on:click={togglePause}>{paused ? "resume" : "pause"}</button>
      <button type="button" on:click={resetWidths}>reset widths</button>
    </div>
  </header>
  {#if lagged}<div class="notice">stream reset; snapshot reloaded</div>{/if}
  {#if droppedPending}<div class="notice">pending buffer capped at {PENDING_LIMIT}; oldest updates dropped</div>{/if}
  <section class="workspace">
    <div class="table-wrap" on:scroll={(event) => { const target = event.currentTarget as HTMLElement; viewportTop = target.scrollTop; viewportHeight = target.clientHeight; }}>
      <table>
        <colgroup>{#each columns as column}<col style={`width: ${columnWidth(column.key, widths)}px`} />{/each}</colgroup>
        <thead><tr>{#each columns as column}
          <th class:right={column.align === "right"}><button type="button" class="sort" on:click={() => sort(column.key)}>{column.label}{sortKey === column.key ? (sortDescending ? " ↓" : " ↑") : ""}</button><button type="button" class="resize" aria-label={`resize ${column.label}`} on:pointerdown={(event) => startResize(event, column.key)}></button></th>
        {/each}</tr></thead>
        <tbody style={`height: ${filtered.length * ROW_HEIGHT}px`}>
          <tr class="spacer" style={`transform: translateY(${visibleStart * ROW_HEIGHT}px)`}></tr>
          {#each visible as record (record.record_id)}
            <tr class:selected={record.record_id === selectedId} on:click={() => select(record)} on:keydown={(event) => event.key === "Enter" && select(record)} tabindex="0">
              <td>{formatTime(record.started_at_unix_ms)}</td><td class="right">{record.status || "-"}</td><td class="right">{valueFor(record, "total")}</td><td>{record.route}</td><td title={record.public_model}>{record.public_model || record.requested_model}</td><td title={record.backend}>{record.backend || "-"}</td><td>{formatOutcome(record)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <aside>
      <h2>record detail</h2>
      {#if selected}
        <dl><dt>record</dt><dd>{selected.record_id}</dd><dt>status</dt><dd>{selected.status || "-"}</dd><dt>route</dt><dd>{selected.route}</dd><dt>model</dt><dd>{selected.public_model || selected.requested_model}</dd><dt>backend</dt><dd>{selected.backend || "-"}</dd><dt>outcome</dt><dd>{formatOutcome(selected)}</dd></dl>
      {:else}<p>Select a record to inspect it.</p>{/if}
    </aside>
  </section>
</main>
