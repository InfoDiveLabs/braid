// Braid's web UI. Plain JavaScript, no build step: this file is served
// exactly as written, so it has to run in whatever browser opens it rather
// than whatever a bundler would have transformed it into.
"use strict";

/* ============================================================ formatting */

// Matches `crates/dl-gui/src/bridge.rs::format_bytes` exactly: decimal units,
// dividing by 1000 rather than 1024, so a number means the same thing here
// as it does in the desktop app and on disk.
function formatBytes(n) {
  if (n === null || n === undefined) return "--";
  const UNITS = ["B", "KB", "MB", "GB", "TB"];
  let value = n;
  let unit = 0;
  while (value >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  return unit === 0 ? `${n} B` : `${value.toFixed(1)} ${UNITS[unit]}`;
}

function formatSpeed(n) {
  return `${formatBytes(n)}/s`;
}

function formatPercent(p) {
  return p === null || p === undefined ? "--" : `${p.toFixed(1)}%`;
}

// Keyed by `dl_core::engine::State::as_str`, which spells a failure "error"
// rather than "failed": that string is the wire format, this is only what a
// person reads on the badge.
const STATE_LABELS = {
  queued: "Queued",
  running: "Running",
  paused: "Paused",
  seeding: "Seeding",
  done: "Done",
  error: "Failed",
};

function stateLabel(state) {
  return STATE_LABELS[state] || state;
}

function escapeHtml(text) {
  const div = document.createElement("div");
  div.textContent = text == null ? "" : String(text);
  return div.innerHTML;
}

/* ==================================================================== icons */

// Inline rather than an icon font or a sprite sheet fetched separately: the
// whole point of this file is that nothing it draws needs a network request
// beyond itself. `currentColor` lets each button's own CSS decide the tint.
const ICONS = {
  pause:
    '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="4" y="3" width="3" height="10" rx="1" fill="currentColor"/><rect x="9" y="3" width="3" height="10" rx="1" fill="currentColor"/></svg>',
  resume:
    '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3v10l8-5-8-5z" fill="currentColor"/></svg>',
  remove:
    '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 5h8M6.5 5V3.4h3V5M5 5l.6 8a1 1 0 0 0 1 .9h2.8a1 1 0 0 0 1-.9L11 5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/></svg>',
};

function iconButton(action, title, danger) {
  const icon = ICONS[action] || "";
  return `<button type="button" class="icon-btn${danger ? " icon-btn-danger" : ""}" data-action="${action}" title="${escapeHtml(title)}" aria-label="${escapeHtml(title)}">${icon}</button>`;
}

/* =================================================================== api */

// Every call to the server's own API goes through here, so a 403 is caught
// in one place rather than at every call site: a session can expire between
// one click and the next, and every screen has to react to that the same
// way, by going back to the login form.
async function api(path, options) {
  const response = await fetch(path, {
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    ...options,
  });
  if (response.status === 403) {
    onSessionLost();
    throw new Error("not authenticated");
  }
  return response;
}

async function apiJson(path, options) {
  const response = await api(path, options);
  const body = await response.json().catch(() => ({}));
  return { ok: response.ok, status: response.status, body };
}

/* ================================================================== state */

const state = {
  transfers: new Map(),
  totalBytesPerSec: 0,
  // The set of rows highlighted for bulk action. A plain click replaces it
  // with one id; ctrl/cmd-click toggles a member; shift-click replaces it
  // with a range. The detail panel opens only when it holds exactly one id,
  // tracked separately in `selectedId` so a two-row selection can still say
  // plainly "no single transfer to show details for".
  selection: new Set(),
  selectedId: null,
  anchorId: null,
  detailTab: "info",
  eventSource: null,
  piecesTimer: null,
  settingsInterfaces: [],
  filterState: "all",
  filterCategory: "",
  searchQuery: "",
  sortKey: "name",
  sortDir: "asc",
};

/* ================================================================ screens */

const loginScreen = document.getElementById("login-screen");
const mainScreen = document.getElementById("main-screen");

function showLogin() {
  loginScreen.style.display = "flex";
  mainScreen.classList.remove("active");
  stopEvents();
}

function showMain() {
  loginScreen.style.display = "none";
  mainScreen.classList.add("active");
}

function onSessionLost() {
  showLogin();
}

/* =================================================================== boot */

async function boot() {
  wireLogin();
  wireLogout();
  wireViewTabs();
  wireAddForm();
  wireDetailPanel();
  wireSettingsForm();
  wireDropZone();
  wireFilterBar();
  wireSortableHeaders();
  wireBulkBar();
  wireStatsBarActions();

  // The only way to know whether a cookie already proves a session: ask for
  // something that requires one and see whether it is refused.
  const probe = await fetch("/api/v1/transfers", { credentials: "same-origin" }).catch(() => null);
  if (probe && probe.ok) {
    showMain();
    startEvents();
  } else {
    showLogin();
  }
}

/* =================================================================== login */

function wireLogin() {
  const form = document.getElementById("login-form");
  const error = document.getElementById("login-error");
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    const username = form.elements.username.value;
    const password = form.elements.password.value;
    try {
      const response = await fetch("/api/v1/login", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username, password }),
      });
      if (!response.ok) {
        error.textContent = "Wrong username or password.";
        error.hidden = false;
        return;
      }
      form.elements.password.value = "";
      showMain();
      startEvents();
    } catch (err) {
      error.textContent = "Could not reach the server.";
      error.hidden = false;
    }
  });
}

