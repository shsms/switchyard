// Phase-1 SPA. Renders /api/topology with vis-network, and on node
// selection shows category-appropriate live charts in the floating
// inspector.
// Visual editing (add / connect / rename / delete) + REPL + Persist
// + Defaults / Scenarios all hang off the same /api/eval mutation
// path so anything done in the UI is also scriptable from outside.

import {
  clearSide,
  evalQuoted,
  refitCharts,
  showComponent,
  startScenarioReportLoop,
} from "./inspect.js";
import { backfillLogs, openWebSocket, setupRepl } from "./repl.js";
import { dashboardTiles } from "./dashboard.js";
import { clockState, pulseBar } from "./chrome.js";
import { setupFormulaTileClicks } from "./formulas.js";
import { scenariosPanel } from "./panels.js";
import {
  jumpToTopology,
  mgPath,
  navigateTo,
  refreshTopology,
  selectMicrogrid,
  setupDensityToggle,
  setupModeToggle,
  setupReplMgChip,
} from "./routing.js";
import { setupDrawerSplitter } from "./splitter.js";
import { topology } from "./topology.js";

// Re-export the routing helpers that other modules still pull
// via `./app.js` so consumers (dashboard / formulas / panels /
// chrome) keep working without rewiring every import site.
export {
  jumpToTopology,
  mgPath,
  navigateTo,
  refreshTopology,
  selectMicrogrid,
  setupDensityToggle,
};

const status = document.getElementById("status");
// `inspect` holds the inspector's swappable content; `inspector` is the
// floating card around it. The `inspector-open` class on <body> shows
// the card AND reserves it a grid column, so the canvas shrinks beside
// it rather than hiding under it. Set when something is selected (or a
// chrome panel is opened); cleared on deselect, Esc, the × button, or a
// tab switch — all via clearSide().
export const inspectEl = document.getElementById("inspect");
export const inspectorEl = document.getElementById("inspector");

// Open the floating inspector showing `panel` — "node" / "formula" or a
// chrome toggle's button id. The matching chrome toggle (Defaults /
// Report) lights up, so its state tracks the actual panel instead of a
// private flag that a ×/tab-switch close would leave stale.
export function openInspector(panel) {
  const wasOpen = document.body.classList.contains("inspector-open");
  document.body.classList.add("inspector-open");
  inspectorEl.dataset.panel = panel || "";
  for (const b of document.querySelectorAll("#defaults-btn, #scenario-report-btn")) {
    b.classList.toggle("primary", b.id === panel);
  }
  // The inspector column reserves space, so opening it shrinks the
  // canvas — reframe so nothing is clipped. Only on the closed→open
  // transition; a content swap (node→node) mustn't re-zoom the graph.
  if (!wasOpen) reflowAfterPanel();
}

// Re-fit the topology graph + any open uPlot charts after the inspector
// column appears or disappears and the panes resize. Deferred a frame so
// the grid reflow has settled before vis-network measures.
export function reflowAfterPanel() {
  requestAnimationFrame(() => {
    try {
      topology.fit();
    } catch (_) {
      /* network not built yet (no microgrid) — nothing to fit */
    }
    refitCharts();
  });
}

export function setStatus(text, klass) {
  status.textContent = text;
  status.className = `status ${klass || ""}`;
}

// Surface a transient toast in the bottom-right. Auto-dismisses after
// ~5s. Use this — not alert() — for action-failure feedback so the
// chrome stays unblocking when the server hiccups during, say, a WS
// reconnect storm. Three places fall outside this rule:
//   * `setStatus` for the persistent connection-state pill (top bar).
//   * `console.error` for diagnostics that only matter in the dev tools.
//   * confirm() prompts that genuinely need a synchronous yes/no.
export function notify(message, kind = "error") {
  let host = document.getElementById("toast-host");
  if (!host) {
    host = document.createElement("div");
    host.id = "toast-host";
    document.body.appendChild(host);
  }
  const t = document.createElement("div");
  t.className = `toast toast-${kind}`;
  t.textContent = message;
  host.appendChild(t);
  setTimeout(() => t.remove(), 5000);
}

// Single-source-of-truth for /api/overrides. Two consumers want
// this data (the chrome's count pill and the overrides dialog),
// both refresh on the same triggers (WS TopologyChanged, the
// dialog's delete actions). Centralising avoids fan-out fetches
// per WS tick and keeps everyone reading off one snapshot.
const overrideState = (() => {
  let snapshot = { persisted: [], count: 0 };
  const subs = new Set();
  let inflight = null;
  async function refresh() {
    if (inflight) return inflight;
    inflight = (async () => {
      try {
        const res = await fetch("/api/overrides");
        if (res.ok) {
          snapshot = await res.json();
          for (const fn of subs) fn(snapshot);
        }
      } catch (_) {
        // Best-effort — server unreachable just leaves the last
        // known snapshot in place so the chrome doesn't blank out.
      } finally {
        inflight = null;
      }
    })();
    return inflight;
  }
  return {
    get: () => snapshot,
    refresh,
    subscribe(fn) {
      subs.add(fn);
      fn(snapshot);
      return () => subs.delete(fn);
    },
  };
})();

