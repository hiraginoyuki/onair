    "use strict";

    const base = "/_onair/inspector";
    const operatorBase = "/_onair/operator";
    const snapshotLimit = 1000;
    const tableHead = document.getElementById("tableHead");
    const rows = document.getElementById("rows");
    const empty = document.getElementById("empty");
    const overview = document.getElementById("overview");
    const detail = document.getElementById("detail");
    const filter = document.getElementById("filter");
    const quickFiltersElement = document.getElementById("quickFilters");
    const columnOptions = document.getElementById("columnOptions");
    const presetOptions = document.getElementById("presetOptions");
    const pauseButton = document.getElementById("pause");
    const statusText = document.getElementById("status");
    const statusDot = document.getElementById("dot");
    const countText = document.getElementById("count");
    const columnStorageKey = "onair.inspector.tableColumns.v2";
    const sortStorageKey = "onair.inspector.tableSort.v1";
    const presetStorageKey = "onair.inspector.tablePresets.v1";
    const maxPresetCount = 24;
    const slowRequestThresholdUs = 5_000_000;
    const records = new Map();
    const pendingRecords = new Map();
    const expandedAttemptDetails = new Map();
    let operatorRuntime = null;
    let operatorConfig = null;
    let operatorModels = null;
    let operatorHealth = null;
    let operatorReloadPending = false;
    let selectedId = null;
    let source = null;
    let livePaused = false;
    let viewPresets = [];

    const timelineFields = [
      ["proxy entry", "proxy_entry_us"],
      ["auth done", "auth_done_us"],
      ["request inspected", "request_inspected_us"],
      ["route selected", "route_selected_us"],
      ["request rewritten", "request_rewritten_us"],
      ["debug capture done", "debug_capture_done_us"],
      ["backend forward start", "backend_forward_start_us"],
      ["backend headers received", "backend_headers_received_us"],
      ["backend body first chunk", "backend_body_first_chunk_us"],
      ["backend body complete", "backend_body_complete_us"],
      ["response rewritten", "response_rewritten_us"],
      ["client response ready", "client_response_ready_us"],
      ["stream complete", "stream_complete_us"],
    ];

    const attemptPhaseFields = [
      ["rewrite", "request_rewritten_us"],
      ["capture", "debug_capture_done_us"],
      ["send", "backend_forward_start_us"],
      ["headers", "backend_headers_received_us"],
      ["first byte", "backend_body_first_chunk_us"],
      ["body done", "backend_body_complete_us"],
      ["stream done", "stream_complete_us"],
    ];

    const tableColumns = [
      {
        key: "time",
        label: "time",
        headerClass: "c-time",
        defaultVisible: true,
        value: record => timeOf(record.started_at_unix_ms),
        sortValue: record => Number(record.started_at_unix_ms || 0),
        title: record => new Date(record.started_at_unix_ms).toISOString(),
        help: "Request start time.",
      },
      {
        key: "status",
        label: "status",
        headerClass: "c-status",
        defaultVisible: true,
        value: record => record.status,
        className: record => `status-code ${statusClass(Number(record.status), record)}${record.exposed_backend_error ? " exposed" : ""}`,
        help: "Client-facing response status. A trailing 'exposed' marker means the upstream's non-2xx body was forwarded to the client because the route had `expose_backend_errors = true`.",
      },
      {
        key: "total_ms",
        label: "total ms",
        headerClass: "c-ms",
        defaultVisible: true,
        value: record => ms(record.timeline && record.timeline.total_us),
        sortValue: record => Number((record.timeline || {}).total_us || 0),
        className: "c-ms",
        title: record => `${ms(record.timeline && record.timeline.total_us)} ms`,
        help: "Total request time measured by onair.",
      },
      {
        key: "route",
        label: "route",
        headerClass: "c-route",
        defaultVisible: true,
        value: record => record.route,
        help: "OpenAI-compatible route family.",
      },
      {
        key: "identity",
        label: "identity",
        headerClass: "c-identity",
        defaultVisible: true,
        value: record => record.identity,
        help: "Authenticated onair client identity.",
      },
      {
        key: "model",
        label: "model",
        headerClass: "c-model",
        defaultVisible: true,
        value: record => record.public_model || record.requested_model,
        help: "Public model visible to the client.",
      },
      {
        key: "backend",
        label: "backend",
        headerClass: "c-backend",
        defaultVisible: true,
        value: record => record.backend,
        help: "Selected backend ID.",
      },
      {
        key: "client",
        label: "client",
        headerClass: "c-client",
        defaultVisible: true,
        value: record => record.effective_client_addr,
        help: "Effective client address after trusted-proxy handling.",
      },
      {
        key: "user_agent",
        label: "user-agent",
        headerClass: "c-user-agent",
        defaultVisible: true,
        value: record => record.user_agent,
        help: "Client User-Agent header.",
      },
      {
        key: "stream",
        label: "stream",
        headerClass: "c-stream",
        defaultVisible: true,
        value: record => record.stream ? "yes" : "no",
        help: "Whether the request used streaming.",
      },
      {
        key: "outcome",
        label: "outcome",
        headerClass: "c-outcome",
        defaultVisible: true,
        value: outcomeText,
        className: record => outcomeText(record) !== "completed" ? "outcome-bad" : "",
        help: "Final proxy outcome.",
      },
      {
        key: "record_id",
        label: "record id",
        headerClass: "c-wider",
        value: record => record.record_id,
        help: "Inspector record ID used for deep links.",
      },
      {
        key: "client_request_id",
        label: "x-request-id",
        headerClass: "c-wider",
        value: record => record.client_request_id,
        help: "Client-supplied X-Request-ID when present.",
      },
      {
        key: "path",
        label: "path",
        headerClass: "c-wider",
        value: record => record.query ? `${record.path}?${record.query}` : record.path,
        help: "Request path and query string.",
      },
      {
        key: "requested_model",
        label: "requested model",
        headerClass: "c-wide",
        value: record => record.requested_model,
        help: "Model name received from the client.",
      },
      {
        key: "backend_model",
        label: "backend model",
        headerClass: "c-wide",
        value: record => record.backend_model,
        help: "Private model name sent to the backend.",
      },
      {
        key: "backend_target",
        label: "backend target",
        headerClass: "c-wider",
        value: record => record.backend_target,
        help: "Configured backend host and port.",
      },
      {
        key: "backend_remote_addr",
        label: "backend remote",
        headerClass: "c-wide",
        value: record => record.backend_remote_addr,
        help: "Backend socket address observed by reqwest when available.",
      },
      {
        key: "peer_addr",
        label: "peer addr",
        headerClass: "c-wide",
        value: record => record.peer_addr,
        help: "Immediate socket peer address.",
      },
      {
        key: "trusted_proxy_addr",
        label: "trusted proxy",
        headerClass: "c-wide",
        value: record => record.trusted_proxy_addr,
        help: "Trusted proxy address used for forwarded client metadata.",
      },
      {
        key: "forwarded_for",
        label: "forwarded for",
        headerClass: "c-wide",
        value: record => record.forwarded_for,
        help: "Forwarded client address trusted by onair.",
      },
      {
        key: "error_kind",
        label: "error kind",
        headerClass: "c-wide",
        value: record => record.error_kind,
        help: "Transport/body error kind for failures.",
      },
      {
        key: "attempts",
        label: "attempts",
        headerClass: "c-status",
        value: record => attemptRecords(record).length || "",
        sortValue: record => attemptRecords(record).length,
        help: "Number of backend attempts recorded for the request.",
      },
    ];
    const defaultColumnKeys = tableColumns
      .filter(column => column.defaultVisible)
      .map(column => column.key);
    const validColumnKeys = new Set(tableColumns.map(column => column.key));
    const visibleColumns = new Set(loadVisibleColumns());
    let sortState = loadSortState();
    const quickFilters = new Set();

    const quickFilterDefinitions = [
      {
        key: "in-flight",
        label: "in-flight",
        title: "Show requests that are currently being processed (live updates).",
        matches: record => isInFlight(record),
      },
      {
        key: "error",
        label: "errors",
        title: "Show requests with status >= 400 or a non-completed outcome.",
        matches: record => Number(record.status) >= 400 || outcomeText(record) !== "completed",
      },
      {
        key: "fallback",
        label: "fallback",
        title: "Show requests with abandoned pre-response attempts or multiple backend attempts.",
        matches: record => (record.retried_attempts || []).length > 0 || attemptRecords(record).length > 1,
      },
      {
        key: "slow",
        label: "slow ≥5s",
        title: "Show requests whose total onair-observed time is at least 5 seconds.",
        matches: record => Number((record.timeline || {}).total_us || 0) >= slowRequestThresholdUs,
      },
      {
        key: "exposed",
        label: "exposed",
        title: "Show non-2xx responses whose upstream body was forwarded to the client via expose_backend_errors.",
        matches: record => record.exposed_backend_error === true,
      },
    ];
    const quickFilterKeys = new Set(quickFilterDefinitions.map(definition => definition.key));

    function setStatus(text, className) {
      statusText.textContent = text;
      statusDot.className = `dot ${className || ""}`;
    }

    function outcomeText(record) {
      if (!record || !record.outcome) return "unknown";
      if (typeof record.outcome === "string") {
        if (record.outcome === "in_flight") return "in-flight";
        return record.outcome;
      }
      if (record.outcome.kind === "preflight") return `preflight:${record.outcome.stage}`;
      if (record.outcome.kind === "in_flight") return "in-flight";
      if (record.outcome.kind === "interrupted") return "interrupted";
      return record.outcome.kind || "unknown";
    }

    function isInFlight(record) {
      if (!record || !record.outcome) return false;
      if (typeof record.outcome === "string") return record.outcome === "in_flight";
      return record.outcome.kind === "in_flight";
    }

    function isInterrupted(record) {
      if (!record || !record.outcome) return false;
      if (typeof record.outcome === "string") return record.outcome === "interrupted";
      return record.outcome.kind === "interrupted";
    }

    function statusClass(status, record) {
      if (record && isInFlight(record)) return "pending";
      if (record && isInterrupted(record)) return "interrupted";
      const code = Number(status);
      if (code >= 500) return "bad";
      if (code >= 400) return "warn";
      if (code === 0) return "pending";
      return "ok";
    }

    function ms(us) {
      if (us === null || us === undefined) return "";
      return (Number(us) / 1000).toFixed(1);
    }

    function timeOf(msSinceEpoch) {
      return new Date(msSinceEpoch).toLocaleTimeString(undefined, { hour12: false });
    }

    function durationText(milliseconds) {
      const seconds = Math.floor(Number(milliseconds || 0) / 1000);
      const minutes = Math.floor(seconds / 60);
      const hours = Math.floor(minutes / 60);
      const days = Math.floor(hours / 24);
      if (days > 0) return `${days}d ${hours % 24}h ${minutes % 60}m`;
      if (hours > 0) return `${hours}h ${minutes % 60}m ${seconds % 60}s`;
      if (minutes > 0) return `${minutes}m ${seconds % 60}s`;
      return `${seconds}s`;
    }

    function compactList(values, limit = 6) {
      const filtered = values.filter(Boolean);
      if (!filtered.length) return "none";
      const shown = filtered.slice(0, limit).join(", ");
      return filtered.length > limit ? `${shown}, +${filtered.length - limit} more` : shown;
    }

    function healthSummary(backends) {
      const counts = new Map();
      for (const backend of backends) {
        counts.set(backend.status, (counts.get(backend.status) || 0) + 1);
      }
      return ["healthy", "degraded", "unhealthy", "unknown"]
        .filter(status => counts.has(status))
        .map(status => `${status}:${counts.get(status)}`)
        .join(", ") || "none";
    }

    function backendHealthText(backend) {
      const parts = [
        backend.status,
        `traffic ${backend.traffic_successes}/${backend.traffic_failures}`,
        `probe ${backend.probe_successes}/${backend.probe_failures}`,
      ];
      if (backend.consecutive_failures > 0) parts.push(`consecutive ${backend.consecutive_failures}`);
      if (backend.last_status) parts.push(`last ${backend.last_status}`);
      if (backend.last_latency_ms !== null && backend.last_latency_ms !== undefined) parts.push(`${backend.last_latency_ms} ms`);
      if (backend.last_source) parts.push(backend.last_source);
      if (backend.last_error_kind) parts.push(backend.last_error_kind);
      return parts.join(" · ");
    }

    function attemptRecords(record) {
      const attempts = record.backend_attempts || [];
      return attempts.length ? attempts : (record.retried_attempts || []);
    }

    function loadVisibleColumns() {
      try {
        const raw = localStorage.getItem(columnStorageKey);
        const parsed = raw && JSON.parse(raw);
        if (Array.isArray(parsed)) {
          const selected = parsed.filter(key => validColumnKeys.has(key));
          if (selected.length) return selected;
        }
      } catch {
        // Ignore localStorage or JSON failures and use defaults.
      }
      return defaultColumnKeys;
    }

    function saveVisibleColumns() {
      try {
        localStorage.setItem(columnStorageKey, JSON.stringify([...visibleColumns]));
      } catch {
        // Column preferences are optional.
      }
    }

    function activeColumns() {
      return tableColumns.filter(column => visibleColumns.has(column.key));
    }

    function loadSortState() {
      try {
        const parsed = JSON.parse(localStorage.getItem(sortStorageKey));
        if (
          parsed
          && validColumnKeys.has(parsed.key)
          && (parsed.direction === "asc" || parsed.direction === "desc")
        ) {
          return parsed;
        }
      } catch {
        // Sorting preferences are optional.
      }
      return { key: "time", direction: "desc" };
    }

    function normalizePresetName(name) {
      return String(name || "").trim().replace(/\s+/g, " ").slice(0, 64);
    }

    function normalizePresetColumns(columns) {
      if (!Array.isArray(columns)) return defaultColumnKeys;
      const ordered = [];
      const selected = new Set(columns.filter(key => validColumnKeys.has(key)));
      for (const column of tableColumns) {
        if (selected.has(column.key)) ordered.push(column.key);
      }
      return ordered.length ? ordered : defaultColumnKeys;
    }

    function normalizePresetQuickFilters(filters) {
      if (!Array.isArray(filters)) return [];
      const selected = new Set(filters.filter(key => quickFilterKeys.has(key)));
      return quickFilterDefinitions.filter(definition => selected.has(definition.key)).map(definition => definition.key);
    }

    function normalizePresetSort(sort) {
      if (
        sort
        && validColumnKeys.has(sort.key)
        && (sort.direction === "asc" || sort.direction === "desc")
      ) {
        return { key: sort.key, direction: sort.direction };
      }
      return { key: "time", direction: "desc" };
    }

    function normalizePreset(preset) {
      const name = normalizePresetName(preset && preset.name);
      if (!name) return null;
      return {
        name,
        columns: normalizePresetColumns(preset.columns),
        quickFilters: normalizePresetQuickFilters(preset.quickFilters),
        sort: normalizePresetSort(preset.sort),
      };
    }

    function loadViewPresets() {
      try {
        const parsed = JSON.parse(localStorage.getItem(presetStorageKey));
        if (Array.isArray(parsed)) {
          const presets = [];
          const names = new Set();
          for (const value of parsed) {
            const preset = normalizePreset(value);
            if (!preset) continue;
            const key = preset.name.toLowerCase();
            if (names.has(key)) continue;
            names.add(key);
            presets.push(preset);
            if (presets.length >= maxPresetCount) break;
          }
          return presets;
        }
      } catch {
        // Presets are optional.
      }
      return [];
    }

    function saveViewPresets() {
      try {
        localStorage.setItem(presetStorageKey, JSON.stringify(viewPresets));
      } catch {
        // Presets are optional.
      }
    }

    function currentPresetState(name) {
      return normalizePreset({
        name,
        columns: activeColumns().map(column => column.key),
        quickFilters: [...quickFilters],
        sort: sortState,
      });
    }

    function presetSummary(preset) {
      const parts = [
        `columns ${preset.columns.length}`,
        `sort ${preset.sort.key} ${preset.sort.direction}`,
        preset.quickFilters.length ? `quick ${preset.quickFilters.join(", ")}` : "quick none",
      ];
      return parts.join(" · ");
    }

    function closeToolbarMenus(except = null) {
      for (const menu of document.querySelectorAll(".toolbar-menu")) {
        if (menu !== except) menu.open = false;
      }
    }

    function applyPreset(preset) {
      visibleColumns.clear();
      for (const columnKey of preset.columns) visibleColumns.add(columnKey);
      sortState = { ...preset.sort };
      quickFilters.clear();
      for (const key of preset.quickFilters) quickFilters.add(key);
      saveVisibleColumns();
      saveSortState();
      renderColumnOptions();
      renderQuickFilters();
      renderTableHeader();
      renderRows();
      renderViewPresets();
      setStatus(`applied view ${preset.name}`, "live");
      closeToolbarMenus();
    }

    function savePreset(name) {
      const preset = currentPresetState(name);
      if (!preset) {
        setStatus("preset name required", "dead");
        return;
      }
      const index = viewPresets.findIndex(existing => existing.name.toLowerCase() === preset.name.toLowerCase());
      if (index >= 0) {
        viewPresets[index] = preset;
      } else if (viewPresets.length < maxPresetCount) {
        viewPresets.push(preset);
      } else {
        setStatus(`max ${maxPresetCount} presets reached`, "dead");
        return;
      }
      saveViewPresets();
      renderViewPresets();
      setStatus(`saved view ${preset.name}`, "live");
    }

    function deletePreset(name) {
      const index = viewPresets.findIndex(existing => existing.name === name);
      if (index < 0) return;
      viewPresets.splice(index, 1);
      saveViewPresets();
      renderViewPresets();
      setStatus(`deleted view ${name}`, "live");
    }

    function resetTableView() {
      visibleColumns.clear();
      for (const columnKey of defaultColumnKeys) visibleColumns.add(columnKey);
      sortState = { key: "time", direction: "desc" };
      quickFilters.clear();
      saveVisibleColumns();
      saveSortState();
      renderColumnOptions();
      renderQuickFilters();
      renderTableHeader();
      renderRows();
      renderViewPresets();
      setStatus("restored default table view", "live");
      closeToolbarMenus();
    }

    function saveSortState() {
      try {
        localStorage.setItem(sortStorageKey, JSON.stringify(sortState));
      } catch {
        // Sorting preferences are optional.
      }
    }

    function sortColumn() {
      return tableColumns.find(column => column.key === sortState.key) || tableColumns[0];
    }

    function sortValue(column, record) {
      const value = column.sortValue ? column.sortValue(record) : column.value(record);
      return value === null || value === undefined ? "" : value;
    }

    function compareValues(left, right) {
      if (typeof left === "number" && typeof right === "number") {
        return left - right;
      }
      return String(left).localeCompare(String(right), undefined, {
        numeric: true,
        sensitivity: "base",
      });
    }

    function sortRecords(list) {
      const column = sortColumn();
      const direction = sortState.direction === "desc" ? -1 : 1;
      return list.sort((left, right) => {
        const leftValue = sortValue(column, left);
        const rightValue = sortValue(column, right);
        const leftEmpty = leftValue === "";
        const rightEmpty = rightValue === "";
        if (leftEmpty || rightEmpty) {
          if (leftEmpty !== rightEmpty) return leftEmpty ? 1 : -1;
        }
        const compared = compareValues(leftValue, rightValue);
        if (compared !== 0) return compared * direction;
        if (left.started_at_unix_ms !== right.started_at_unix_ms) {
          return right.started_at_unix_ms - left.started_at_unix_ms;
        }
        return String(right.record_id).localeCompare(String(left.record_id));
      });
    }

    function toggleSort(columnKey) {
      if (sortState.key === columnKey) {
        sortState = {
          key: columnKey,
          direction: sortState.direction === "asc" ? "desc" : "asc",
        };
      } else {
        sortState = { key: columnKey, direction: "asc" };
      }
      saveSortState();
      renderTableHeader();
      renderRows();
    }

    function renderQuickFilters() {
      quickFiltersElement.replaceChildren();
      const label = document.createElement("span");
      label.className = "quick-label";
      label.textContent = "quick";
      quickFiltersElement.appendChild(label);

      for (const quickFilter of quickFilterDefinitions) {
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = quickFilter.label;
        button.title = quickFilter.title;
        button.className = quickFilters.has(quickFilter.key) ? "active" : "";
        button.addEventListener("click", () => {
          if (quickFilters.has(quickFilter.key)) {
            quickFilters.delete(quickFilter.key);
          } else {
            quickFilters.add(quickFilter.key);
          }
          renderQuickFilters();
          renderRows();
        });
        quickFiltersElement.appendChild(button);
      }
    }

    function matchesQuickFilters(record) {
      for (const quickFilter of quickFilterDefinitions) {
        if (quickFilters.has(quickFilter.key) && !quickFilter.matches(record)) {
          return false;
        }
      }
      return true;
    }

    function updatePauseButton() {
      const pending = pendingRecords.size;
      pauseButton.textContent = livePaused
        ? (pending ? `resume (${pending})` : "resume")
        : "pause";
      pauseButton.className = livePaused ? "active" : "";
      pauseButton.title = livePaused
        ? "Resume live updates and apply buffered request records."
        : "Pause applying live request records to the table.";
      if (livePaused) {
        setStatus(pending ? `paused (+${pending})` : "paused", "");
      }
    }

    function applyRecord(record) {
      records.set(record.record_id, record);
    }

    function renderAfterRecordChange() {
      if (operatorRuntime) operatorRuntime.inspector_retained_requests = records.size;
      renderRows();
      renderOverview();
      scheduleOperatorReload();
      if (selectedId && records.has(selectedId)) renderDetail(records.get(selectedId));
    }

    function renderColumnOptions() {
      columnOptions.replaceChildren();
      for (const column of tableColumns) {
        const label = document.createElement("label");
        label.title = column.help || column.label;
        const checkbox = document.createElement("input");
        checkbox.type = "checkbox";
        checkbox.checked = visibleColumns.has(column.key);
        checkbox.addEventListener("change", () => {
          if (checkbox.checked) {
            visibleColumns.add(column.key);
          } else if (visibleColumns.size > 1) {
            visibleColumns.delete(column.key);
          } else {
            checkbox.checked = true;
            return;
          }
          saveVisibleColumns();
          renderTableHeader();
          renderRows();
        });
        const text = document.createElement("span");
        text.textContent = column.label;
        label.append(checkbox, text);
        columnOptions.appendChild(label);
      }
    }

    function renderViewPresets() {
      presetOptions.replaceChildren();

      const form = document.createElement("form");
      form.className = "preset-form";
      const nameInput = document.createElement("input");
      nameInput.type = "text";
      nameInput.autocomplete = "off";
      nameInput.spellcheck = "false";
      nameInput.maxLength = 64;
      nameInput.placeholder = "preset name";
      nameInput.title = "Save the current columns, sort, and quick filters under a local name.";
      const saveButton = document.createElement("button");
      saveButton.type = "submit";
      saveButton.textContent = "save view";
      saveButton.title = "Save the current table view as a local preset.";
      const resetButton = document.createElement("button");
      resetButton.type = "button";
      resetButton.textContent = "reset";
      resetButton.title = "Restore the default columns, sort, and quick filters.";
      resetButton.addEventListener("click", resetTableView);
      form.append(nameInput, saveButton, resetButton);
      form.addEventListener("submit", event => {
        event.preventDefault();
        const name = nameInput.value.trim();
        if (!name) {
          setStatus("preset name required", "dead");
          nameInput.focus();
          return;
        }
        savePreset(name);
        nameInput.value = "";
        nameInput.focus();
      });

      presetOptions.appendChild(form);

      const list = document.createElement("div");
      list.className = "preset-list";
      if (!viewPresets.length) {
        const emptyState = document.createElement("div");
        emptyState.className = "toolbar-help";
        emptyState.style.textAlign = "left";
        emptyState.textContent = "No saved views yet. Save the current table layout here, or reset to the defaults.";
        list.appendChild(emptyState);
      } else {
        for (const preset of viewPresets) {
          const row = document.createElement("div");
          row.className = "preset-row";

          const meta = document.createElement("div");
          meta.className = "preset-meta";
          const name = document.createElement("div");
          name.className = "preset-name";
          name.textContent = preset.name;
          const summary = document.createElement("div");
          summary.className = "preset-summary";
          summary.textContent = presetSummary(preset);
          meta.append(name, summary);

          const actions = document.createElement("div");
          actions.className = "preset-actions";

          const applyButton = document.createElement("button");
          applyButton.type = "button";
          applyButton.textContent = "apply";
          applyButton.title = "Apply this saved table view.";
          applyButton.addEventListener("click", () => applyPreset(preset));

          const deleteButton = document.createElement("button");
          deleteButton.type = "button";
          deleteButton.textContent = "delete";
          deleteButton.title = "Delete this saved table view.";
          deleteButton.addEventListener("click", () => deletePreset(preset.name));

          actions.append(applyButton, deleteButton);
          row.append(meta, actions);
          list.appendChild(row);
        }
      }
      presetOptions.appendChild(list);
    }

    function renderTableHeader() {
      const tr = document.createElement("tr");
      for (const column of activeColumns()) {
        const th = document.createElement("th");
        const classes = [column.headerClass, sortState.key === column.key ? "sorted" : null]
          .filter(Boolean)
          .join(" ");
        if (classes) th.className = classes;
        const button = document.createElement("button");
        button.type = "button";
        button.className = "sort-button";
        button.textContent = `${column.label}${sortState.key === column.key ? (sortState.direction === "asc" ? " ↑" : " ↓") : ""}`;
        const headerText = `${column.label}: ${column.help || "Click to sort."}`;
        th.dataset.full = headerText;
        button.title = `${column.help || column.label} Click to sort.`;
        button.addEventListener("click", () => toggleSort(column.key));
        th.appendChild(button);
        tr.appendChild(th);
      }
      tableHead.replaceChildren(tr);
    }

    function searchable(record) {
      return [
        record.record_id,
        record.client_request_id,
        record.route,
        record.identity,
        record.requested_model,
        record.public_model,
        record.backend_model,
        record.backend,
        record.backend_target,
        record.backend_remote_addr,
        record.peer_addr,
        record.effective_client_addr,
        record.trusted_proxy_addr,
        record.forwarded_for,
        record.user_agent,
        record.path,
        record.query,
        outcomeText(record),
        record.exposed_backend_error ? "exposed" : "",
        ...attemptRecords(record).flatMap(attempt => [
          attempt.backend,
          attempt.backend_target,
          attempt.backend_remote_addr,
          attempt.outcome,
          attempt.error_kind,
          attempt.upstream_status,
          attempt.debug_capture_id,
        ]),
      ].filter(Boolean).join(" ").toLowerCase();
    }

    function upsert(record) {
      if (livePaused) {
        pendingRecords.set(record.record_id, record);
        updatePauseButton();
        return;
      }
      applyRecord(record);
      renderAfterRecordChange();
    }

    function hashRecordId() {
      const hash = window.location.hash.slice(1);
      if (!hash) return null;
      try {
        return decodeURIComponent(hash);
      } catch {
        return hash;
      }
    }

    function recordLink(record) {
      return `${window.location.pathname}#${encodeURIComponent(record.record_id)}`;
    }

    function selectRecord(record, updateHash) {
      selectedId = record.record_id;
      if (updateHash) history.replaceState(null, "", `#${encodeURIComponent(record.record_id)}`);
      renderRows();
      renderDetail(record);
    }

    async function selectRecordId(recordId, updateHash) {
      if (!recordId) return;
      selectedId = recordId;
      if (records.has(recordId)) {
        selectRecord(records.get(recordId), updateHash);
        return;
      }

      renderRows();
      const response = await fetch(`${base}/requests/${encodeURIComponent(recordId)}`, { cache: "no-store" });
      if (!response.ok) {
        showDetailMessage(`Request ${recordId} is not retained.`);
        return;
      }
      const record = await response.json();
      records.set(record.record_id, record);
      selectRecord(record, updateHash);
    }

    function recordList() {
      const tokens = filter.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
      const list = Array.from(records.values());
      const filtered = tokens.length
        ? list.filter(record => {
          const haystack = searchable(record);
          return tokens.every(token => haystack.includes(token));
        })
        : list;
      return sortRecords(filtered.filter(matchesQuickFilters));
    }

    function renderRows() {
      const list = recordList();
      const columns = activeColumns();
      rows.replaceChildren();
      const inFlightCount = Array.from(records.values()).filter(isInFlight).length;
      const countSuffix = inFlightCount > 0 ? `, ${inFlightCount} in-flight` : "";
      const total = records.size;
      countText.textContent = list.length === total
        ? `${total} request${total === 1 ? "" : "s"}${countSuffix}`
        : `${list.length}/${total} requests${countSuffix}`;
      empty.style.display = list.length ? "none" : "block";

      for (const record of list.slice(0, 1000)) {
        const tr = document.createElement("tr");
        if (record.record_id === selectedId) tr.classList.add("selected");
        tr.addEventListener("click", () => {
          selectRecord(record, true);
        });
        for (const column of columns) {
          const value = column.value(record);
          const text = value == null || value === "" ? "" : String(value);
          const td = document.createElement("td");
          td.textContent = text;
          if (text) td.dataset.full = text;
          td.title = column.title ? String(column.title(record, value) || text) : text;
          const className = typeof column.className === "function"
            ? column.className(record, value)
            : column.className;
          if (className) td.className = className;
          tr.appendChild(td);
        }
        rows.appendChild(tr);
      }
    }

    function showDetailMessage(message) {
      const div = document.createElement("div");
      div.className = "empty";
      div.textContent = message;
      detail.replaceChildren(div);
    }

    function renderOverview() {
      if (!operatorRuntime || !operatorConfig || !operatorModels || !operatorHealth) {
        overview.replaceChildren(kvCard("operator overview", [["state", "loading"]]));
        return;
      }

      const modelRouteCount = operatorModels.public_models
        .reduce((sum, model) => sum + model.routes.length, 0);
      const backendHealthRows = operatorHealth.backends.map(backend => [
        backend.backend,
        backendHealthText(backend),
      ]);
      overview.replaceChildren(
        kvCard("operator overview", [
          ["uptime", durationText(operatorRuntime.uptime_ms)],
          ["retained requests", operatorRuntime.inspector_retained_requests],
          ["clients", operatorRuntime.clients],
          ["backends", operatorRuntime.backends],
          ["backend health", healthSummary(operatorHealth.backends)],
          ["public models", operatorRuntime.public_models],
          ["telemetry", `${operatorRuntime.telemetry.exporter} (${operatorRuntime.telemetry.service_name})`],
        ]),
        kvCard("backend health", backendHealthRows.length ? backendHealthRows : [["state", "none"]]),
        kvCard("active config", [
          ["bind", operatorConfig.server.bind],
          ["routing", `${operatorConfig.routing.strategy}, fallback attempts ${operatorConfig.routing.fallback_attempts}`],
          ["body limit bytes", operatorConfig.server.request_body_limit_bytes],
          ["trusted proxies", compactList(operatorConfig.server.trusted_proxy_cidrs)],
          ["debug capture", operatorConfig.debug_capture.enabled ? `enabled: ${operatorConfig.debug_capture.directory}` : "disabled"],
          ["inspector remote", operatorConfig.inspector.allow_remote ? "allowed" : "loopback-only"],
          ["health probes", operatorConfig.health.active ? `${operatorConfig.health.path} every ${operatorConfig.health.interval_ms} ms` : "disabled"],
        ]),
        kvCard("model visibility", [
          ["public models", compactList(operatorModels.public_models.map(model => model.public))],
          ["clients", compactList(operatorModels.clients.map(client => `${client.id} (${client.models.length})`))],
          ["backend routes", modelRouteCount],
        ]),
      );
    }

    function kvCard(title, pairs) {
      const card = document.createElement("div");
      card.className = "card";
      const heading = document.createElement("h2");
      heading.textContent = title;
      const grid = document.createElement("div");
      grid.className = "kv";
      for (const [key, value] of pairs) {
        const k = document.createElement("div");
        k.className = "key";
        k.textContent = key;
        const v = document.createElement("div");
        v.textContent = value === null || value === undefined || value === "" ? "none" : String(value);
        grid.append(k, v);
      }
      card.append(heading, grid);
      return card;
    }

    function timelineCard(record) {
      const card = document.createElement("div");
      card.className = "card";
      const heading = document.createElement("h2");
      heading.textContent = "timeline";
      const list = document.createElement("div");
      list.className = "timeline";
      const timeline = record.timeline || {};
      const total = Number(timeline.total_us || 0);
      for (const [label, field] of timelineFields) {
        const value = timeline[field];
        if (value === null || value === undefined) continue;
        const row = document.createElement("div");
        row.className = "bar-row";
        const name = document.createElement("div");
        name.className = "bar-label";
        name.textContent = label;
        const number = document.createElement("div");
        number.className = "bar-value";
        number.textContent = `${ms(value)} ms`;
        const track = document.createElement("div");
        track.className = "bar-track";
        const fill = document.createElement("div");
        fill.className = "bar-fill";
        fill.style.width = `${Math.max(1, Math.min(100, total ? Number(value) / total * 100 : 1))}%`;
        track.appendChild(fill);
        row.append(name, number, track);
        list.appendChild(row);
      }
      card.append(heading, list);
      return card;
    }

    function percent(value, total) {
      if (!Number.isFinite(value) || !Number.isFinite(total) || total <= 0) return 0;
      return Math.max(0, Math.min(100, value / total * 100));
    }

    function phaseMs(value) {
      if (value === null || value === undefined) return "";
      return `${ms(value)} ms`;
    }

    function attemptDetailSet(record) {
      let set = expandedAttemptDetails.get(record.record_id);
      if (!set) {
        set = new Set();
        expandedAttemptDetails.set(record.record_id, set);
      }
      return set;
    }

    function attemptKey(attempt, index) {
      return String(attempt.attempt || index + 1);
    }

    function attemptExpanded(record, attempt, index) {
      const set = expandedAttemptDetails.get(record.record_id);
      return set ? set.has(attemptKey(attempt, index)) : false;
    }

    function setAttemptExpanded(record, attempt, index, expanded) {
      const key = attemptKey(attempt, index);
      if (expanded) {
        const set = attemptDetailSet(record);
        set.add(key);
      } else {
        const set = expandedAttemptDetails.get(record.record_id);
        if (!set) return;
        set.delete(key);
        if (!set.size) expandedAttemptDetails.delete(record.record_id);
      }
    }

    function setAllAttemptsExpanded(record, attempts, expanded) {
      if (expanded) {
        const set = attemptDetailSet(record);
        attempts.forEach((attempt, index) => set.add(attemptKey(attempt, index)));
      } else {
        expandedAttemptDetails.delete(record.record_id);
      }
      renderDetail(record);
    }

    function expandedAttemptCount(record, attempts) {
      return attempts.reduce(
        (count, attempt, index) => count + (attemptExpanded(record, attempt, index) ? 1 : 0),
        0,
      );
    }

    function attemptDetailPairs(attempt) {
      return [
        ["backend target", attempt.backend_target],
        ["backend remote", attempt.backend_remote_addr],
        ["status", attempt.status],
        ["upstream status", attempt.upstream_status],
        ["outcome", attempt.outcome],
        ["error kind", attempt.error_kind],
        ["elapsed", phaseMs(attempt.elapsed_us)],
        ["debug capture id", attempt.debug_capture_id],
        ["request rewrite", phaseMs(attempt.request_rewritten_us)],
        ["debug capture", phaseMs(attempt.debug_capture_done_us)],
        ["backend send start", phaseMs(attempt.backend_forward_start_us)],
        ["upstream headers", phaseMs(attempt.backend_headers_received_us)],
        ["first body chunk", phaseMs(attempt.backend_body_first_chunk_us)],
        ["body complete", phaseMs(attempt.backend_body_complete_us)],
        ["stream complete", phaseMs(attempt.stream_complete_us)],
      ];
    }

    function attemptDetailGrid(attempt) {
      const grid = document.createElement("div");
      grid.className = "kv waterfall-kv";
      for (const [key, value] of attemptDetailPairs(attempt)) {
        const k = document.createElement("div");
        k.className = "key";
        k.textContent = key;
        const v = document.createElement("div");
        v.textContent = value === null || value === undefined || value === "" ? "none" : String(value);
        grid.append(k, v);
      }
      return grid;
    }

    function attemptPhaseList(attempt) {
      const phases = document.createElement("div");
      phases.className = "waterfall-phase-list";
      for (const [name, field] of attemptPhaseFields) {
        const value = attempt[field];
        if (value === null || value === undefined) continue;
        const event = document.createElement("span");
        event.className = "waterfall-event";
        event.textContent = `${name} ${ms(value)} ms`;
        event.title = `${name}: ${ms(value)} ms`;
        phases.appendChild(event);
      }
      return phases;
    }

    function attemptWaterfallCard(record) {
      const attempts = attemptRecords(record);
      if (!attempts.length) return null;
      const timeline = record.timeline || {};
      const total = Math.max(
        Number(timeline.total_us || 0),
        ...attempts.map(attempt => Number(attempt.ended_us || 0)),
      );
      const card = document.createElement("div");
      card.className = "card";
      const heading = document.createElement("h2");
      heading.textContent = "attempt waterfall";
      const controls = document.createElement("div");
      controls.className = "waterfall-controls";
      const expandedCount = expandedAttemptCount(record, attempts);
      const controlsLabel = document.createElement("div");
      controlsLabel.textContent = `${expandedCount}/${attempts.length} attempt detail${attempts.length === 1 ? "" : "s"} expanded`;
      const controlActions = document.createElement("div");
      controlActions.className = "waterfall-controls-actions";
      const expandAll = document.createElement("button");
      expandAll.type = "button";
      expandAll.textContent = "expand all";
      expandAll.title = "Show dense metadata and phase timings for every backend attempt.";
      expandAll.addEventListener("click", () => setAllAttemptsExpanded(record, attempts, true));
      const collapseAll = document.createElement("button");
      collapseAll.type = "button";
      collapseAll.textContent = "collapse all";
      collapseAll.title = "Hide per-attempt detail panes and keep the waterfall compact.";
      collapseAll.addEventListener("click", () => setAllAttemptsExpanded(record, attempts, false));
      controlActions.append(expandAll, collapseAll);
      controls.append(controlsLabel, controlActions);
      const list = document.createElement("div");
      list.className = "waterfall";

      const scale = document.createElement("div");
      scale.className = "waterfall-scale";
      const scaleLabel = document.createElement("div");
      scaleLabel.textContent = "request";
      const axis = document.createElement("div");
      axis.className = "waterfall-axis";
      [0, total / 2, total].forEach(value => {
        const tick = document.createElement("span");
        tick.textContent = `${ms(value)} ms`;
        axis.appendChild(tick);
      });
      scale.append(scaleLabel, axis);
      list.appendChild(scale);

      for (const [index, attempt] of attempts.entries()) {
        const started = Number(attempt.started_us || 0);
        const ended = Number(attempt.ended_us || started);
        const isExpanded = attemptExpanded(record, attempt, index);
        const row = document.createElement("div");
        row.className = "waterfall-row";
        if (isExpanded) row.classList.add("expanded");

        const label = document.createElement("div");
        label.className = "waterfall-label";
        const title = document.createElement("strong");
        title.textContent = `#${attempt.attempt} ${attempt.backend || "unknown"}`;
        const detail = document.createElement("span");
        detail.className = "waterfall-detail";
        detail.textContent = [
          attempt.outcome,
          `status ${attempt.status}`,
          attempt.upstream_status ? `upstream ${attempt.upstream_status}` : null,
          `${ms(attempt.elapsed_us)} ms`,
        ].filter(Boolean).join(" · ");
        label.append(title, detail);

        const track = document.createElement("div");
        track.className = "waterfall-track";
        const span = document.createElement("div");
        span.className = `waterfall-span ${statusClass(Number(attempt.status), null)}`;
        span.style.left = `${percent(started, total)}%`;
        span.style.width = `${Math.max(0.6, percent(ended - started, total))}%`;
        span.title = [
          attempt.backend_target,
          attempt.backend_remote_addr,
          attempt.debug_capture_id ? `capture ${attempt.debug_capture_id}` : null,
        ].filter(Boolean).join(" · ");
        track.appendChild(span);

        for (const [name, field] of attemptPhaseFields) {
          const value = attempt[field];
          if (value === null || value === undefined) continue;
          const marker = document.createElement("div");
          marker.className = "waterfall-tick";
          marker.style.left = `${percent(Number(value), total)}%`;
          marker.title = `${name}: ${ms(value)} ms`;
          track.appendChild(marker);
        }

        const toggle = document.createElement("button");
        toggle.type = "button";
        toggle.className = "waterfall-toggle";
        toggle.textContent = isExpanded ? "hide details" : "details";
        toggle.title = isExpanded
          ? "Hide dense metadata and phase timings for this attempt."
          : "Show dense metadata and phase timings for this attempt.";
        toggle.addEventListener("click", () => {
          setAttemptExpanded(record, attempt, index, !isExpanded);
          renderDetail(record);
        });

        const body = document.createElement("div");
        body.className = "waterfall-row-body";
        const phaseList = attemptPhaseList(attempt);
        body.appendChild(attemptDetailGrid(attempt));
        if (phaseList.childElementCount) body.appendChild(phaseList);

        row.append(label, track, toggle, body);
        list.appendChild(row);
      }

      card.append(heading, controls, list);
      return card;
    }

    function retryAttemptsCard(record) {
      const attempts = record.retried_attempts || [];
      if (!attempts.length) return null;
      return kvCard("retried attempts", attempts.map(attempt => [
        `#${attempt.attempt} ${attempt.backend}`,
        [
          attempt.outcome,
          `status ${attempt.status}`,
          attempt.error_kind,
          `${attempt.elapsed_ms} ms`,
          attempt.backend_target,
          attempt.backend_remote_addr,
          attempt.debug_capture_id ? `capture ${attempt.debug_capture_id}` : null,
        ].filter(Boolean).join(" · "),
      ]));
    }

    function jsonCard(record) {
      const card = document.createElement("div");
      card.className = "card";
      const heading = document.createElement("h2");
      heading.textContent = "raw json";
      const pre = document.createElement("pre");
      pre.textContent = JSON.stringify(record, null, 2);
      card.append(heading, pre);
      return card;
    }

    function recordJson(record) {
      return `${JSON.stringify(record, null, 2)}\n`;
    }

    async function copyText(text) {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(text);
        return;
      }

      const textarea = document.createElement("textarea");
      textarea.value = text;
      textarea.setAttribute("readonly", "");
      textarea.style.position = "fixed";
      textarea.style.left = "-9999px";
      document.body.appendChild(textarea);
      textarea.select();
      const copied = document.execCommand("copy");
      textarea.remove();
      if (!copied) throw new Error("copy command failed");
    }

    function downloadRecordJson(record) {
      const blob = new Blob([recordJson(record)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `onair-request-${record.record_id}.json`;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(url);
    }

    function actionsCard(record) {
      const card = document.createElement("div");
      card.className = "card";
      const heading = document.createElement("h2");
      heading.textContent = "actions";
      const actions = document.createElement("div");
      actions.className = "actions";

      const copyButton = document.createElement("button");
      copyButton.type = "button";
      copyButton.textContent = "copy record json";
      copyButton.title = "Copy the selected inspector record JSON to the clipboard.";
      copyButton.addEventListener("click", async () => {
        try {
          await copyText(recordJson(record));
          setStatus("copied record json", "live");
        } catch (error) {
          setStatus(`copy failed: ${error.message}`, "dead");
        }
      });

      const downloadButton = document.createElement("button");
      downloadButton.type = "button";
      downloadButton.textContent = "download json";
      downloadButton.title = "Download the selected inspector record JSON.";
      downloadButton.addEventListener("click", () => {
        downloadRecordJson(record);
        setStatus("downloaded record json", "live");
      });

      actions.append(copyButton, downloadButton);
      card.append(heading, actions);
      return card;
    }

      function renderDetail(record) {
      const cards = [
        kvCard("request", [
          ["record id", record.record_id],
          ["deep link", recordLink(record)],
          ["client request id", record.client_request_id],
          ["started", new Date(record.started_at_unix_ms).toISOString()],
          ["completed", new Date(record.completed_at_unix_ms).toISOString()],
          ["method", record.method],
          ["path", record.query ? `${record.path}?${record.query}` : record.path],
          ["route", record.route],
          ["identity", record.identity],
          ["status", record.status],
          ["outcome", outcomeText(record)],
          ["error kind", record.error_kind],
          ["exposed backend error", record.exposed_backend_error ? "yes" : "no"],
        ]),
        kvCard("routing", [
          ["requested model", record.requested_model],
          ["public model", record.public_model],
          ["backend model", record.backend_model],
          ["backend", record.backend],
          ["backend target", record.backend_target],
          ["backend remote addr", record.backend_remote_addr],
          ["stream", record.stream ? "yes" : "no"],
        ]),
        kvCard("client", [
          ["peer addr", record.peer_addr],
          ["effective client", record.effective_client_addr],
          ["trusted proxy", record.trusted_proxy_addr],
          ["forwarded for", record.forwarded_for],
          ["user agent", record.user_agent],
        ]),
        kvCard("sizes and usage", [
          ["request body bytes", record.request_body_bytes],
          ["response body bytes", record.response_body_bytes],
          ["input tokens", record.input_tokens],
          ["cached input tokens", record.cached_input_tokens],
          ["output tokens", record.output_tokens],
          ["debug capture id", record.debug_capture_id],
        ]),
      ];
      cards.push(actionsCard(record));
      const waterfallCard = attemptWaterfallCard(record);
      if (waterfallCard) cards.push(waterfallCard);
      const retryCard = retryAttemptsCard(record);
      if (retryCard) cards.push(retryCard);
      cards.push(timelineCard(record), jsonCard(record));
      detail.replaceChildren(...cards);
    }

    async function fetchJson(path) {
      const response = await fetch(path, { cache: "no-store" });
      if (!response.ok) throw new Error(`${path} failed: HTTP ${response.status}`);
      return response.json();
    }

    async function reloadOperator() {
      [operatorRuntime, operatorConfig, operatorModels, operatorHealth] = await Promise.all([
        fetchJson(`${operatorBase}/runtime`),
        fetchJson(`${operatorBase}/config`),
        fetchJson(`${operatorBase}/models`),
        fetchJson(`${operatorBase}/health`),
      ]);
      renderOverview();
    }

    function scheduleOperatorReload() {
      if (operatorReloadPending) return;
      operatorReloadPending = true;
      setTimeout(() => {
        operatorReloadPending = false;
        reloadOperator().catch(error => setStatus(error.message, "dead"));
      }, 250);
    }

    async function reload() {
      await reloadOperator();
      const list = await fetchJson(`${base}/requests?limit=${snapshotLimit}`);
      records.clear();
      pendingRecords.clear();
      for (const record of list) records.set(record.record_id, record);
      if (operatorRuntime) operatorRuntime.inspector_retained_requests = records.size;
      renderRows();
      renderOverview();
      updatePauseButton();
      const target = selectedId || hashRecordId();
      if (target) await selectRecordId(target, false);
    }

    function connect() {
      if (source) source.close();
      source = new EventSource(`${base}/events?snapshot_limit=${snapshotLimit}`);
      source.addEventListener("open", () => {
        if (livePaused) {
          updatePauseButton();
        } else {
          setStatus("live", "live");
        }
      });
      source.addEventListener("snapshot", event => upsert(JSON.parse(event.data)));
      source.addEventListener("request", event => upsert(JSON.parse(event.data)));
      source.addEventListener("lagged", () => reload().catch(error => setStatus(error.message, "dead")));
      source.addEventListener("error", () => {
        setStatus("reconnecting", "dead");
      });
    }

    document.getElementById("reload").addEventListener("click", () => {
      reload().then(() => setStatus("reloaded", "live")).catch(error => setStatus(error.message, "dead"));
    });
    pauseButton.addEventListener("click", () => {
      livePaused = !livePaused;
      if (!livePaused) {
        if (pendingRecords.size) {
          for (const record of pendingRecords.values()) applyRecord(record);
          pendingRecords.clear();
          renderAfterRecordChange();
        }
        setStatus("live", "live");
      }
      updatePauseButton();
    });
    filter.addEventListener("input", renderRows);
    filter.addEventListener("search", renderRows);
    for (const menu of document.querySelectorAll(".toolbar-menu")) {
      menu.addEventListener("toggle", () => {
        if (menu.open) closeToolbarMenus(menu);
      });
    }
    document.addEventListener("pointerdown", event => {
      if (!(event.target instanceof Node)) return;
      let insideMenu = false;
      for (const menu of document.querySelectorAll(".toolbar-menu")) {
        if (menu.contains(event.target)) {
          insideMenu = true;
          break;
        }
      }
      if (!insideMenu) closeToolbarMenus();
    });
    document.addEventListener("keydown", event => {
      if (event.key === "Escape") {
        const openMenu = document.querySelector(".toolbar-menu[open]");
        if (!openMenu) return;
        openMenu.open = false;
        const summary = openMenu.querySelector("summary");
        if (summary) summary.focus();
      }
    });
    window.addEventListener("hashchange", () => {
      const target = hashRecordId();
      if (target) {
        selectRecordId(target, false).catch(error => showDetailMessage(error.message));
      } else {
        selectedId = null;
        renderRows();
        showDetailMessage("Select a request to inspect timings and metadata.");
      }
    });

    viewPresets = loadViewPresets();
    renderQuickFilters();
    renderColumnOptions();
    renderViewPresets();
    renderTableHeader();
    updatePauseButton();
    renderOverview();
    reload().catch(error => setStatus(error.message, "dead")).finally(connect);