function wireLogout() {
  document.getElementById("logout-btn").addEventListener("click", async () => {
    await fetch("/api/v1/logout", { method: "POST", credentials: "same-origin" }).catch(() => {});
    showLogin();
  });
}

/* ============================================================= view tabs */

function wireViewTabs() {
  const tabs = document.getElementById("view-tabs");
  tabs.addEventListener("click", (event) => {
    const button = event.target.closest(".view-tab");
    if (!button) return;
    for (const tab of tabs.querySelectorAll(".view-tab")) tab.classList.toggle("active", tab === button);
    const view = button.dataset.view;
    document.getElementById("transfers-view").hidden = view !== "transfers";
    document.getElementById("settings-view").hidden = view !== "settings";
    if (view === "settings") loadSettings();
  });
}

/* ============================================================ event stream */

function startEvents() {
  if (state.eventSource) return;
  const source = new EventSource("/api/v1/events");
  source.onmessage = (event) => {
    let frame;
    try {
      frame = JSON.parse(event.data);
    } catch (err) {
      return;
    }
    applyFrame(frame);
  };
  source.onerror = async () => {
    // `EventSource` retries on its own for an ordinary network hiccup; the
    // one case worth checking for is a session that has since expired,
    // which looks identical from here until something asks and gets a 403.
    const probe = await fetch("/api/v1/transfers", { credentials: "same-origin" }).catch(() => null);
    if (probe && probe.status === 403) onSessionLost();
  };
  state.eventSource = source;
}

function stopEvents() {
  if (state.eventSource) {
    state.eventSource.close();
    state.eventSource = null;
  }
  stopPiecesPolling();
}

function applyFrame(frame) {
  state.totalBytesPerSec = frame.total_bytes_per_sec || 0;

  const seen = new Set();
  for (const transfer of frame.transfers) {
    state.transfers.set(transfer.id, transfer);
    seen.add(transfer.id);
  }
  for (const id of Array.from(state.transfers.keys())) {
    if (!seen.has(id)) state.transfers.delete(id);
  }
  // A vanished transfer (removed from another tab, or finished and pruned)
  // cannot stay in a selection nobody can act on any more.
  for (const id of Array.from(state.selection)) {
    if (!seen.has(id)) state.selection.delete(id);
  }

  renderStatsBar();
  renderCategoryOptions();
  renderTable();
  renderBulkBar();

  if (state.selectedId !== null) {
    if (!seen.has(state.selectedId)) {
      closeDetail();
    } else {
      renderDetailFromState();
    }
  }
}

/* ============================================================= stats bar */

function renderStatsBar() {
  document.getElementById("stat-down").textContent = formatSpeed(state.totalBytesPerSec);

  let upload = 0;
  let active = 0;
  for (const t of state.transfers.values()) {
    if (t.torrent && t.torrent.upload_bytes_per_sec) upload += t.torrent.upload_bytes_per_sec;
    if (t.state === "running") active += 1;
  }
  document.getElementById("stat-up").textContent = formatSpeed(upload);
  document.getElementById("stat-active").textContent = String(active);
  document.getElementById("stat-total").textContent = String(state.transfers.size);
}

function wireStatsBarActions() {
  document.getElementById("pause-all-btn").addEventListener("click", async () => {
    const ids = [];
    for (const t of state.transfers.values()) {
      if (t.state === "running" || t.state === "queued") ids.push(t.id);
    }
    await Promise.all(ids.map((id) => api(`/api/v1/transfers/${id}/pause`, { method: "POST" })));
  });
  document.getElementById("resume-all-btn").addEventListener("click", async () => {
    const ids = [];
    for (const t of state.transfers.values()) {
      if (t.state === "paused" || t.state === "error") ids.push(t.id);
    }
    await Promise.all(ids.map((id) => api(`/api/v1/transfers/${id}/resume`, { method: "POST" })));
  });
}

/* ============================================================= filter bar */

// One place both the table and a shift-click range read the current row
// order from, so "the eleventh visible row" means the same thing to each.
function viewRows() {
  const query = state.searchQuery.trim().toLowerCase();
  const rows = Array.from(state.transfers.values()).filter((t) => {
    if (!matchesStateFilter(t, state.filterState)) return false;
    if (state.filterCategory === "__none__" && t.category) return false;
    if (state.filterCategory && state.filterCategory !== "__none__" && t.category !== state.filterCategory) {
      return false;
    }
    if (query && !t.filename.toLowerCase().includes(query)) return false;
    return true;
  });
  rows.sort((a, b) => compareTransfers(a, b, state.sortKey));
  if (state.sortDir === "desc") rows.reverse();
  return rows;
}