// Map from a topology component to its public Lisp constructor.
// Inverters split on subtype ("battery" / "solar"); everything else
// keys off category. Returns null for categories we don't know how
// to clone (e.g. an unrecognised proto-derived kind).
function makeFnFor(c) {
  if (c.category === "inverter") {
    return c.subtype === "solar" ? "make-solar-inverter" : "make-battery-inverter";
  }
  return {
    grid: "make-grid-connection-point",
    meter: "make-meter",
    battery: "make-battery",
    "ev-charger": "make-ev-charger",
    chp: "make-chp",
  }[c.category] ?? null;
}

// Editor-style clipboard for copy / paste of node subgraphs. Holds a
// snapshot of the *structure* (categories, subtypes, edges between
// the captured nodes) — runtime state (SoC, setpoints) is not part
// of the snapshot, matching duplicate's structural-only semantics.
//
// Clipboard survives until replaced; paste can be repeated to drop
// multiple copies of the same subgraph. Cleared on hard reload (page
// refresh) since we don't persist it.
const clipboard = (() => {
  let buf = null; // { components: [{id, category, subtype}], edges: [[from,to]] }
  return {
    get: () => buf,
    isEmpty: () => buf == null || buf.components.length === 0,
    set(snapshot) {
      buf = snapshot;
    },
  };
})();

// Per-microgrid undo / redo stacks over the overrides file. Each
// canvas-driven mutation snapshots the file BEFORE the eval; Ctrl-Z
// restores the snapshot via POST /api/mg/{id}/overrides/text, which
// rewrites + reloads in one shot. Stacks are keyed by mg id so
// switching microgrids doesn't lose either side's history.
export const undoMgr = (() => {
  const undoStacks = new Map(); // mgId -> string[]
  const redoStacks = new Map(); // mgId -> string[]
  const MAX_DEPTH = 50;

  function stackFor(map, mgId) {
    let s = map.get(mgId);
    if (!s) { s = []; map.set(mgId, s); }
    return s;
  }

  async function fetchText(mgId) {
    const res = await fetch(`/api/mg/${mgId}/overrides/text`);
    if (!res.ok) throw new Error(`GET ${res.status}`);
    return await res.text();
  }

  async function postText(mgId, text) {
    const res = await fetch(`/api/mg/${mgId}/overrides/text`, {
      method: "POST",
      body: text,
    });
    if (!res.ok) {
      const body = await res.text();
      throw new Error(`POST ${res.status}: ${body}`);
    }
  }

  // Snapshot the overrides file before a mutation. Pushes onto the
  // current mg's undo stack and clears its redo stack (the standard
  // editor rule — a fresh edit invalidates the redo history).
  async function record() {
    const mgId = readSelectedMg();
    if (mgId == null) return;
    try {
      const snap = await fetchText(mgId);
      const u = stackFor(undoStacks, mgId);
      u.push(snap);
      while (u.length > MAX_DEPTH) u.shift();
      redoStacks.set(mgId, []);
    } catch (e) {
      console.warn("undoMgr.record failed:", e);
    }
  }

  async function undo() {
    const mgId = readSelectedMg();
    if (mgId == null) return;
    const u = stackFor(undoStacks, mgId);
    if (!u.length) {
      notify("Nothing to undo on this microgrid.");
      return;
    }
    let current;
    try { current = await fetchText(mgId); } catch (e) {
      notify(`Undo failed: ${e.message}`);
      return;
    }
    const target = u.pop();
    try {
      await postText(mgId, target);
    } catch (e) {
      u.push(target);
      notify(`Undo failed: ${e.message}`);
      return;
    }
    stackFor(redoStacks, mgId).push(current);
  }

  async function redo() {
    const mgId = readSelectedMg();
    if (mgId == null) return;
    const r = stackFor(redoStacks, mgId);
    if (!r.length) {
      notify("Nothing to redo on this microgrid.");
      return;
    }
    let current;
    try { current = await fetchText(mgId); } catch (e) {
      notify(`Redo failed: ${e.message}`);
      return;
    }
    const target = r.pop();
    try {
      await postText(mgId, target);
    } catch (e) {
      r.push(target);
      notify(`Redo failed: ${e.message}`);
      return;
    }
    stackFor(undoStacks, mgId).push(current);
  }

  return { record, undo, redo };
})();

