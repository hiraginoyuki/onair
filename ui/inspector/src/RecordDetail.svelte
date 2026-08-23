<script lang="ts">
  import { onDestroy } from "svelte";

  import {
    formatBytes,
    formatDate,
    formatMs,
    formatOutcome,
    statusTone
  } from "./lib/record-format";
  import type {
    InspectorAttempt,
    InspectorRecord,
    SelectionState,
    Timeline
  } from "./lib/types";

  type TimelineField = { key: keyof Timeline; label: string };
  type PhaseField = { key: keyof InspectorAttempt; label: string };

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

  export let selection: SelectionState;
  export let onNotice: (message: string, persistent?: boolean) => void;

  let expandedAttempts = new Set<string>();
  let expansionSelectionKey = "";
  const downloadUrls = new Set<string>();

  $: selected = selection.kind === "ready" ? selection.item.record : undefined;
  $: selectedIsDetached = selection.kind === "ready" && selection.detached;
  $: detailViewState =
    selection.kind === "ready" ? (selection.detached ? "detached-ready" : "ready") : selection.kind;
  $: selectedAttempts = selected ? attemptRecords(selected) : [];
  $: waterfallTotal = selected
    ? Math.max(
        selected.timeline.total_us,
        ...selectedAttempts.map((attempt) => attempt.ended_us || 0)
      )
    : 0;
  $: timelineEntries = selected
    ? timelineFields
        .map((field) => ({ ...field, value: timelineValue(selected.timeline, field.key) }))
        .filter((entry): entry is TimelineField & { value: number } => entry.value !== undefined)
    : [];
  $: {
    const nextExpansionSelectionKey =
      selection.kind === "ready"
        ? `${selection.item.record_id}:${selection.item.revision}`
        : selection.kind === "none"
          ? "none"
          : `${selection.kind}:${selection.recordId}`;
    if (nextExpansionSelectionKey !== expansionSelectionKey) {
      expansionSelectionKey = nextExpansionSelectionKey;
      expandedAttempts = new Set();
    }
  }
  onDestroy(() => {
    for (const url of downloadUrls) URL.revokeObjectURL(url);
    downloadUrls.clear();
  });

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

  function phaseValue(
    attempt: InspectorAttempt,
    key: keyof InspectorAttempt
  ): number | undefined {
    const value = attempt[key];
    return typeof value === "number" ? value : undefined;
  }

  function attemptKey(attempt: InspectorAttempt, index: number): string {
    return `${attempt.attempt || index + 1}:${attempt.backend}`;
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
      [
        "upstream status",
        attempt.upstream_status === undefined ? "-" : String(attempt.upstream_status)
      ],
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
      onNotice("record JSON copied");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "copy failed", true);
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
    onNotice("record JSON downloaded");
  }