function matchesStateFilter(t, group) {
  switch (group) {
    case "downloading":
      return t.state === "running" || t.state === "queued";
    case "seeding":
      return t.state === "seeding";
    case "completed":
      return t.state === "done";
    case "paused":
      return t.state === "paused";
    case "failed":
      return t.state === "error";
    default:
      return true;
  }
}

function sortValue(t, key) {
  switch (key) {
    case "size":
      return t.total != null ? t.total : t.downloaded;
    case "progress":
      return t.percent != null ? t.percent : 0;
    case "speed":
      return t.state === "running" ? t.bytes_per_sec || 0 : 0;
    case "state":
      return stateLabel(t.state).toLowerCase();
    case "name":
    default:
      return (t.filename || "").toLowerCase();
  }
}

function compareTransfers(a, b, key) {
  const va = sortValue(a, key);
  const vb = sortValue(b, key);
  if (typeof va === "string") return va.localeCompare(vb);
  return va - vb;
}

function computeStateCounts() {
  const counts = { all: 0, downloading: 0, seeding: 0, completed: 0, paused: 0, failed: 0 };
  for (const t of state.transfers.values()) {
    counts.all += 1;
    if (t.state === "running" || t.state === "queued") counts.downloading += 1;
    else if (t.state === "seeding") counts.seeding += 1;
    else if (t.state === "done") counts.completed += 1;
    else if (t.state === "paused") counts.paused += 1;
    else if (t.state === "error") counts.failed += 1;
  }
  return counts;
}

function renderStateCounts() {
  const counts = computeStateCounts();
  for (const [key, value] of Object.entries(counts)) {
    const el = document.getElementById(`count-${key}`);
    if (el) el.textContent = value > 0 ? String(value) : "";
  }
}

function renderCategoryOptions() {
  const select = document.getElementById("category-filter");
  const datalist = document.getElementById("category-options");
  const categories = new Set();
  let hasUncategorized = false;
  for (const t of state.transfers.values()) {
    if (t.category) categories.add(t.category);
    else hasUncategorized = true;
  }
  const sorted = Array.from(categories).sort((a, b) => a.localeCompare(b));

  const previous = select.value;
  select.innerHTML =
    '<option value="">All categories</option>' +
    sorted.map((c) => `<option value="${escapeHtml(c)}">${escapeHtml(c)}</option>`).join("") +
    (hasUncategorized ? '<option value="__none__">Uncategorized</option>' : "");
  const stillValid = Array.from(select.options).some((o) => o.value === previous);
  select.value = stillValid ? previous : "";
  state.filterCategory = select.value;

  datalist.innerHTML = sorted.map((c) => `<option value="${escapeHtml(c)}"></option>`).join("");
}

function wireFilterBar() {
  const tabs = document.getElementById("state-tabs");
  tabs.addEventListener("click", (event) => {
    const button = event.target.closest(".state-tab");
    if (!button) return;
    for (const tab of tabs.querySelectorAll(".state-tab")) tab.classList.toggle("active", tab === button);
    state.filterState = button.dataset.state;
    renderTable();
  });

  document.getElementById("category-filter").addEventListener("change", (event) => {
    state.filterCategory = event.target.value;
    renderTable();
  });

  document.getElementById("search-input").addEventListener("input", (event) => {
    state.searchQuery = event.target.value;
    renderTable();
  });
}

function wireSortableHeaders() {
  document.getElementById("transfers-table").querySelector("thead").addEventListener("click", (event) => {
    const th = event.target.closest("th[data-sort]");
    if (!th) return;
    const key = th.dataset.sort;
    if (state.sortKey === key) {
      state.sortDir = state.sortDir === "asc" ? "desc" : "asc";
    } else {
      state.sortKey = key;
      state.sortDir = "asc";
    }
    renderTable();
  });
}

function renderSortIndicators() {
  for (const th of document.querySelectorAll("#transfers-table thead th[data-sort]")) {
    const active = th.dataset.sort === state.sortKey;
    th.classList.toggle("sort-active", active);
    th.classList.toggle("sort-asc", active && state.sortDir === "asc");
    th.classList.toggle("sort-desc", active && state.sortDir === "desc");
  }
}

/* ================================================================== table */