function snapshotSelection(selectedIds) {
  const mainId = topology.mainMeterId();
  const components = selectedIds
    .map((id) => topology.get(id))
    .filter(Boolean)
    .map(({ id, category, subtype, hidden }) => ({
      id,
      category,
      subtype,
      hidden: !!hidden,
      main: id === mainId,
    }));
  if (!components.length) return null;
  const selected = new Set(selectedIds);
  const edges = topology
    .connections()
    .filter(([from, to]) => selected.has(from) && selected.has(to));
  return { components, edges };
}

function copySelection() {
  const ids = topology.selectedIds();
  if (!ids.length) {
    notify("Nothing selected to copy.");
    return false;
  }
  const snap = snapshotSelection(ids);
  if (!snap) return false;
  const unknown = snap.components.find((c) => makeFnFor(c) == null);
  if (unknown) {
    notify(`Don't know how to copy a "${unknown.category}".`);
    return false;
  }
  clipboard.set(snap);
  const n = snap.components.length;
  notify(`Copied ${n} component${n > 1 ? "s" : ""} to clipboard.`, "success");
  return true;
}

// Paste the clipboard subgraph as a fresh set of components + edges
// via one let*-bound eval. Matches duplicate's old behavior — uses
// the public make-* wrappers so per-category defaults apply, threads
// component-id to wire reconnects atomically. One pending log entry.
async function pasteClipboard() {
  if (clipboard.isEmpty()) {
    notify("Clipboard is empty — copy something first.");
    return;
  }
  const snap = clipboard.get();
  const bindings = snap.components
    .map((c) => {
      const flags = [];
      // make-meter's `:hidden t` and `:main t` only apply to meters,
      // but other categories ignore unknown kwargs gracefully — emit
      // when set so the snapshot round-trips. Sticky for cut+paste
      // and cross-mg copy+paste; same-mg copy+paste of an existing
      // `:main` meter will surface a "main meter already set" error
      // from make-meter, which is the expected guard.
      if (c.hidden) flags.push(":hidden t");
      if (c.main) flags.push(":main t");
      const args = flags.length ? ` ${flags.join(" ")}` : "";
      return `(m${c.id} (${makeFnFor(c)}${args}))`;
    })
    .join(" ");
  const reconnects = snap.edges
    .map(([from, to]) => `(connect m${from} m${to})`)
    .join(" ");
  const src = reconnects
    ? `(let* (${bindings}) ${reconnects})`
    : `(let* (${bindings}) t)`;
  await undoMgr.record();
  const res = await fetch(mgPath("eval"), { method: "POST", body: src });
  const data = await res.json();
  if (!data.ok) notify(`Paste failed: ${data.error}`);
}

async function deleteSelection() {
  const ids = topology.selectedIds();
  if (!ids.length) {
    notify("Nothing selected to delete.");
    return;
  }
  const removes = ids.map((id) => `(remove-component ${id})`).join(" ");
  const src = `(progn ${removes})`;
  await undoMgr.record();
  const res = await fetch(mgPath("eval"), { method: "POST", body: src });
  const data = await res.json();
  if (!data.ok) notify(`Delete failed: ${data.error}`);
}

async function cutSelection() {
  if (copySelection()) await deleteSelection();
}

function selectAllVisible() {
  const ids = topology.allIds();
  if (!ids.length) return;
  topology.select(ids);
  showComponent(topology.get(ids[0]));
}

// Floating right-click menu. Items are context-dependent: Copy +
// Delete (and Cut) when something's selected, Paste when nothing's
// selected and the clipboard has content. Hidden on outside click,
// Esc, or after running an action.
export function showContextMenu(x, y) {
  const menu = document.getElementById("ctx-menu");
  const sel = topology.selectedIds();
  const items = [];
  if (sel.length) {
    items.push({ label: "Copy", shortcut: "Ctrl/Cmd+C", action: copySelection });
    items.push({ label: "Cut", shortcut: "Ctrl/Cmd+X", action: cutSelection });
    items.push({ label: "Delete", shortcut: "Del", action: deleteSelection });
  } else if (!clipboard.isEmpty()) {
    items.push({ label: "Paste", shortcut: "Ctrl/Cmd+V", action: pasteClipboard });
  }
  if (!items.length) return; // nothing relevant; keep menu hidden
  menu.innerHTML = items
    .map(
      (it) =>
        `<button class="ctx-item" data-idx="${items.indexOf(it)}">
          <span>${it.label}</span><kbd>${it.shortcut}</kbd>
        </button>`,
    )
    .join("");
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  menu.hidden = false;
  // Clamp to viewport — menu has a fixed width so we can compare
  // after layout settles.
  requestAnimationFrame(() => {
    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) {
      menu.style.left = `${window.innerWidth - rect.width - 4}px`;
    }
    if (rect.bottom > window.innerHeight) {
      menu.style.top = `${window.innerHeight - rect.height - 4}px`;
    }
  });
  for (const btn of menu.querySelectorAll(".ctx-item")) {
    btn.addEventListener("click", () => {
      const idx = Number(btn.dataset.idx);
      hideContextMenu();
      items[idx].action();
    });
  }
}