</script>

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
        <h2 class="detail-record-id" title={selection.item.record_id}
          >{selection.item.record_id}</h2
        >
        <p class="detail-revision">
          revision <span class="numeric">{selection.item.revision}</span>
        </p>
      {/if}
    </div>
    {#if selectedIsDetached}
      <span
        class="detached-badge"
        aria-label="Detached: pinned revision is outside the current live table window"
        title="Pinned revision is outside the current live table window">detached</span
      >
    {/if}
  </div>

  {#if selection.kind === "loading"}
    <p class="empty-detail">
      Loading record <span class="detail-target-id">{selection.recordId}</span>…
    </p>
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
        <dt>path</dt><dd class="break"
          >{selected.query ? `${selected.path}?${selected.query}` : selected.path}</dd
        >
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
        <dt>response body</dt><dd class="numeric"
          >{formatBytes(selected.response_body_bytes)}</dd
        >
        <dt>input tokens</dt><dd class="numeric">{selected.input_tokens.toLocaleString()}</dd>
        <dt>cached input</dt><dd class="numeric"
          >{selected.cached_input_tokens.toLocaleString()}</dd
        >
        <dt>output tokens</dt><dd class="numeric">{selected.output_tokens.toLocaleString()}</dd>
        <dt>debug capture</dt><dd class="break">{selected.debug_capture_id || "—"}</dd>
      </dl>
    </section>

    <section class="detail-card">
      <div class="card-heading-row">
        <h3>timeline</h3><span class="numeric">{formatMs(selected.timeline.total_us)}</span>
      </div>
      <div class="timeline">
        {#each timelineEntries as entry}
          <div class="timeline-row">
            <span class="timeline-label">{entry.label}</span>
            <span class="timeline-value numeric">{formatMs(entry.value)}</span>
            <span class="timeline-track"
              ><span
                class="timeline-fill"
                style={`width: ${timelinePercent(entry.value, selected.timeline.total_us)}%`}
              ></span></span
            >
          </div>
        {/each}
      </div>
    </section>

    {#if selectedAttempts.length}
      <section class="detail-card">
        <div class="card-heading-row">
          <h3>attempt waterfall</h3>
          <span class="muted"
            >{selectedAttempts.length} attempt{selectedAttempts.length === 1 ? "" : "s"}</span
          >
        </div>
        <div class="waterfall-actions">
          <button type="button" on:click={() => toggleAllAttempts(true)}>expand all</button>
          <button type="button" on:click={() => toggleAllAttempts(false)}>collapse all</button>
        </div>
        <div class="waterfall-scale">
          <span>request</span><span class="numeric"
            >0 ms · {formatMs(waterfallTotal / 2)} · {formatMs(waterfallTotal)}</span
          >
        </div>
        <div class="waterfall">
          {#each selectedAttempts as attempt, index}
            <div
              class:expanded={expandedAttempts.has(attemptKey(attempt, index))}
              class="attempt"
            >
              <div class="attempt-heading">
                <div>
                  <strong>#{attempt.attempt} {attempt.backend || "unknown"}</strong>
                  <span class="attempt-summary"
                    >{attempt.outcome} · status {attempt.status} · {formatMs(
                      attempt.elapsed_us
                    )}</span
                  >
                </div>
                <button
                  type="button"
                  class="compact-action"
                  aria-expanded={expandedAttempts.has(attemptKey(attempt, index))}
                  aria-controls={`attempt-details-${index}`}
                  on:click={() => toggleAttempt(attempt, index)}
                >
                  {expandedAttempts.has(attemptKey(attempt, index)) ? "hide details" : "details"}
                </button>
              </div>
              <div class="attempt-track">
                <span
                  class={`attempt-span ${statusTone(attempt.status)}`}
                  style={`left: ${timelinePercent(attempt.started_us, waterfallTotal)}%; width: ${Math.max(
                    0.8,
                    timelinePercent(
                      Math.max(0, attempt.ended_us - attempt.started_us),
                      waterfallTotal
                    )
                  )}%`}
                  title={`${attempt.backend_target} · ${formatMs(attempt.elapsed_us)}`}
                ></span>
                {#each attemptPhases(attempt) as phase}
                  <span
                    class="attempt-tick"
                    style={`left: ${timelinePercent(phase.value, waterfallTotal)}%`}
                    title={`${phase.label}: ${formatMs(phase.value)}`}
                  ></span>
                {/each}
              </div>
              {#if expandedAttempts.has(attemptKey(attempt, index))}
                <div class="attempt-details" id={`attempt-details-${index}`}>
                  <dl class="kv">
                    {#each attemptDetails(attempt) as pair}
                      <dt>{pair[0]}</dt><dd class="break">{pair[1]}</dd>
                    {/each}
                  </dl>
                  {#if attemptPhases(attempt).length}
                    <div class="phase-list">
                      {#each attemptPhases(attempt) as phase}<span
                          >{phase.label} {formatMs(phase.value)}</span
                        >{/each}
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
            <dd class="break"
              >{attempt.outcome} · status {attempt.status} · {formatMs(attempt.elapsed_us)}{attempt.error_kind
                ? ` · ${attempt.error_kind}`
                : ""}</dd
            >
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