function renderTable() {
  renderStateCounts();
  renderSortIndicators();

  const body = document.getElementById("transfers-body");
  const emptyNote = document.getElementById("empty-note");
  const noMatchNote = document.getElementById("no-match-note");
  const rows = viewRows();

  emptyNote.hidden = state.transfers.size > 0;
  noMatchNote.hidden = state.transfers.size === 0 || rows.length > 0;

  body.innerHTML = rows.map(rowHtml).join("");

  for (const tr of body.querySelectorAll("tr[data-id]")) {
    const id = Number(tr.dataset.id);
    tr.addEventListener("click", (event) => {
      if (event.target.closest("button")) return;
      handleRowClick(id, event);
    });
  }
  for (const button of body.querySelectorAll("[data-action]")) {
    button.addEventListener("click", async (event) => {
      event.stopPropagation();
      const id = Number(button.closest("tr").dataset.id);
      const action = button.dataset.action;
      if (action === "pause") await api(`/api/v1/transfers/${id}/pause`, { method: "POST" });
      if (action === "resume") await api(`/api/v1/transfers/${id}/resume`, { method: "POST" });
      if (action === "remove") {
        // No confirmation: this leaves files on disk untouched, the same
        // contract the bulk "Remove" action makes. Only "remove with files",
        // reachable from the bulk bar once something is selected, asks first.
        await api(`/api/v1/transfers/${id}`, { method: "DELETE" });
        const wasDetail = state.selectedId === id;
        state.selection.delete(id);
        if (wasDetail) closeDetail();
        else {
          renderTable();
          renderBulkBar();
        }
      }
    });
  }
}

function handleRowClick(id, event) {
  if (event.shiftKey && state.anchorId !== null) {
    const ids = viewRows().map((t) => t.id);
    const a = ids.indexOf(state.anchorId);
    const b = ids.indexOf(id);
    if (a === -1 || b === -1) {
      selectSingle(id);
    } else {
      const [lo, hi] = a < b ? [a, b] : [b, a];
      state.selection = new Set(ids.slice(lo, hi + 1));
      syncSelectedId();
    }
  } else if (event.metaKey || event.ctrlKey) {
    if (state.selection.has(id)) {
      state.selection.delete(id);
    } else {
      state.selection.add(id);
    }
    state.anchorId = id;
    syncSelectedId();
  } else {
    selectSingle(id);
  }

  renderTable();
  renderBulkBar();

  if (state.selectedId !== null) {
    document.getElementById("detail-panel").hidden = false;
    state.detailTab = "info";
    renderDetailFromState();
    loadChecksum(state.selectedId);
  } else {
    document.getElementById("detail-panel").hidden = true;
    stopPiecesPolling();
  }
}

function selectSingle(id) {
  state.selection = new Set([id]);
  state.anchorId = id;
  state.selectedId = id;
}

function syncSelectedId() {
  state.selectedId = state.selection.size === 1 ? Array.from(state.selection)[0] : null;
}

function rowHtml(t) {
  const percent = t.percent;
  const pct = percent === null || percent === undefined ? 0 : percent;
  const selected = state.selection.has(t.id) ? " selected" : "";
  const categoryPill = t.category ? `<span class="category-pill">${escapeHtml(t.category)}</span>` : "";
  return `
    <tr data-id="${t.id}" class="${selected.trim()}">
      <td>
        <div class="row-name">
          <div class="name-line">
            <span class="name">${escapeHtml(t.filename)}</span>
            ${categoryPill}
          </div>
          <span class="host">${escapeHtml(t.host || "")}</span>
        </div>
      </td>
      <td class="col-size">${t.total != null ? formatBytes(t.total) : formatBytes(t.downloaded)}</td>
      <td class="progress-cell">
        <div class="progress-track">
          <div class="progress-fill state-${t.state}" style="width:${pct}%"></div>
        </div>
        <div class="progress-label">${formatPercent(percent)}</div>
      </td>
      <td class="col-speed">${t.state === "running" ? formatSpeed(t.bytes_per_sec) : ""}</td>
      <td class="col-state"><span class="state-badge state-${t.state}">${stateLabel(t.state)}</span></td>
      <td class="col-actions">
        <div class="row-actions">
          ${t.state === "running" ? iconButton("pause", "Pause") : ""}
          ${t.state === "paused" || t.state === "queued" ? iconButton("resume", "Resume") : ""}
          ${t.state === "error" ? iconButton("resume", "Retry") : ""}
          ${iconButton("remove", "Remove", true)}
        </div>
      </td>
    </tr>`;
}

/* ================================================================ bulk bar */

function renderBulkBar() {
  const bar = document.getElementById("bulk-bar");
  const count = state.selection.size;
  // Shown for a selection of one, not just several: "remove with files" has
  // no per-row equivalent (see the row actions' own comment), so the bulk
  // bar is the only place that action exists at all, and it has to reach a
  // single torrent's files just as well as a batch of them.
  bar.hidden = count === 0;
  if (count > 0) {
    document.getElementById("bulk-count").textContent = count === 1 ? "1 selected" : `${count} selected`;
  }
}