function hideContextMenu() {
  const menu = document.getElementById("ctx-menu");
  if (menu) menu.hidden = true;
}

function setupContextMenu() {
  // Outside-click and Esc dismiss the menu. Capture phase so the
  // click that picked the menu item runs first.
  document.addEventListener("mousedown", (e) => {
    const menu = document.getElementById("ctx-menu");
    if (!menu.hidden && !menu.contains(e.target)) hideContextMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hideContextMenu();
  });
}

function setupAddForm() {
  const sel = document.getElementById("add-category");
  const btn = document.getElementById("add-btn");
  btn.addEventListener("click", async () => {
    const fn = sel.value;
    btn.disabled = true;
    try {
      await undoMgr.record();
      const res = await fetch(mgPath("eval"), {
        method: "POST",
        body: `(${fn})`,
      });
      const data = await res.json();
      if (!data.ok) notify(`Create failed: ${data.error}`);
    } finally {
      btn.disabled = false;
    }
  });
}

export function escapeHtml(s) {
  return String(s).replace(/[<>&]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;" })[c]);
}

async function showOverridesDialog() {
  const dlg = document.getElementById("pending-dialog");
  const content = document.getElementById("pending-dialog-content");
  // Subscribe to live updates so a bulk-delete from the toolbar
  // re-renders the list without each handler having to explicitly
  // call renderOverridesDialog. Unsubscribe on close to stop
  // pinging the host element after it's hidden.
  const unsubscribe = overrideState.subscribe((data) =>
    renderOverridesDialog(content, data),
  );
  dlg.addEventListener("close", () => unsubscribe(), { once: true });
  dlg.showModal();
  overrideState.refresh();
}

function renderOverridesDialog(content, data) {
  const persisted = data.persisted || [];
  if (!persisted.length) {
    content.innerHTML = '<p class="hint">no active overrides</p>';
    return;
  }
  const rows = persisted
    .map(
      (o) =>
        `<label class="pending-entry persisted">
          <input type="checkbox" class="ovr-check" data-idx="${o.idx}" />
          <div class="pending-num">#${o.idx + 1}</div>
          <pre>${escapeHtml(o.source)}</pre>
        </label>`,
    )
    .join("");
  content.innerHTML = `
    <div class="ovr-toolbar">
      <button class="hdr-btn" data-action="all">Select all</button>
      <button class="hdr-btn" data-action="none">Deselect all</button>
      <button class="hdr-btn" data-action="invert">Invert</button>
      <span class="spacer"></span>
      <button class="hdr-btn primary" data-action="delete" disabled>Delete selected</button>
    </div>
    <div class="ovr-rows">${rows}</div>
  `;
  const checks = () => content.querySelectorAll(".ovr-check");
  const deleteBtn = content.querySelector('[data-action="delete"]');
  function refreshDeleteState() {
    deleteBtn.disabled = ![...checks()].some((c) => c.checked);
  }
  for (const btn of content.querySelectorAll(".ovr-toolbar [data-action]")) {
    btn.addEventListener("click", async () => {
      const action = btn.dataset.action;
      if (action === "all") {
        for (const c of checks()) c.checked = true;
      } else if (action === "none") {
        for (const c of checks()) c.checked = false;
      } else if (action === "invert") {
        for (const c of checks()) c.checked = !c.checked;
      } else if (action === "delete") {
        const indices = [...checks()]
          .filter((c) => c.checked)
          .map((c) => Number(c.dataset.idx));
        if (!indices.length) return;
        const res = await fetch("/api/persisted/delete", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ indices }),
        });
        if (res.ok) {
          overrideState.refresh();
        } else {
          notify(`Delete failed: ${res.status} ${await res.text()}`);
        }
      }
      refreshDeleteState();
    });
  }
  for (const c of checks()) c.addEventListener("change", refreshDeleteState);
}

function setupHelpButton() {
  const dlg = document.getElementById("help-dialog");
  document
    .getElementById("help-btn")
    .addEventListener("click", () => dlg.showModal());
  document
    .getElementById("help-dialog-close")
    .addEventListener("click", () => dlg.close());
  // Click-outside-to-dismiss, mirroring the pending dialog.
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg) dlg.close();
  });
}

