<script lang="ts">
  import { onMount } from "svelte";

  import {
    formatDate,
    formatMs,
    formatOutcome,
    formatTime,
    statusTone
  } from "./lib/record-format";
  import { columns, columnWidth, ROW_HEIGHT } from "./lib/store";
  import type { ColumnKey, InspectorRecord, VersionedRecord } from "./lib/types";

  type Widths = Partial<Record<ColumnKey, number>>;

  const widthKey = "onair.inspector.v4.widths";

  export let displayRecords: ReadonlyMap<string, VersionedRecord>;
  export let filter: string;
  export let selectedId: string;
  export let selectedDetached: boolean;
  export let streamReady: boolean;
  export let paused: boolean;
  export let onSelect: (recordId: string) => void;

  let sortKey: ColumnKey = "time";
  let sortDescending = true;
  let widths: Widths = loadWidths();
  let viewportTop = 0;
  let viewportHeight = 480;
  let viewportLeft = 0;
  let tableWrap: HTMLElement | undefined;
  let resizeStart:
    | {
        key: ColumnKey;
        x: number;
        width: number;
        pointerId: number;
        element: HTMLElement;
      }
    | undefined;
  let observedFilter = filter;
  let observedStreamReady = streamReady;

  $: filterNeedle = filter.trim().toLowerCase();
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
  $: visibleEnd = Math.min(
    filtered.length,
    Math.ceil((viewportTop + viewportHeight) / ROW_HEIGHT) + 8
  );
  $: visible = filtered.slice(visibleStart, visibleEnd);
  $: if (filter !== observedFilter || streamReady !== observedStreamReady) {
    observedFilter = filter;
    observedStreamReady = streamReady;
    resetTableViewport();
  }

  onMount(() => {
    widths = loadWidths();
    const resize = () => {
      viewportHeight = Math.max(260, tableWrap?.clientHeight ?? window.innerHeight - 168);
    };
    const tableResizeObserver = new ResizeObserver(resize);
    resize();
    if (tableWrap) tableResizeObserver.observe(tableWrap);
    window.addEventListener("resize", resize);
    return () => {
      tableResizeObserver.disconnect();
      window.removeEventListener("resize", resize);
      stopResize();
    };
  });

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

  function sort(key: ColumnKey) {
    if (sortKey === key) sortDescending = !sortDescending;
    else {
      sortKey = key;
      sortDescending = key === "time";
    }
  }

  export function resetWidths() {
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
      [key]: Math.max(
        column.minWidth,
        Math.min(column.maxWidth, columnWidth(key, widths) + delta)
      )
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

  function resetTableViewport() {
    viewportTop = 0;
    if (tableWrap) tableWrap.scrollTop = 0;
  }
</script>

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
              aria-sort={sortKey === column.key
                ? sortDescending
                  ? "descending"
                  : "ascending"
                : "none"}
            >
              <button
                type="button"
                class="sort"
                aria-label={`Sort by ${column.label}`}
                on:click={() => sort(column.key)}
              >
                <span class="sort-label">{column.label}</span>
                {#if sortKey === column.key}
                  <span class="sort-indicator" aria-hidden="true"
                    >{sortDescending ? "↓" : "↑"}</span
                  >
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
            class:detached={record.record_id === selectedId && selectedDetached}
            aria-selected={record.record_id === selectedId}
            aria-label={`Inspect request ${record.record_id}`}
            on:click={() => onSelect(record.record_id)}
            on:keydown={(event) => {
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                onSelect(record.record_id);
              }
            }}
            tabindex="0"
          >
            <td title={formatDate(record.started_at_unix_ms)}
              >{formatTime(record.started_at_unix_ms)}</td
            >
            <td class={`right status-cell ${statusTone(record.status)}`}>{record.status || "-"}</td>
            <td class="right numeric">{formatMs(record.timeline.total_us)}</td>
            <td>{record.route || "-"}</td>
            <td title={record.public_model || record.requested_model}
              >{record.public_model || record.requested_model || "-"}</td
            >
            <td title={record.backend}>{record.backend || "-"}</td>
            <td title={formatOutcome(record)}>{formatOutcome(record)}</td>
            <td class="right exposed-cell">
              {#if record.exposed_backend_error}<span
                  class="exposed-marker"
                  title="upstream error body exposed">yes</span
                >{:else}<span class="muted">—</span>{/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if filtered.length === 0}
      <div class="empty-state">
        {#if !streamReady}Waiting for inspector stream…{:else if filter.trim()}No records match the
          current filter.{:else}No retained records.{/if}
      </div>
    {/if}
  </div>
</div>