function wireBulkBar() {
  document.getElementById("bulk-clear").addEventListener("click", () => {
    state.selection.clear();
    state.selectedId = null;
    state.anchorId = null;
    renderTable();
    renderBulkBar();
  });

  document.querySelector("#bulk-bar .bulk-actions").addEventListener("click", async (event) => {
    const button = event.target.closest("button[data-bulk]");
    if (!button) return;
    const ids = Array.from(state.selection);
    const action = button.dataset.bulk;

    if (action === "pause") {
      const targets = ids.filter((id) => {
        const t = state.transfers.get(id);
        return t && (t.state === "running" || t.state === "queued");
      });
      await Promise.all(targets.map((id) => api(`/api/v1/transfers/${id}/pause`, { method: "POST" })));
    } else if (action === "resume") {
      const targets = ids.filter((id) => {
        const t = state.transfers.get(id);
        return t && (t.state === "paused" || t.state === "error");
      });
      await Promise.all(targets.map((id) => api(`/api/v1/transfers/${id}/resume`, { method: "POST" })));
    } else if (action === "remove") {
      // Files are kept: the same contract a single row's Remove button
      // makes, so this needs no confirmation either.
      await Promise.all(ids.map((id) => api(`/api/v1/transfers/${id}`, { method: "DELETE" })));
      clearSelectionAfterRemoval(ids);
    } else if (action === "remove-files") {
      // Deleting several people's... several downloads' worth of data on one
      // click is exactly the situation that gets a tool uninstalled.
      const warning =
        ids.length === 1
          ? "Permanently delete this transfer's files from disk? This cannot be undone."
          : `Permanently delete the files for all ${ids.length} selected transfers? This cannot be undone.`;
      if (!confirm(warning)) return;
      await Promise.all(
        ids.map((id) => api(`/api/v1/transfers/${id}?delete_files=true`, { method: "DELETE" })),
      );
      clearSelectionAfterRemoval(ids);
    }
  });
}

function clearSelectionAfterRemoval(ids) {
  for (const id of ids) state.selection.delete(id);
  if (state.selectedId !== null && ids.includes(state.selectedId)) closeDetail();
  renderTable();
  renderBulkBar();
}

/* ================================================================= add box */

function wireAddForm() {
  const form = document.getElementById("add-form");
  const disclosure = document.getElementById("add-disclosure");
  const more = document.getElementById("add-more");

  disclosure.addEventListener("click", () => {
    const expanded = disclosure.getAttribute("aria-expanded") === "true";
    disclosure.setAttribute("aria-expanded", String(!expanded));
    more.hidden = expanded;
  });

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const url = document.getElementById("add-url").value.trim();
    if (!url) return;
    await submitNewTransfer({ url });
  });
}

async function submitNewTransfer(payload) {
  const errorBox = document.getElementById("add-error");
  errorBox.hidden = true;

  const destination = document.getElementById("add-destination").value.trim();
  const connections = document.getElementById("add-connections").value.trim();
  const expect = document.getElementById("add-expect").value.trim();
  const category = document.getElementById("add-category").value.trim();
  const startPaused = document.getElementById("add-start-paused").checked;

  const body = { ...payload };
  if (destination) body.destination = destination;
  if (connections) body.connections = Number(connections);
  if (expect) body.expect = expect;
  if (category) body.category = category;
  if (startPaused) body.start_paused = true;

  const { ok, body: result } = await apiJson("/api/v1/transfers", {
    method: "POST",
    body: JSON.stringify(body),
  }).catch((err) => ({ ok: false, body: { error: String(err) } }));

  if (!ok) {
    errorBox.textContent = result.error || "Could not add that transfer.";
    errorBox.hidden = false;
    return;
  }

  document.getElementById("add-url").value = "";
  document.getElementById("add-start-paused").checked = false;
}

/* ============================================================== drag/drop */

function wireDropZone() {
  const overlay = document.getElementById("drop-overlay");
  let dragDepth = 0;

  document.addEventListener("dragenter", (event) => {
    if (!event.dataTransfer || !event.dataTransfer.types.includes("Files")) return;
    dragDepth += 1;
    overlay.hidden = false;
  });
  document.addEventListener("dragover", (event) => {
    if (event.dataTransfer && event.dataTransfer.types.includes("Files")) event.preventDefault();
  });
  document.addEventListener("dragleave", () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) overlay.hidden = true;
  });
  document.addEventListener("drop", async (event) => {
    if (!event.dataTransfer || !event.dataTransfer.files.length) return;
    event.preventDefault();
    dragDepth = 0;
    overlay.hidden = true;

    const file = Array.from(event.dataTransfer.files).find((f) => f.name.toLowerCase().endsWith(".torrent"));
    if (!file) return;
    const base64 = await fileToBase64(file);
    await submitNewTransfer({ torrent_data: base64, filename: file.name });
  });
}

function fileToBase64(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      // `readAsDataURL` gives "data:<mime>;base64,<payload>"; only the part
      // after the comma is the base64 the server's `/api/v1/transfers`
      // expects.
      const commaIndex = reader.result.indexOf(",");
      resolve(reader.result.slice(commaIndex + 1));
    };
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}

/* =============================================================== detail */

function closeDetail() {
  state.selection.clear();
  state.selectedId = null;
  state.anchorId = null;
  document.getElementById("detail-panel").hidden = true;
  stopPiecesPolling();
  renderTable();
  renderBulkBar();
}