function setupSnapshotsDialog() {
  const dlg = document.getElementById("snapshots-dialog");
  const btn = document.getElementById("snapshots-btn");
  const close = document.getElementById("snapshots-dialog-close");
  const list = document.getElementById("snapshots-list");
  const input = document.getElementById("snapshot-name-input");
  const form = document.getElementById("snapshot-save-form");
  if (!dlg || !btn) return;

  async function refresh() {
    list.innerHTML = "";
    try {
      const res = await fetch("/api/snapshots");
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const names = (await res.json()).snapshots || [];
      if (names.length === 0) {
        list.innerHTML = '<li class="hint">No snapshots yet.</li>';
        return;
      }
      for (const name of names) {
        const li = document.createElement("li");
        li.className = "snapshot-row";
        li.innerHTML = `
          <span class="snapshot-name">${escapeHtml(name)}</span>
          <button class="hdr-btn snapshot-load" type="button">Load</button>
        `;
        li.querySelector(".snapshot-load").addEventListener("click", async () => {
          if (!confirm(`Load snapshot "${name}"? Current overrides will be replaced.`)) return;
          const r = await fetch("/api/snapshots/load", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name }),
          });
          if (!r.ok) {
            alert(`Load failed: ${await r.text()}`);
            return;
          }
          dlg.close();
        });
        list.appendChild(li);
      }
    } catch (err) {
      list.innerHTML = `<li class="hint">error: ${escapeHtml(err.message)}</li>`;
    }
  }

  btn.addEventListener("click", () => {
    refresh();
    dlg.showModal();
  });
  close.addEventListener("click", () => dlg.close());
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg) dlg.close();
  });
  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const name = input.value.trim();
    if (!name) return;
    const r = await fetch("/api/snapshots/save", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    if (!r.ok) {
      alert(`Save failed: ${await r.text()}`);
      return;
    }
    input.value = "";
    await refresh();
  });
}

function setupOverridesDialog() {
  const dlg = document.getElementById("pending-dialog");
  document
    .getElementById("pending-dialog-close")
    .addEventListener("click", () => dlg.close());
  // Click on the backdrop (target === dialog itself, not inner
  // card) closes the dialog. Keeps click-outside-to-dismiss working.
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg) dlg.close();
  });
}

function setupOverridesPill() {
  const pill = document.getElementById("pending-pill");
  pill.addEventListener("click", showOverridesDialog);
  const count = document.getElementById("pending-count");
  // Every successful eval write-throughs to the override file, so
  // the count is just the file's form total — no "unsaved" state
  // to track. Hidden when zero so the chrome stays clean on a
  // fresh checkout.
  overrideState.subscribe((data) => {
    const total = (data.persisted || []).length;
    count.textContent = total;
    pill.hidden = total === 0;
  });
}

/// Generic inspector toggle: a chrome button (Defaults / Report) that
/// opens the floating inspector with some custom render. Clicking it
/// again closes the inspector.
function makeSidePanelToggle(btnId, render) {
  const btn = document.getElementById(btnId);
  btn.addEventListener("click", async () => {
    // Clicking the lit button (its panel is the one showing) closes;
    // otherwise render this panel and open — even if the inspector is
    // already up showing something else, which just swaps the content.
    if (document.body.classList.contains("inspector-open") && inspectorEl.dataset.panel === btnId) {
      clearSide();
      return;
    }
    await render();
    openInspector(btnId);
  });
}

// Both side-panel toggles use the same chrome-button + swap-side-
// panel pattern. The render functions below own the actual content.
const setupDefaultsToggle = () => makeSidePanelToggle("defaults-btn", renderDefaults);
const setupScenarioReportToggle = () =>
  makeSidePanelToggle("scenario-report-btn", renderScenarioReport);

async function renderScenarioReport() {
  inspectEl.innerHTML = `
    <h2>Scenario report</h2>
    <p class="hint">Live aggregate metrics for the running scenario.
       Polls every 2 s while this panel is open.</p>
    <div id="sc-report-card"><span class="hint">loading…</span></div>
    <h3>Recent events</h3>
    <ul id="sc-report-events" class="sc-events"><li class="hint">—</li></ul>
  `;
  // Initial paint, then start polling. inspect.js owns the timer
  // handle so clearSide can cancel it from the inspect tear-down
  // path without reaching across module boundaries.
  await refreshScenarioReport();
  startScenarioReportLoop(setInterval(refreshScenarioReport, 2000));
}

async function refreshScenarioReport() {
  try {
    const [reportRes, eventsRes] = await Promise.all([
      fetch("/api/scenario/report"),
      fetch("/api/scenario/events?limit=50"),
    ]);
    if (!reportRes.ok || !eventsRes.ok) return;
    const r = await reportRes.json();
    const ev = await eventsRes.json();
    const card = document.getElementById("sc-report-card");
    if (card) card.innerHTML = renderScenarioCard(r);
    const list = document.getElementById("sc-report-events");
    if (list) list.innerHTML = renderScenarioEvents(ev.events);
  } catch (_e) {
    // Network blip; let the next tick try again. Don't tear down
    // the panel — the user can read the previous values until the
    // server is back.
  }
}

