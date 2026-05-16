// Phase-1 SPA. Renders /api/topology with vis-network, and on node
// selection shows category-appropriate live charts in the floating
// inspector.
// Visual editing (add / connect / rename / delete) + REPL + Persist
// + Defaults / Scenarios all hang off the same /api/eval mutation
// path so anything done in the UI is also scriptable from outside.

import {
  copySelection,
  cutSelection,
  deleteSelection,
  pasteClipboard,
  selectAllVisible,
  setupAddForm,
  setupContextMenu,
  undoMgr,
} from "./editor.js";
import { clearSide, refitCharts, showComponent } from "./inspect.js";
import {
  overrideState,
  setupDefaultsToggle,
  setupHelpButton,
  setupOverridesDialog,
  setupOverridesPill,
  setupScenarioReportToggle,
  setupSnapshotsDialog,
} from "./dialogs.js";
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
export function escapeHtml(s) {
  return String(s).replace(/[<>&]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;" })[c]);
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