function wireDetailPanel() {
  document.getElementById("detail-close").addEventListener("click", closeDetail);
  document.getElementById("detail-tabs").addEventListener("click", (event) => {
    const button = event.target.closest("button[data-tab]");
    if (!button) return;
    state.detailTab = button.dataset.tab;
    renderDetailFromState();
  });
}

// The checksum block is not in the event stream (see `api/v1.rs`'s own
// reasoning: it costs a lookup per transfer, and only one open panel wants
// it), so it is fetched once when a row is opened rather than every tick.
let checksumCache = new Map();
async function loadChecksum(id) {
  const { ok, body } = await apiJson(`/api/v1/transfers/${id}`).catch(() => ({ ok: false }));
  if (ok && body.checksum) {
    checksumCache.set(id, body.checksum);
    if (state.selectedId === id) renderDetailFromState();
  } else {
    checksumCache.delete(id);
  }
}

function renderDetailFromState() {
  const t = state.transfers.get(state.selectedId);
  if (!t) return;

  document.getElementById("detail-title").textContent = t.filename;
  document.getElementById("detail-subtitle").textContent = t.host || "";

  const isTorrent = !!t.torrent;
  const hasFiles = isTorrent && t.torrent.files && t.torrent.files.length > 0;
  const tabs = [
    { id: "info", label: "Info" },
    { id: "pieces", label: "Pieces" },
    { id: "connections", label: isTorrent ? "Peers" : "Connections" },
  ];
  if (hasFiles) tabs.push({ id: "files", label: "Files" });
  if (!tabs.find((tab) => tab.id === state.detailTab)) state.detailTab = "info";

  const tabsBox = document.getElementById("detail-tabs");
  tabsBox.innerHTML = tabs
    .map((tab) => `<button data-tab="${tab.id}" class="${tab.id === state.detailTab ? "active" : ""}">${tab.label}</button>`)
    .join("");

  for (const id of ["info", "pieces", "connections", "files"]) {
    document.getElementById(`detail-${id}`).hidden = id !== state.detailTab;
  }

  renderInfoTab(t);
  if (state.detailTab === "pieces") {
    startPiecesPolling(t.id);
  } else {
    stopPiecesPolling();
  }
  if (state.detailTab === "connections") renderConnectionsTab(t);
  if (state.detailTab === "files" && hasFiles) renderFilesTab(t);
}

function renderInfoTab(t) {
  const checksumBox = document.getElementById("detail-checksum");
  const checksum = checksumCache.get(t.id);
  if (checksum) {
    const verified =
      checksum.verified === true
        ? '<span class="checksum-status verified-yes">Verified</span>'
        : checksum.verified === false
          ? '<span class="checksum-status verified-no">Mismatch</span>'
          : '<span class="checksum-status verified-unknown">Not yet checked</span>';
    checksumBox.hidden = false;
    checksumBox.innerHTML = `
      <div>${checksum.algorithm.toUpperCase()} checksum &middot; ${verified}</div>
      <div class="checksum-hex">${escapeHtml(checksum.hex)}</div>`;
  } else {
    checksumBox.hidden = true;
    checksumBox.innerHTML = "";
  }

  const stats = [];
  stats.push(["State", stateLabel(t.state)]);
  if (t.category) stats.push(["Category", t.category]);
  stats.push(["Downloaded", formatBytes(t.downloaded) + (t.total != null ? ` of ${formatBytes(t.total)}` : "")]);
  if (t.state === "running") {
    stats.push(["Speed", formatSpeed(t.bytes_per_sec)]);
    stats.push(["Average", formatSpeed(t.smoothed_bytes_per_sec)]);
  }
  if (t.phase) stats.push(["Phase", t.phase]);
  if (t.torrent) {
    if (t.torrent.info_hash) stats.push(["Info hash", t.torrent.info_hash]);
    stats.push(["Uploaded", formatBytes(t.torrent.uploaded)]);
    if (t.torrent.upload_bytes_per_sec) stats.push(["Upload speed", formatSpeed(t.torrent.upload_bytes_per_sec)]);
  }
  if (t.error) stats.push(["Error", t.error]);

  const statList = document.getElementById("detail-stats");
  statList.innerHTML = stats
    .map(
      ([label, value]) => `
      <div class="stat-row">
        <dt class="stat-label">${escapeHtml(label)}</dt>
        <dd class="stat-value${label === "Error" ? " error-text" : ""}" style="margin:0">${escapeHtml(value)}</dd>
      </div>`,
    )
    .join("");

  const ifaceBox = document.getElementById("detail-interfaces");
  if (!isTorrentLike(t) && t.lanes && t.lanes.length > 0) {
    const maxBytes = Math.max(...t.lanes.map((l) => l.bytes), 1);
    ifaceBox.innerHTML = t.lanes
      .map(
        (lane, i) => `
        <div class="interface-row">
          <span class="interface-dot" style="background:var(--iface-${i % 5})"></span>
          <span class="interface-name">${escapeHtml(lane.label)}</span>
          <span class="interface-rate">${lane.bytes_per_sec != null ? formatSpeed(lane.bytes_per_sec) : ""}</span>
          <span class="interface-bar">
            <span class="interface-bar-fill" style="width:${(lane.bytes / maxBytes) * 100}%;background:var(--iface-${i % 5})"></span>
          </span>
        </div>`,
      )
      .join("");
  } else {
    ifaceBox.innerHTML = "";
  }
}