function renderScenarioCard(r) {
  const fmt = (v, unit = "W") =>
    v == null ? "—" : `${(v / 1000).toFixed(2)} k${unit}`;
  const soc = r.soc_stats
    ? `${r.soc_stats.mean_pct.toFixed(1)} % mean ·
       ${r.soc_stats.median_pct.toFixed(1)} % median ·
       ${r.soc_stats.mode_pct ?? "—"} % mode`
    : "—";
  const avgRows = r.main_meter_window_averages.length
    ? r.main_meter_window_averages
        .slice(-6)
        .map((w) => {
          const ts = new Date(w.window_start).toISOString().slice(11, 16);
          return `<tr><td>${ts}Z</td><td>${(w.avg_w / 1000).toFixed(2)} kW</td></tr>`;
        })
        .join("")
    : `<tr><td colspan="2" class="hint">no windows yet</td></tr>`;
  return `
    <dl class="sc-report-dl">
      <dt>elapsed</dt><dd>${r.scenario_elapsed_s.toFixed(1)} s</dd>
      <dt>main-meter peak</dt><dd>${fmt(r.peak_main_meter_w)}</dd>
      <dt>battery charge</dt><dd>${fmt(r.total_battery_charged_wh, "Wh")}</dd>
      <dt>battery discharge</dt><dd>${fmt(r.total_battery_discharged_wh, "Wh")}</dd>
      <dt>PV produced</dt><dd>${fmt(r.total_pv_produced_wh, "Wh")}</dd>
      <dt>battery SoC</dt><dd>${soc}</dd>
    </dl>
    <h3>15-min main-meter averages (last 6)</h3>
    <table class="sc-report-tbl">
      <thead><tr><th>window</th><th>avg</th></tr></thead>
      <tbody>${avgRows}</tbody>
    </table>
  `;
}

function renderScenarioEvents(events) {
  if (!events.length) {
    return '<li class="hint">no events yet</li>';
  }
  return events
    .slice(-20)
    .reverse()
    .map((e) => {
      const t = new Date(e.ts).toISOString().slice(11, 19);
      return `<li><code>${t}Z</code> <strong>${escapeHtml(e.kind)}</strong>
              ${escapeHtml(e.payload)}</li>`;
    })
    .join("");
}


async function renderDefaults() {
  const res = await fetch("/api/defaults");
  const data = await res.json();
  inspectEl.innerHTML = `
    <h2>Per-category defaults</h2>
    <p class="hint">
      Edit a value (raw Lisp) and click Save to <code>setq</code> the
      variable. Changes apply immediately and ride the pending log.
    </p>
    <div id="defaults-list"></div>
  `;
  const list = document.getElementById("defaults-list");
  for (const e of data.entries) {
    const block = document.createElement("div");
    block.className = "defaults-entry";
    block.innerHTML = `
      <label>${e.var_name}</label>
      <textarea rows="4" spellcheck="false">${escapeHtml(e.value)}</textarea>
      <button class="hdr-btn primary">Save</button>
    `;
    const ta = block.querySelector("textarea");
    block.querySelector("button").addEventListener("click", async () => {
      const expr = `(setq ${e.var_name} (quote ${ta.value}))`;
      await evalQuoted(expr);
    });
    list.appendChild(block);
  }
}

// Wire the floating panels' chrome: the inspector's × (close +
// deselect the node so a re-click reopens it), and the ＋ Add button /
// its panel's × (toggle the topology-only Add-component card).
function setupFloatingPanels() {
  document.getElementById("inspector-close").addEventListener("click", () => {
    clearSide();
    topology.select([]);
  });
  const addPanel = document.getElementById("add-panel");
  document
    .getElementById("add-toggle")
    .addEventListener("click", () => addPanel.classList.toggle("open"));
  document
    .getElementById("add-panel-close")
    .addEventListener("click", () => addPanel.classList.remove("open"));
}

// ─── Dashboard tiles ────────────────────────────────────────────────────────
//
// Aggregated metrics from the loopback Microgrid client flow into the
// Dashboard pane via two paths: (a) /api/microgrid/latest at mode-
// enter time so the tiles paint immediately with a real number, and
// (b) microgrid_sample WS frames for the per-second updates. Every
// tile selects its source via `data-stream="..."`; new tiles only
// have to declare the right stream name to participate.