function isTorrentLike(t) {
  return !!t.torrent;
}

function renderConnectionsTab(t) {
  const list = document.getElementById("connections-list");
  const empty = document.getElementById("connections-empty");

  if (t.torrent) {
    const peers = t.torrent.peer_list || [];
    empty.hidden = peers.length > 0;
    empty.textContent = "No peers connected right now.";
    list.innerHTML = peers
      .map(
        (peer) => `
        <div class="connection-row">
          <div class="connection-main">
            <span class="connection-label">${escapeHtml(peer.address)}</span>
            <span class="connection-detail">${escapeHtml(peer.client || peer.state)}</span>
          </div>
          <span class="connection-rate">${formatBytes(peer.downloaded)} down &middot; ${formatBytes(peer.uploaded)} up</span>
        </div>`,
      )
      .join("");
  } else {
    const lanes = t.lanes || [];
    empty.hidden = lanes.length > 0;
    empty.textContent = "Nothing carrying traffic right now.";
    list.innerHTML = lanes
      .map(
        (lane) => `
        <div class="connection-row">
          <div class="connection-main">
            <span class="connection-label">${escapeHtml(lane.label)}</span>
            <span class="connection-detail">${lane.parked ? "Parked" : "Active"} &middot; ${formatBytes(lane.bytes)} transferred</span>
          </div>
          <span class="connection-rate">${lane.bytes_per_sec != null ? formatSpeed(lane.bytes_per_sec) : ""}</span>
        </div>`,
      )
      .join("");
  }
}

function renderFilesTab(t) {
  const list = document.getElementById("files-list");
  const files = (t.torrent && t.torrent.files) || [];
  list.innerHTML = files
    .map((file) => {
      const progress = file.len > 0 ? file.downloaded / file.len : 0;
      const complete = progress >= 1;
      return `
        <div class="file-row">
          <div class="file-main">
            <span class="file-name">${escapeHtml(file.path)}</span>
          </div>
          <div class="file-size-wrap">
            <span class="file-size">${formatBytes(file.len)}</span>
            <div class="file-progress-track">
              <div class="file-progress-fill${complete ? " complete" : ""}" style="width:${progress * 100}%"></div>
            </div>
          </div>
        </div>`;
    })
    .join("");
}

/* ================================================================= pieces */

// Merges chunks into at most this many cells, matching
// `crates/dl-gui/src/bridge.rs::MAX_CELLS` (18 columns by 32 rows) exactly:
// a grid that silently drew a prefix of a large torrent would be a lie about
// what is actually on disk, so a huge one is bucketed instead of truncated.
const MAX_CELLS = 18 * 32;
const GRID_COLUMNS = 18;

function bucketFor(count) {
  return count <= MAX_CELLS ? 1 : Math.ceil(count / MAX_CELLS);
}

function isComplete(bitmap, index) {
  const byte = bitmap[index >> 3];
  return byte !== undefined && (byte & (1 << (index % 8))) !== 0;
}

function cellsFor(chunkCount, bitmap, inflight) {
  const bucket = bucketFor(chunkCount);
  const cellCount = Math.ceil(chunkCount / bucket);
  const inflightSet = new Set(inflight);
  const cells = [];
  for (let cell = 0; cell < cellCount; cell += 1) {
    const first = cell * bucket;
    const last = Math.min(first + bucket, chunkCount);
    const span = Math.max(last - first, 1);
    let done = 0;
    let busy = false;
    for (let i = first; i < last; i += 1) {
      if (isComplete(bitmap, i)) done += 1;
      if (inflightSet.has(i)) busy = true;
    }
    const cellState = done === span ? 2 : busy ? 1 : 0;
    cells.push({ state: cellState, fill: done / span });
  }
  return { cells, bucket };
}

function base64ToBytes(b64) {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

async function refreshPieces(id) {
  const { ok, body } = await apiJson(`/api/v1/transfers/${id}/pieces`).catch(() => ({ ok: false }));
  const grid = document.getElementById("pieces-grid");
  const note = document.getElementById("pieces-bucket-note");
  if (!ok || body.chunk_count == null) {
    // `.pieces-grid` is a grid with a fixed 18 columns for the piece cells;
    // without this, a single <p> dropped in as its only child is forced into
    // one of those columns and wraps to a single word per line.
    grid.style.display = "block";
    grid.innerHTML = '<p class="muted">No piece data right now: this transfer is not currently running.</p>';
    note.hidden = true;
    return;
  }

  const bitmap = base64ToBytes(body.complete || "");
  const { cells, bucket } = cellsFor(body.chunk_count, bitmap, body.inflight || []);

  grid.style.display = "grid";
  grid.style.gridTemplateColumns = `repeat(${GRID_COLUMNS}, 1fr)`;
  grid.innerHTML = cells
    .map((cell) => {
      if (cell.state === 2) return '<div class="piece-cell piece-have"></div>';
      const pct = cell.state === 1 ? 35 + cell.fill * 50 : cell.fill * 100;
      if (pct <= 0) return '<div class="piece-cell"></div>';
      return `<div class="piece-cell piece-partial" style="background:color-mix(in srgb, var(--accent) ${pct}%, transparent)"></div>`;
    })
    .join("");

  if (bucket > 1) {
    note.hidden = false;
    note.textContent = `Each cell is ${bucket} pieces.`;
  } else {
    note.hidden = true;
  }
}

function startPiecesPolling(id) {
  stopPiecesPolling();
  refreshPieces(id);
  state.piecesTimer = setInterval(() => refreshPieces(id), 1000);
}

function stopPiecesPolling() {
  if (state.piecesTimer) {
    clearInterval(state.piecesTimer);
    state.piecesTimer = null;
  }
}

/* =============================================================== settings */

function wireSettingsForm() {
  document.getElementById("durability-segmented").addEventListener("click", (event) => {
    const button = event.target.closest("button[data-value]");
    if (!button) return;
    for (const b of event.currentTarget.querySelectorAll("button")) b.classList.toggle("active", b === button);
  });

  document.getElementById("settings-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    await saveSettings();
  });
}

function megabytesToBytes(text) {
  const value = Number(text);
  if (!text.trim() || !Number.isFinite(value) || value <= 0) return 0;
  return Math.round(value * 1_000_000);
}

function bytesToMegabytesText(bytes) {
  return bytes == null ? "" : String(bytes / 1_000_000);
}

async function loadSettings() {
  const { ok, body } = await apiJson("/api/v1/settings").catch(() => ({ ok: false }));
  if (!ok) return;

  document.getElementById("settings-download-limit").value = bytesToMegabytesText(body.download_limit);
  document.getElementById("settings-upload-limit").value = bytesToMegabytesText(body.upload_limit);
  document.getElementById("settings-download-dir").value = body.download_dir || "";
  document.getElementById("settings-connections").value = body.connections || "";

  for (const button of document.querySelectorAll("#durability-segmented button")) {
    button.classList.toggle("active", button.dataset.value === body.durability);
  }
  const durabilityNotes = {
    safe: "Flushes on every completed chunk and survives a power cut at the cost of speed.",
    balanced: "Flushes to disk every 5 seconds or 64 MB. A crash costs at most that much re-downloading.",
    fast: "Leaves flushing to the operating system. A power cut can cost the whole transfer.",
  };
  document.getElementById("durability-note").textContent = durabilityNotes[body.durability] || "";

  state.settingsInterfaces = body.interfaces || [];
  const list = document.getElementById("interface-limits-list");
  const emptyNote = document.getElementById("interface-limits-empty");
  emptyNote.hidden = state.settingsInterfaces.length > 0;
  list.innerHTML = state.settingsInterfaces
    .map(
      (iface) => `
      <div class="interface-limit-row" data-interface="${escapeHtml(iface.name)}">
        <span class="interface-name">${escapeHtml(iface.name)}${iface.up ? "" : " (down)"}</span>
        <label class="unit-field">
          <input type="number" min="0" step="1" placeholder="Unlimited"
            value="${bytesToMegabytesText((body.interface_limits || {})[iface.name])}">
          <span class="unit">MB/s</span>
        </label>
      </div>`,
    )
    .join("");
}

async function saveSettings() {
  const status = document.getElementById("settings-status");
  status.textContent = "Saving…";

  const durabilityButton = document.querySelector("#durability-segmented button.active");
  const interfaceLimits = {};
  for (const row of document.querySelectorAll("#interface-limits-list [data-interface]")) {
    const name = row.dataset.interface;
    const input = row.querySelector("input");
    interfaceLimits[name] = megabytesToBytes(input.value);
  }

  const patch = {
    download_limit: megabytesToBytes(document.getElementById("settings-download-limit").value),
    upload_limit: megabytesToBytes(document.getElementById("settings-upload-limit").value),
    interface_limits: interfaceLimits,
    durability: durabilityButton ? durabilityButton.dataset.value : "balanced",
    download_dir: document.getElementById("settings-download-dir").value.trim(),
    connections: Number(document.getElementById("settings-connections").value) || 1,
  };

  const { ok, body } = await apiJson("/api/v1/settings", {
    method: "POST",
    body: JSON.stringify(patch),
  }).catch((err) => ({ ok: false, body: { error: String(err) } }));

  if (ok) {
    status.textContent = "Saved.";
    setTimeout(() => {
      if (status.textContent === "Saved.") status.textContent = "";
    }, 2000);
  } else {
    status.textContent = body.error || "Could not save settings.";
  }
}

/* =================================================================== go */

boot();