// ─── Dispatches (per-microgrid) ─────────────────────────────────────────────
//
// Read-only table of the dispatches switchyard's dispatch API holds for
// the selected microgrid. Rendered on entering the Dispatches sub-tab
// and refetched when a `dispatch_changed` WS event names this microgrid
// (the dispatch CLI created / updated / deleted one).
export const dispatchesPanel = (() => {
  const host = () => document.getElementById("dispatches-body");
  // The microgrid currently shown — set by render(), read by the
  // create form + row-button handlers (which are wired once in setup).
  let currentMg = null;

  function fmtTs(ms) {
    if (ms == null) return "—";
    try {
      return new Date(ms).toLocaleString("en-GB", {
        year: "numeric",
        month: "short",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
        hour12: false,
        timeZone: clockState.tzInUse(),
      });
    } catch (_) {
      return new Date(ms).toISOString();
    }
  }

  function fmtDuration(s) {
    if (s == null) return "indefinite";
    if (s === 0) return "instant";
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    return (
      [h && `${h}h`, m && `${m}m`, sec && `${sec}s`].filter(Boolean).join(" ") ||
      "0s"
    );
  }

  function payloadText(p) {
    if (p == null) return "—";
    if (typeof p === "object" && !Array.isArray(p) && Object.keys(p).length === 0)
      return "—";
    return JSON.stringify(p);
  }

  function rowHtml(d) {
    const status = d.active
      ? '<span class="disp-badge disp-on">active</span>'
      : '<span class="disp-badge disp-off">inactive</span>';
    const dry = d.dry_run ? ' <span class="disp-badge disp-dry">dry-run</span>' : "";
    const payload = payloadText(d.payload);
    const payloadCell =
      payload === "—"
        ? "—"
        : `<code title="${escapeHtml(payload).replace(/"/g, "&quot;")}">${escapeHtml(
            payload.length > 60 ? payload.slice(0, 59) + "…" : payload,
          )}</code>`;
    const toggle = d.active ? "Pause" : "Resume";
    return `<tr>
      <td class="disp-id">#${d.id}</td>
      <td>${escapeHtml(d.type)}</td>
      <td>${status}${dry}</td>
      <td>${escapeHtml(fmtTs(d.start_ms))}</td>
      <td>${escapeHtml(fmtDuration(d.duration_s))}</td>
      <td>${escapeHtml(d.target)}</td>
      <td>${escapeHtml(d.recurrence || "once")}</td>
      <td class="disp-payload">${payloadCell}</td>
      <td class="disp-actions">
        <button class="link-btn" data-disp-toggle="${d.id}" data-next="${d.active ? 0 : 1}">${toggle}</button>
        <button class="link-btn disp-del" data-disp-del="${d.id}">Delete</button>
      </td>
    </tr>`;
  }

  function emptyHtml() {
    return `<p class="hint">No dispatches for this microgrid yet — create one with the form above, <code>swctl dispatch create</code>, or the dispatch CLI.</p>`;
  }

  // All mutations funnel through here; a non-2xx surfaces the server's
  // error text (the store's 400 / 404 messages) to the toast.
  async function mutate(method, path, body) {
    const opts = { method };
    if (body !== undefined) {
      opts.headers = { "content-type": "application/json" };
      opts.body = JSON.stringify(body);
    }
    const res = await fetch(path, opts);
    if (!res.ok) {
      const txt = await res.text().catch(() => "");
      throw new Error(txt || `HTTP ${res.status}`);
    }
    return res;
  }

  async function create(form) {
    if (currentMg == null) return;
    const fd = new FormData(form);
    const type = String(fd.get("type") || "").trim();
    const target = String(fd.get("target") || "").trim();
    if (!type || !target) {
      notify("type and target are required");
      return;
    }
    const body = {
      type,
      target,
      active: fd.get("active") === "on",
      dry_run: fd.get("dry_run") === "on",
    };
    const dur = String(fd.get("duration") || "").trim();
    if (dur !== "") {
      const n = Number(dur);
      if (!Number.isFinite(n) || n < 0) {
        notify("duration must be a non-negative number of seconds");
        return;
      }
      body.duration_s = Math.floor(n);
    }
    const payloadRaw = String(fd.get("payload") || "").trim();
    if (payloadRaw !== "") {
      try {
        body.payload = JSON.parse(payloadRaw);
      } catch (_) {
        notify("payload must be valid JSON");
        return;
      }
    }
    try {
      await mutate("POST", `/api/mg/${currentMg}/dispatches`, body);
      form.reset();
      notify("dispatch created", "info");
      render(currentMg);
    } catch (err) {
      notify(`create failed: ${err.message}`);
    }
  }

  async function setActive(id, active) {
    if (currentMg == null) return;
    try {
      await mutate("POST", `/api/mg/${currentMg}/dispatches/${id}/active`, {
        active,
      });
      render(currentMg);
    } catch (err) {
      notify(`${active ? "resume" : "pause"} failed: ${err.message}`);
    }
  }

  async function remove(id) {
    if (currentMg == null) return;
    if (!confirm(`Delete dispatch #${id}? This can't be undone.`)) return;
    try {
      await mutate("DELETE", `/api/mg/${currentMg}/dispatches/${id}`);
      render(currentMg);
    } catch (err) {
      notify(`delete failed: ${err.message}`);
    }
  }

  async function render(mgId) {
    currentMg = mgId;
    const el = host();
    if (!el) return;
    let list;
    try {
      const res = await fetch(`/api/mg/${mgId}/dispatches`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      list = await res.json();
    } catch (err) {
      el.innerHTML = `<p class="hint">dispatches unavailable: ${escapeHtml(
        err.message,
      )}</p>`;
      return;
    }
    if (!Array.isArray(list) || list.length === 0) {
      el.innerHTML = emptyHtml();
      return;
    }
    el.innerHTML = `<table class="disp-table">
      <thead><tr>
        <th>ID</th><th>Type</th><th>Status</th><th>Start</th>
        <th>Duration</th><th>Target</th><th>Recurs</th><th>Payload</th><th></th>
      </tr></thead>
      <tbody>${list.map(rowHtml).join("")}</tbody>
    </table>`;
  }

  // Wire the create form + row-action delegation once at startup. The
  // form and #dispatches-body are static in index.html, so the
  // listeners survive every render() (which only swaps innerHTML).
  function setup() {
    const form = document.getElementById("dispatch-create-form");
    if (form) {
      form.addEventListener("submit", (e) => {
        e.preventDefault();
        create(form);
      });
    }
    const body = host();
    if (body) {
      body.addEventListener("click", (e) => {
        const btn = e.target.closest("button");
        if (!btn) return;
        if (btn.dataset.dispToggle != null) {
          setActive(Number(btn.dataset.dispToggle), btn.dataset.next === "1");
        } else if (btn.dataset.dispDel != null) {
          remove(Number(btn.dataset.dispDel));
        }
      });
    }
  }

  return { render, setup };
})();

// ─── Grid frequency bridge ──────────────────────────────────────────────────

async function init() {
  setupAddForm();
  setupDefaultsToggle();
  setupScenarioReportToggle();
  setupFloatingPanels();
  setupDrawerSplitter();
  setupOverridesDialog();
  setupSnapshotsDialog();
  backfillLogs();
  setupOverridesPill();
  // The topology canvas calls back to showComponent / clearSide on
  // node click + canvas click (declared further down). Wire it up
  // before the first apply so the listeners are in place.
  topology.setSelectionHandler(showComponent, clearSide);
  // Editor-style keyboard shortcuts. All check that focus isn't in
  // a text editor (REPL textarea, dialog inputs) before firing, so
  // typing remains unaffected.
  document.addEventListener("keydown", (e) => {
    const inEditable = e.target.matches?.("input, textarea, [contenteditable]");
    if (inEditable) return;
    const meta = e.metaKey || e.ctrlKey;
    const key = e.key.toLowerCase();
    if (meta && e.shiftKey && key === "z") {
      e.preventDefault();
      undoMgr.redo();
    } else if (meta && key === "z") {
      e.preventDefault();
      undoMgr.undo();
    } else if (meta && key === "y") {
      // Common Windows-style redo alias.
      e.preventDefault();
      undoMgr.redo();
    } else if (meta && key === "c") {
      e.preventDefault();
      copySelection();
    } else if (meta && key === "v") {
      e.preventDefault();
      pasteClipboard();
    } else if (meta && key === "x") {
      e.preventDefault();
      cutSelection();
    } else if (meta && key === "a") {
      e.preventDefault();
      selectAllVisible();
    } else if (e.key === "Delete" || e.key === "Backspace") {
      e.preventDefault();
      deleteSelection();
    } else if (e.key === "Escape") {
      // Topology's own click handler closes the inspector on deselect;
      // mirror that here for keyboard parity.
      topology.select([]);
      clearSide();
    }
  });
  setupContextMenu();
  setupHelpButton();
  setupModeToggle();
  setupReplMgChip();
  setupFormulaTileClicks();
  scenariosPanel.setup();
  dispatchesPanel.setup();
  await clockState.init();
  pulseBar.setup();
  await refreshTopology();
  await overrideState.refresh();
  // WS push: refresh both the topology (so the canvas reflects the
  // mutation) and the pending state (so the pill, dialog, and
  // inspector all update) on every TopologyChanged. Sample events
  // go straight into the live-charts router.
  openWebSocket((_v) => {
    refreshTopology();
    overrideState.refresh();
    // The loopback supervisor debounces ~300ms and rebuilds the
    // Microgrid handle; /api/microgrid/latest + /formulas return
    // 503 mid-rebuild. Delay the dashboard re-fetch so it lands
    // after the supervisor settles. backfill() is 503-tolerant —
    // an undershoot leaves the existing tooltip + values, and
    // the next sample-flow tick overwrites the displayed numbers.
    setTimeout(() => dashboardTiles.backfill(), 800);
  });
  setupRepl();
}

init();
