// Phase-1 SPA. Renders /api/topology with vis-network, and on node
// selection shows category-appropriate live charts in the floating
// inspector.
// Visual editing (add / connect / rename / delete) + REPL + Persist
// + Defaults / Scenarios all hang off the same /api/eval mutation
// path so anything done in the UI is also scriptable from outside.

import {
  COMPLETIONS,
  indentForNewline,
  rainbowHighlight,
  wordAtCursor,
} from "./repl-syntax.js";
import {
  batteryPairs,
  chpRows,
  dashboardTiles,
  evRows,
  pvRows,
} from "./dashboard.js";
import { clockState, gridFrequency, pulseBar } from "./chrome.js";
import { microgridsPanel, scenariosPanel } from "./panels.js";
import { topology } from "./topology.js";

const status = document.getElementById("status");
// `inspect` holds the inspector's swappable content; `inspector` is the
// floating card around it. The `inspector-open` class on <body> shows
// the card AND reserves it a grid column, so the canvas shrinks beside
// it rather than hiding under it. Set when something is selected (or a
// chrome panel is opened); cleared on deselect, Esc, the × button, or a
// tab switch — all via clearSide().
const inspectEl = document.getElementById("inspect");
const inspectorEl = document.getElementById("inspector");

// Open the floating inspector showing `panel` — "node" / "formula" or a
// chrome toggle's button id. The matching chrome toggle (Defaults /
// Report) lights up, so its state tracks the actual panel instead of a
// private flag that a ×/tab-switch close would leave stale.
function openInspector(panel) {
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
function reflowAfterPanel() {
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

// One uPlot per metric — multi-series (e.g. P + bound envelope on one
// chart) lands when we tackle the merge-by-shared-timestamp problem.
const CHARTS_BY_CATEGORY = {
  grid: ["frequency_hz"],
  meter: ["active_power_w", "reactive_power_var"],
  inverter: ["active_power_w", "reactive_power_var"],
  battery: ["soc_pct", "dc_power_w"],
  "ev-charger": ["soc_pct", "dc_power_w"],
  chp: ["active_power_w"],
};

// Display-only labels per metric. Scaling, units, and the
// "is this a power-family quantity?" decision now come off the
// /api/history response's `quantity` + `unit` fields — see
// chooseScale below. Anything not in this table falls back to the
// raw metric name as the chart title.
const METRIC_TITLES = {
  active_power_w:     "Active Power",
  reactive_power_var: "Reactive Power",
  frequency_hz:       "Frequency",
  soc_pct:            "SoC",
  dc_power_w:         "DC Power",
};

// Pick a display scale from a typed quantity + base unit. Power-
// family quantities autoscale W → kW → MW based on the data range;
// everything else uses the base unit verbatim. The `quantity` /
// `unit` arguments mirror the `Sample<Q>` / `Q.base_unit()` shape
// upstream in frequenz-microgrid, so the same code can serve any
// `Power` / `ReactivePower` / `Frequency` / `Percentage` payload.
function chooseScale(quantity, unit, values) {
  const isPower = quantity === "Power" || quantity === "ReactivePower";
  if (isPower && values.length) {
    const max = Math.max(...values.map((v) => Math.abs(v)));
    if (max >= 1e6) return { div: 1e6, unit: `M${unit}` };
    if (max >= 1e3) return { div: 1e3, unit: `k${unit}` };
    return { div: 1, unit };
  }
  return { div: 1, unit: unit || "" };
}

// Live-chart state for whichever component the user has selected.
// Replaced wholesale on every selection change; the previous uPlots
// get destroyed in clear(). All access to the per-selection chart
// session goes through this module so the surrounding code never
// has to spell out the "is the right component selected, has the
// metric been wired" preconditions for the live push paths.
const liveCharts = (() => {
  let active = null; // { id, charts: Map<metric, {plot, xs, ys, scale}> }
  return {
    set(id, charts) {
      active = { id, charts };
    },
    clear() {
      if (!active) return;
      for (const ch of active.charts.values()) ch.plot.destroy();
      active = null;
    },
    pushSample(id, metric, ts_ms, value) {
      if (!active || active.id !== Number(id)) return;
      const series = active.charts.get(metric);
      if (!series) return;
      series.xs.push(ts_ms / 1000);
      // Apply the chart's chosen unit scale so live samples stay
      // consistent with the backfilled ones.
      series.ys.push(value / series.scale.div);
      // Cap to 5-minute window so the chart doesn't grow forever.
      const cutoff = Date.now() / 1000 - 300;
      while (series.xs.length && series.xs[0] < cutoff) {
        series.xs.shift();
        series.ys.shift();
      }
      series.plot.setData([series.xs, series.ys]);
    },
    pushSetpoint(ev) {
      // Only render if the event is for the currently-inspected
      // component; otherwise it'll be picked up next time the user
      // selects that node (the server's per-component log is the
      // source of truth).
      if (!active || active.id !== Number(ev.id)) return;
      const list = inspectEl.querySelector(".sp-list");
      if (!list) return;
      // Drop the "none" placeholder if it's still showing.
      const empty = list.querySelector(".sp-empty");
      if (empty) empty.remove();
      const li = document.createElement("li");
      li.className = `sp-event ${ev.accepted ? "accepted" : "rejected"}`;
      const ts = new Date(ev.ts_ms).toLocaleTimeString();
      // The WS event carries the setpoint kind on `setpoint_kind`
      // to dodge collision with the SiteEvent discriminator (also
      // called `kind`). Escape every interpolation — the server
      // currently only emits fixed-shape strings and a numeric
      // `value`, but defense-in-depth (anything that lands in
      // `innerHTML` goes through `escapeHtml` first).
      const tag = escapeHtml(String(ev.setpoint_kind ?? "").replace("_", " "));
      const head = `<span class="sp-ts">${escapeHtml(ts)}</span> <span class="sp-tag">${tag}</span> <span class="sp-val">${escapeHtml(String(ev.value))}</span>`;
      const body = ev.accepted
        ? '<span class="sp-ok">✓ accepted</span>'
        : `<span class="sp-bad">✕ ${escapeHtml(ev.reason || "")}</span>`;
      li.innerHTML = `${head}<br/>${body}`;
      list.prepend(li);
      // Trim if the list is getting long — match the 600s window
      // used by the initial fetch.
      while (list.children.length > 100) list.removeChild(list.lastChild);
    },
    refit() {
      if (!active) return;
      for (const series of active.charts.values()) {
        const parent = series.plot.root.parentElement;
        if (!parent) continue;
        series.plot.setSize({
          width: parent.clientWidth,
          height: 140,
        });
      }
    },
  };
})();

// Brighten a #rrggbb hex by `n` per channel (clamped to 255). Used to
// derive hover + selected node-background tints from the canonical
// category color so the same node visibly reacts to interaction
// without us hand-picking a separate palette per state.

// Categories that the gRPC server actually accepts setpoints on.
// command-mode (timeout / error fault simulation) only makes sense
// for these — grids and meters have no setpoint surface, so we hide
// the dropdown rather than offering a knob that does nothing.
const ACCEPTS_SETPOINTS = new Set(["battery", "inverter", "ev-charger", "chp"]);

// Per-category runtime knobs the inspector exposes as numeric
// inputs. Each one binds to an existing Lisp setter — so this is
// just UI sugar over what the REPL could already do. Construction-
// time args (capacity, rated bounds, …) aren't here because most
// aren't runtime-mutable on the underlying component yet.
// `dynamic: true` knobs accept either a numeric literal or a Lisp
// expression (lambda, quoted symbol, …) — the underlying defun
// dispatches on input kind. Inputs with `dynamic` render as text,
// everything else as numeric. See the renderInspect Knobs block.
const KNOBS_BY_CATEGORY = {
  meter: [
    { label: "power (W or expr)", defun: "set-meter-power", dynamic: true },
  ],
  inverter: [
    { label: "reactive PF limit", defun: "set-reactive-pf-limit" },
    { label: "reactive apparent (VA)", defun: "set-reactive-apparent-va" },
  ],
};

function knobsFor(d) {
  const knobs = [...(KNOBS_BY_CATEGORY[d.category] || [])];
  // Solar inverters also get a sunlight knob — driven by the same
  // (set-solar-sunlight ID PCT) defun the cloud-curve timer uses.
  if (d.category === "inverter" && d.subtype === "solar") {
    knobs.unshift({
      label: "sunlight (% or expr)",
      defun: "set-solar-sunlight",
      dynamic: true,
    });
  }
  return knobs;
}

function renderInspect(d, parentIds, childIds) {
  const renderEdgeRow = (id, dataAttr) => {
    const c = topology.get(id);
    const label = c ? c.name : `id ${id}`;
    return `<li>${escapeHtml(label)} <button class="link-btn" ${dataAttr}="${id}">✕</button></li>`;
  };
  const parentList = parentIds.length
    ? parentIds.map((id) => renderEdgeRow(id, "data-disconnect-from")).join("")
    : '<li class="hint">none</li>';
  const childList = childIds.length
    ? childIds.map((id) => renderEdgeRow(id, "data-disconnect-to")).join("")
    : '<li class="hint">none</li>';

  inspectEl.innerHTML = `
    <h2><input id="rename" class="name-input" value="${escapeHtml(d.name)}" /></h2>
    <dl>
      <dt>id</dt><dd>${d.id}</dd>
      <dt>category</dt><dd>${d.category}</dd>
      <dt>subtype</dt><dd>${d.subtype || "—"}</dd>
    </dl>
    <h3>Runtime</h3>
    <dl>
      <dt>health</dt><dd>${selectField("health", d.health, ["ok", "error", "standby"])}</dd>
      <dt>telemetry</dt><dd>${selectField("telemetry-mode", d.telemetry_mode, ["normal", "silent", "closed"])}</dd>
      ${ACCEPTS_SETPOINTS.has(d.category)
        ? `<dt>commands</dt><dd>${selectField("command-mode", d.command_mode, ["normal", "timeout", "error"])}</dd>`
        : ""}
    </dl>
    ${(() => {
      const knobs = knobsFor(d);
      if (!knobs.length) return "";
      return `<h3>Knobs</h3><dl>${knobs
        .map((k) => {
          const inputAttrs = k.dynamic
            ? `type="text" placeholder="value or (lambda () ...)"`
            : `type="number" step="any" placeholder="value"`;
          return `<dt>${escapeHtml(k.label)}</dt><dd>
            <input ${inputAttrs} class="knob-input"
                   data-defun="${k.defun}" />
          </dd>`;
        })
        .join("")}</dl>`;
    })()}
    <h3>Connections</h3>
    <div class="conns">
      <div><strong>parents</strong><ul>${parentList}</ul></div>
      <div><strong>children</strong><ul>${childList}</ul></div>
    </div>
    <div id="charts"></div>
  `;

  // Wire form callbacks. Every action POSTs to /api/eval; the WS
  // TopologyChanged refresh re-reads the form state from the server
  // and re-renders this panel automatically.
  document.getElementById("rename").addEventListener("change", (e) => {
    const name = e.target.value.trim();
    if (!name) return;
    evalQuoted(`(rename-component ${d.id} "${jsToLispString(name)}")`);
  });
  for (const [key, defun] of [
    ["health", "set-component-health"],
    ["telemetry-mode", "set-component-telemetry-mode"],
    ["command-mode", "set-component-command-mode"],
  ]) {
    const sel = inspectEl.querySelector(`select[data-knob="${key}"]`);
    if (!sel) continue; // dropdown hidden for this category
    sel.addEventListener("change", (e) => {
      evalQuoted(`(${defun} ${d.id} '${e.target.value})`);
    });
  }
  // Numeric knob inputs: change (or Enter then blur) → eval the
  // setter with the typed value; then clear so the field reads as
  // "what would you set it to next" rather than "what's it set to
  // now" (we don't have getters for most of these, and stale values
  // would mislead).
  for (const inp of inspectEl.querySelectorAll(".knob-input")) {
    inp.addEventListener("change", (e) => {
      const v = e.target.value.trim();
      if (v === "") return;
      evalQuoted(`(${e.target.dataset.defun} ${d.id} ${v})`);
      e.target.value = "";
    });
  }
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-from]")) {
    btn.addEventListener("click", () =>
      evalQuoted(`(disconnect ${btn.dataset.disconnectFrom} ${d.id})`),
    );
  }
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-to]")) {
    btn.addEventListener("click", () =>
      evalQuoted(`(disconnect ${d.id} ${btn.dataset.disconnectTo})`),
    );
  }
}

function selectField(knob, current, options) {
  const opts = options
    .map(
      (o) => `<option value="${o}"${o === current ? " selected" : ""}>${o}</option>`,
    )
    .join("");
  return `<select data-knob="${knob}">${opts}</select>`;
}

function jsToLispString(s) {
  return s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

async function evalQuoted(expr) {
  await undoMgr.record();
  const res = await fetch(mgPath("eval"), { method: "POST", body: expr });
  // res.json() can throw "JSON.parse: unexpected character" if the
  // server returned an empty / non-JSON body (e.g. a 5xx with HTML
  // error page, or a connection that died mid-response). Surface the
  // raw text so the actual culprit shows up in the console instead
  // of an opaque parse error.
  const text = await res.text();
  let data;
  try {
    data = JSON.parse(text);
  } catch (_e) {
    console.error(`evalQuoted: bad JSON for ${expr.slice(0, 60)}…`, {
      status: res.status,
      body: text,
    });
    notify(`${expr}: server returned non-JSON (HTTP ${res.status})`);
    return;
  }
  if (!data.ok) notify(`${expr}: ${data.error}`);
}

async function showComponent(d) {
  if (!d) return;
  openInspector("node");
  liveCharts.clear();

  // vis-network's getConnectedNodes(id, direction) returns the
  // ids on either side of the selected node — cheaper than walking
  // /api/topology for the disconnect buttons. Display labels get
  // resolved by renderInspect via topology.get().
  const parentIds = topology.parentsOf(d.id);
  const childIds = topology.childrenOf(d.id);
  renderInspect(d, parentIds, childIds);

  const metrics = CHARTS_BY_CATEGORY[d.category] || [];
  const container = document.getElementById("charts");
  const charts = new Map(); // metric → { plot, xs, ys }

  for (const metric of metrics) {
    const slot = document.createElement("div");
    slot.className = "chart";
    container.appendChild(slot);
    const url = `${mgPath("history")}?id=${d.id}&metric=${metric}&window_s=300`;
    const resp = await (await fetch(url)).json();
    const samples = resp.samples || [];
    const xs = samples.map(([t]) => t / 1000);
    const ys = samples.map(([, v]) => v);
    const { plot, scale } = makePlot(slot, metric, resp.quantity, resp.unit, xs, ys);
    // Stored ys are pre-scaled (already divided by scale.div) so the
    // live push path can append by dividing each new sample once.
    charts.set(metric, { plot, xs, ys: ys.map((y) => y / scale.div), scale });
  }
  liveCharts.set(d.id, charts);

  // Setpoint events: list recent control-app requests + outcome
  // below the charts. Live-overlay markers on the chart are a
  // follow-up; this is the inspector's MVP.
  await renderSetpoints(d.id, container);
}

async function renderSetpoints(id, container) {
  const wrap = document.createElement("div");
  wrap.className = "setpoints";
  wrap.innerHTML = "<h3>Recent setpoints</h3>";
  container.appendChild(wrap);
  try {
    const res = await fetch(`/api/setpoints?id=${id}&window_s=600`);
    const data = await res.json();
    // Always create the list element, even when empty — pushSetpoint
    // appends to it on incoming WS events. A no-events placeholder
    // hint sits inside the list and gets dropped once the first
    // event lands.
    const list = document.createElement("ol");
    list.className = "sp-list";
    if (!data.events.length) {
      const empty = document.createElement("li");
      empty.className = "hint sp-empty";
      empty.textContent = "none in the last 10 min";
      list.appendChild(empty);
    }
    // Newest first reads better in a chronological log.
    for (const e of data.events.slice().reverse()) {
      const li = document.createElement("li");
      const accepted = e.outcome.kind === "accepted";
      li.className = `sp-event ${accepted ? "accepted" : "rejected"}`;
      const ts = new Date(e.ts).toLocaleTimeString();
      // Same escape discipline as the live-WS counterpart above.
      const tag = escapeHtml(String(e.kind ?? "").replace("_", " "));
      const head = `<span class="sp-ts">${escapeHtml(ts)}</span> <span class="sp-tag">${tag}</span> <span class="sp-val">${escapeHtml(String(e.value))}</span>`;
      const body = accepted
        ? '<span class="sp-ok">✓ accepted</span>'
        : `<span class="sp-bad">✕ ${escapeHtml(e.outcome.reason)}</span>`;
      li.innerHTML = `${head}<br/>${body}`;
      list.appendChild(li);
    }
    wrap.appendChild(list);
  } catch (err) {
    wrap.insertAdjacentHTML(
      "beforeend",
      `<p class="hint">setpoints unavailable: ${escapeHtml(err.message)}</p>`,
    );
  }
}

function makePlot(container, metric, quantity, unit, xs, ys) {
  const title = METRIC_TITLES[metric] || metric;
  const scale = chooseScale(quantity, unit, ys);
  const scaledYs = ys.map((y) => y / scale.div);
  const opts = {
    width: container.clientWidth || 280,
    height: 140,
    title: scale.unit ? `${title} (${scale.unit})` : title,
    cursor: { drag: { x: false, y: false } },
    legend: { show: false },
    scales: { x: { time: true } },
    axes: [
      { stroke: "#7d848e", grid: { stroke: "#353a45", width: 0.5 } },
      // size = pixels reserved for the y-axis labels. 60 fits values
      // up to 6 chars (e.g. -32.5 kW) without truncation.
      {
        stroke: "#7d848e",
        grid: { stroke: "#353a45", width: 0.5 },
        size: 60,
      },
    ],
    series: [
      {},
      { stroke: "#79b8ff", width: 1.5, points: { show: false } },
    ],
  };
  return { plot: new uPlot(opts, [xs, scaledYs], container), scale };
}

// Close the floating inspector: stop its live charts / report poll,
// reset its content, and hide the card. Named `clearSide` for the
// callers that predate the float (deselect handler, Esc, panel toggles).
function clearSide() {
  const wasOpen = document.body.classList.contains("inspector-open");
  liveCharts.clear();
  if (scenarioReportTimer != null) {
    clearInterval(scenarioReportTimer);
    scenarioReportTimer = null;
  }
  inspectEl.innerHTML =
    '<p class="hint">Click a node to inspect. Right-click for the context menu.</p>';
  document.body.classList.remove("inspector-open");
  delete inspectorEl.dataset.panel;
  for (const b of document.querySelectorAll("#defaults-btn, #scenario-report-btn")) {
    b.classList.remove("primary");
  }
  // Closing gives the column back to the canvas — refit it (and charts).
  if (wasOpen) reflowAfterPanel();
}

let scenarioReportTimer = null;

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
  // Initial paint, then start polling.
  await refreshScenarioReport();
  scenarioReportTimer = setInterval(refreshScenarioReport, 2000);
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

/// Generic drag-to-resize handler. The drawer splitter (between the
/// topology row and the bottom drawer) uses it: capture the
/// starting state on mousedown, compute a delta on mousemove,
/// hand it back to the caller as a clamped px value, refit any
/// open uPlot charts on every frame so they keep up with the
/// container width.
///
///   axis: "x" | "y"             which mouse coord to track
///   splitter: HTMLElement       drag handle
///   getStart(): number          current size we're modifying
///   apply(value: number): void  write the new size somewhere
///   clamp(value, viewportSize): clamp to a sensible range
function makeSplitter({ axis, splitter, getStart, apply, clamp }) {
  const isHoriz = axis === "y";
  const cursor = isHoriz ? "row-resize" : "col-resize";

  let dragging = false;
  let start = 0;
  let startSize = 0;

  splitter.addEventListener("mousedown", (e) => {
    dragging = true;
    start = isHoriz ? e.clientY : e.clientX;
    startSize = getStart();
    splitter.classList.add("dragging");
    document.body.style.cursor = cursor;
    document.body.style.userSelect = "none";
    e.preventDefault();
  });
  document.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    const here = isHoriz ? e.clientY : e.clientX;
    const delta = start - here; // positive = drag toward the start
    const viewport = isHoriz ? window.innerHeight : window.innerWidth;
    apply(clamp(startSize + delta, viewport));
    refitCharts();
  });
  document.addEventListener("mouseup", () => {
    if (!dragging) return;
    dragging = false;
    splitter.classList.remove("dragging");
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  });
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

/// Horizontal splitter between topology row and bottom drawer.
/// Updates main's grid-template-rows to resize the drawer.
function setupDrawerSplitter() {
  const main = document.getElementById("app");
  const drawer = document.getElementById("repl");
  const MIN_DRAWER = 120;
  const MIN_TOP_FRAC = 0.2; // keep at least 20% of main for the canvas
  makeSplitter({
    axis: "y",
    splitter: document.getElementById("drawer-splitter"),
    getStart: () => drawer.getBoundingClientRect().height,
    apply: (h) => {
      // Main's grid template has FOUR rows: the auto mgheader, the
      // 1fr topology row, the 5px drawer-splitter, the drawer.
      // An earlier shape rewrote only three values here, dropping
      // the mgheader's `auto` track — the grid then collapsed
      // and the canvas disappeared as soon as the user dragged the
      // splitter at all. Keep all four tracks.
      main.style.gridTemplateRows = `auto 1fr 5px ${h}px`;
    },
    clamp: (h, vh) => {
      const mainH = main.getBoundingClientRect().height;
      // mainH excludes the header; we use it (not vh) for the upper
      // clamp so the canvas stays at MIN_TOP_FRAC of the drawer's
      // own container.
      void vh;
      return Math.max(MIN_DRAWER, Math.min(mainH * (1 - MIN_TOP_FRAC), h));
    },
  });
}

const refitCharts = () => liveCharts.refit();

// Log panel above the REPL. /api/logs gives the load-time backfill
// (ring of recent records); /ws/events kind:"log" appends each new
// record live. Capped at 500 DOM rows so a chatty session doesn't
// freeze the panel.
function appendLog(ev) {
  const box = document.getElementById("logs");
  const el = document.createElement("div");
  el.className = `log-line ${(ev.level || "info").toLowerCase()}`;
  const ts = new Date(ev.ts_ms).toLocaleTimeString();
  el.innerHTML =
    `<span class="log-ts">${ts}</span>` +
    `<span class="log-lvl">${escapeHtml(ev.level || "")}</span>` +
    `<span class="log-msg">${escapeHtml(ev.message || "")}</span>`;
  // Scroll-pin: only auto-scroll if the user hadn't scrolled away.
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 30;
  box.appendChild(el);
  while (box.children.length > 500) box.removeChild(box.firstChild);
  if (atBottom) box.scrollTop = box.scrollHeight;
}

async function backfillLogs() {
  try {
    const lines = await (await fetch("/api/logs")).json();
    for (const ln of lines) appendLog(ln);
  } catch (_) {}
}

// Hardcoded completion candidates for the REPL. Until tulisp exposes
// obarray enumeration upstream, this list has to track the surface
// switchyard exposes by hand. Drop-in replacement: hit /api/symbols
// (TBD) and merge the response into this array.

function setupRepl() {
  const form = document.getElementById("repl-form");
  const input = document.getElementById("repl-input");
  const overlay = document.getElementById("repl-input-overlay");
  const output = document.getElementById("repl-output");
  const completions = document.getElementById("repl-completions");
  let selectedIdx = 0;
  let active = []; // current list of candidates

  // Electric-pair: typed open chars insert their close + leave the
  // cursor between. Closing char typed when the next char is the
  // same close just steps over instead of doubling up. Backspace
  // immediately after an empty pair eats both halves.
  const PAIRS = { "(": ")", "[": "]", "{": "}", "\"": "\"" };
  const CLOSES = new Set(Object.values(PAIRS));

  function refreshOverlay() {
    overlay.innerHTML = rainbowHighlight(input.value);
    overlay.scrollTop = input.scrollTop;
  }

  function renderCompletions() {
    if (!active.length) {
      completions.hidden = true;
      completions.innerHTML = "";
      return;
    }
    completions.hidden = false;
    completions.innerHTML = active
      .map(
        (c, i) =>
          `<li class="${i === selectedIdx ? "selected" : ""}" data-i="${i}">${escapeHtml(c)}</li>`,
      )
      .join("");
    for (const li of completions.querySelectorAll("li")) {
      li.addEventListener("mousedown", (e) => {
        e.preventDefault(); // don't blur the textarea
        selectedIdx = Number(li.dataset.i);
        applyCompletion();
      });
    }
  }

  function refresh() {
    const { prefix } = wordAtCursor(input);
    if (!prefix || prefix.length < 1) {
      active = [];
    } else {
      active = COMPLETIONS.filter((c) => c.startsWith(prefix)).slice(0, 12);
      // If the only match is exactly what's typed, no point showing a popup.
      if (active.length === 1 && active[0] === prefix) active = [];
    }
    selectedIdx = 0;
    renderCompletions();
  }

  function applyCompletion() {
    if (!active.length) return;
    const choice = active[selectedIdx];
    const { start, end } = wordAtCursor(input);
    const v = input.value;
    input.value = v.slice(0, start) + choice + v.slice(end);
    const newCursor = start + choice.length;
    input.setSelectionRange(newCursor, newCursor);
    active = [];
    renderCompletions();
    // Programmatic .value assignment doesn't fire `input`; nudge
    // the overlay (and other input listeners) explicitly.
    refreshOverlay();
  }

  // Send the current textarea contents through /api/format and
  // replace them with the result. Cursor preservation is best-
  // effort: we count non-whitespace characters before the old
  // cursor and place the new cursor after the same count of
  // non-whitespace characters in the formatted output. The
  // formatter only rearranges whitespace, so this lands the
  // cursor at the same logical token.
  async function formatInput() {
    const src = input.value;
    if (!src.trim()) return;
    const oldCursor = input.selectionStart;
    let nonWsBefore = 0;
    for (let i = 0; i < oldCursor; i++) {
      if (!/\s/.test(src[i])) nonWsBefore++;
    }
    let res;
    try {
      res = await fetch("/api/format?width=60", {
        method: "POST",
        body: src,
      });
    } catch (_) {
      return;
    }
    if (!res.ok) return;
    let formatted = await res.text();
    // tulisp-fmt always emits a trailing newline; the textarea
    // looks tidier without one for typical REPL fragments.
    if (formatted.endsWith("\n")) formatted = formatted.slice(0, -1);
    let newCursor = formatted.length;
    let seen = 0;
    for (let i = 0; i < formatted.length; i++) {
      if (!/\s/.test(formatted[i])) {
        if (seen === nonWsBefore) {
          newCursor = i;
          break;
        }
        seen++;
      }
    }
    input.value = formatted;
    input.setSelectionRange(newCursor, newCursor);
    refreshOverlay();
  }

  async function run() {
    const src = input.value.trim();
    if (!src) return;
    const entry = document.createElement("div");
    entry.className = "repl-entry";
    entry.innerHTML = `<pre class="repl-prompt">▸ ${escapeHtml(src)}</pre>`;
    output.appendChild(entry);
    output.scrollTop = output.scrollHeight;
    try {
      const res = await fetch(mgPath("eval"), { method: "POST", body: src });
      const data = await res.json();
      const klass = data.ok ? "repl-value" : "repl-error";
      const text = data.ok ? data.value : data.error;
      const out = document.createElement("pre");
      out.className = klass;
      out.textContent = text;
      entry.appendChild(out);
    } catch (err) {
      const out = document.createElement("pre");
      out.className = "repl-error";
      out.textContent = `transport error: ${err.message}`;
      entry.appendChild(out);
    }
    input.value = "";
    refreshOverlay();
    output.scrollTop = output.scrollHeight;
  }

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    run();
  });
  input.addEventListener("input", () => {
    refreshOverlay();
    refresh();
  });
  input.addEventListener("scroll", () => {
    overlay.scrollTop = input.scrollTop;
  });
  input.addEventListener("blur", () => {
    // Defer hide so click-on-li handlers fire first.
    setTimeout(() => {
      active = [];
      renderCompletions();
    }, 100);
  });
  input.addEventListener("keydown", (e) => {
    // Completion popup keys take priority when it's open.
    if (active.length) {
      if (e.key === "Tab" || (e.key === "Enter" && !e.ctrlKey && !e.metaKey)) {
        e.preventDefault();
        applyCompletion();
        return;
      }
      if (e.key === "ArrowDown") {
        e.preventDefault();
        selectedIdx = (selectedIdx + 1) % active.length;
        renderCompletions();
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        selectedIdx = (selectedIdx - 1 + active.length) % active.length;
        renderCompletions();
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        active = [];
        renderCompletions();
        return;
      }
    }
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      run();
      return;
    }
    // Tab when the completion popup isn't open: roundtrip the
    // textarea contents through /api/format. The popup-open case
    // is handled in the block above.
    if (e.key === "Tab" && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      formatInput();
      return;
    }
    // Plain Enter: walk back through the typed text to the
    // innermost still-open paren and indent the new line at its
    // column + 2. Strings / comments are skipped during the walk
    // so a `;` inside a comment (and an unbalanced `(` inside a
    // string) doesn't perturb the depth count. Doesn't replicate
    // tulisp-fmt's special-form rules (let bindings align under
    // first arg, etc.) — Tab roundtrips through the formatter for
    // canonical layout.
    if (e.key === "Enter" && !e.ctrlKey && !e.metaKey && !e.altKey) {
      const cursor = input.selectionStart;
      const end = input.selectionEnd;
      const indent = indentForNewline(input.value, cursor);
      const insert = `\n${" ".repeat(indent)}`;
      const v = input.value;
      e.preventDefault();
      input.value = v.slice(0, cursor) + insert + v.slice(end);
      input.setSelectionRange(cursor + insert.length, cursor + insert.length);
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return;
    }
    // Electric-pair: skip if user is also holding a modifier (so
    // Ctrl-9 etc. on layouts that produce `(` directly still
    // works as the user expects).
    if (e.ctrlKey || e.metaKey || e.altKey) return;

    const v = input.value;
    const s = input.selectionStart;
    const e2 = input.selectionEnd;
    if (e.key in PAIRS) {
      e.preventDefault();
      const open = e.key;
      const close = PAIRS[open];
      // Step-over when typing a quote and cursor is already
      // immediately before that same quote.
      if (open === close && s === e2 && v[s] === open) {
        input.setSelectionRange(s + 1, s + 1);
        return;
      }
      if (s === e2) {
        input.value = v.slice(0, s) + open + close + v.slice(s);
        input.setSelectionRange(s + 1, s + 1);
      } else {
        input.value = v.slice(0, s) + open + v.slice(s, e2) + close + v.slice(e2);
        input.setSelectionRange(s + 1, e2 + 1);
      }
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return;
    }
    if (CLOSES.has(e.key) && s === e2 && v[s] === e.key) {
      // Cursor sitting right before a matching close — just step
      // past instead of double-typing.
      e.preventDefault();
      input.setSelectionRange(s + 1, s + 1);
      return;
    }
    if (e.key === "Backspace" && s === e2 && s > 0) {
      const before = v[s - 1];
      const after = v[s];
      if (before in PAIRS && PAIRS[before] === after) {
        e.preventDefault();
        input.value = v.slice(0, s - 1) + v.slice(s + 1);
        input.setSelectionRange(s - 1, s - 1);
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
    }
  });
  // Initial paint so the overlay shows whatever the textarea was
  // pre-filled with (e.g. browser back-button restored content).
  refreshOverlay();
}

// Self-reconnecting WS with exponential backoff. Starts at 1 s,
// doubles on each close, caps at 30 s, resets to 1 s on a clean
// onopen. A laptop returning from sleep, a server bounce, or a
// notify-reload that briefly drops connections all heal without
// a manual page refresh — important for an overnight soak run.
//
// On reconnect (i.e. open after a previous open) we also nudge a
// topology refresh because samples may have moved while we were
// away. The very first open is a no-op there because init()
// already awaited refreshTopology before opening the WS.
function openWebSocket(onTopologyChanged) {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}/ws/events`;
  const MIN_DELAY = 1000;
  const MAX_DELAY = 30000;
  let delay = MIN_DELAY;
  let everConnected = false;
  function connect() {
    const ws = new WebSocket(url);
    ws.onopen = () => {
      delay = MIN_DELAY;
      if (everConnected) {
        // Catch up state the canvas and inspector cached from
        // before the drop. Loopback pill + dashboard tiles also
        // self-heal via their next poll / WS frame.
        onTopologyChanged(0);
      }
      everConnected = true;
    };
    ws.onmessage = (msg) => {
      // Defensive: vis-network and other libs sometimes pump non-string
      // frames (binary, blob) through fetch / WS pipelines that look
      // identical from a try/catch perspective. Surface the actual
      // payload type to console so a "JSON.parse undefined" surprise
      // points straight at the offending frame.
      if (typeof msg.data !== "string") {
        console.warn("WS: non-string payload, skipping:", msg.data);
        return;
      }
      let ev;
      try {
        ev = JSON.parse(msg.data);
      } catch (e) {
        console.warn("WS: JSON parse failed:", e.message, "payload was:", msg.data);
        return;
      }
      // Per-microgrid events carry mg_id (post-D3); we filter out
      // anything from a microgrid other than the currently-selected
      // one so the dashboard doesn't paint with samples from a
      // neighbour. Enterprise-scoped events (log, lagged) ship
      // mg_id = undefined and pass through regardless.
      const selectedMg = readSelectedMg();
      const perMg = ev.kind === "sample" || ev.kind === "microgrid_sample"
                 || ev.kind === "topology_changed" || ev.kind === "setpoint"
                 || ev.kind === "dispatch_changed";
      if (perMg && selectedMg != null && ev.mg_id != null && ev.mg_id !== selectedMg) {
        return;
      }
      if (ev.kind === "sample") {
        liveCharts.pushSample(ev.id, ev.metric, ev.ts_ms, ev.value);
        batteryPairs.applySample(ev);
        pvRows.applySample(ev);
        evRows.applySample(ev);
        chpRows.applySample(ev);
        gridFrequency.applySample(ev);
      } else if (ev.kind === "microgrid_sample") {
        dashboardTiles.applySample(ev);
      } else if (ev.kind === "topology_changed") {
        onTopologyChanged(ev.version);
      } else if (ev.kind === "setpoint") {
        liveCharts.pushSetpoint(ev);
        pulseBar.recordSetpoint();
      } else if (ev.kind === "log") {
        appendLog(ev);
      } else if (ev.kind === "dispatch_changed") {
        // The dispatch store changed for ev.mg_id; refetch only if
        // we're actually looking at that microgrid's Dispatches tab.
        if (selectedMg != null && readSubview() === "dispatches") {
          dispatchesPanel.render(selectedMg);
        }
      }
    };
    ws.onclose = () => {
      const secs = Math.round(delay / 1000);
      setStatus(`disconnected — retry in ${secs}s`, "error");
      setTimeout(connect, delay);
      delay = Math.min(delay * 2, MAX_DELAY);
    };
    // onerror fires alongside onclose; setStatus message stays as
    // the "retry in Xs" we just set so the user sees the recovery
    // plan rather than an opaque "ws error".
    ws.onerror = () => {};
  }
  connect();
}

// ─── Dashboard tiles ────────────────────────────────────────────────────────
//
// Aggregated metrics from the loopback Microgrid client flow into the
// Dashboard pane via two paths: (a) /api/microgrid/latest at mode-
// enter time so the tiles paint immediately with a real number, and
// (b) microgrid_sample WS frames for the per-second updates. Every
// tile selects its source via `data-stream="..."`; new tiles only
// have to declare the right stream name to participate.

// Parses a graph-crate-rendered formula like
//   MAX(#2 - COALESCE(#1002, #1001, 0.0), 0.0)
// into an AST: { kind: "op" | "call" | "ref" | "num", ... }. Used by
// the formula inspector (F4 stage 2) to pretty-print the formula
// with each #N as a clickable link to the topology canvas. Hand-
// rolled recursive descent — the grammar is tiny (numbers, refs,
// + - * /, function calls) and a parser library would dwarf it.
function parseFormula(src) {
  let i = 0;
  const skipWs = () => {
    while (i < src.length && /\s/.test(src[i])) i++;
  };
  const peek = () => {
    skipWs();
    return src[i];
  };
  const match = (re) => {
    skipWs();
    const m = src.slice(i).match(re);
    if (m && m.index === 0) {
      i += m[0].length;
      return m[0];
    }
    return null;
  };
  function expr() {
    let left = mul();
    while (peek() === "+" || peek() === "-") {
      const op = src[i++];
      left = { kind: "op", op, left, right: mul() };
    }
    return left;
  }
  function mul() {
    let left = atom();
    while (peek() === "*" || peek() === "/") {
      const op = src[i++];
      left = { kind: "op", op, left, right: atom() };
    }
    return left;
  }
  function atom() {
    skipWs();
    if (src[i] === "(") {
      i++;
      const e = expr();
      skipWs();
      if (src[i] === ")") i++;
      return { kind: "paren", inner: e };
    }
    if (src[i] === "#") {
      i++;
      const m = match(/^\d+/);
      return { kind: "ref", id: Number(m) };
    }
    const num = match(/^-?\d+(\.\d+)?([eE][-+]?\d+)?/);
    if (num != null) return { kind: "num", value: Number(num) };
    const ident = match(/^[A-Za-z_][A-Za-z0-9_]*/);
    if (ident) {
      skipWs();
      if (src[i] === "(") {
        i++;
        const args = [];
        skipWs();
        while (src[i] != null && src[i] !== ")") {
          args.push(expr());
          skipWs();
          if (src[i] === ",") {
            i++;
            continue;
          }
          break;
        }
        if (src[i] === ")") i++;
        return { kind: "call", name: ident, args };
      }
      return { kind: "ident", name: ident };
    }
    return { kind: "unknown", text: src.slice(i) };
  }
  return expr();
}

// Render a parsed formula AST as nested HTML. Each #N ref becomes a
// .formula-ref span carrying data-id so a delegated click handler
// can flip to Topology mode + select. Function calls (COALESCE /
// MAX / MIN / etc.) break onto their own lines when they contain
// more than two args, mirroring how prettier-style formatters wrap
// long arg lists; everything else stays inline so a short formula
// like `#2` doesn't expand to four lines for one ref.
function formulaToHtml(node) {
  // Local rather than the file-level `escapeHtml` so the formula
  // panel stays self-contained; named `escapeText` rather than
  // `escape` to avoid shadowing the global escape() function.
  const escapeText = (s) =>
    String(s).replace(
      /[&<>"']/g,
      (c) =>
        ({
          "&": "&amp;",
          "<": "&lt;",
          ">": "&gt;",
          '"': "&quot;",
          "'": "&#39;",
        })[c],
    );
  function rec(n) {
    switch (n.kind) {
      case "ref":
        return `<span class="formula-ref" data-id="${n.id}" title="select component ${n.id}">#${n.id}</span>`;
      case "num":
        return `<span class="formula-num">${n.value}</span>`;
      case "ident":
        return `<span class="formula-ident">${escapeText(n.name)}</span>`;
      case "paren":
        return `(${rec(n.inner)})`;
      case "op":
        return `${rec(n.left)} <span class="formula-op">${n.op}</span> ${rec(n.right)}`;
      case "call": {
        const args = n.args.map(rec);
        const head = `<span class="formula-call">${escapeText(n.name)}</span>`;
        if (args.length <= 2 && n.args.every((a) => a.kind === "ref" || a.kind === "num")) {
          return `${head}(${args.join(", ")})`;
        }
        const indented = args
          .map((a) => `  <div class="formula-arg">${a}</div>`)
          .join("");
        return `${head}(\n${indented})`;
      }
      default:
        return `<span class="formula-raw">${escapeText(n.text || "")}</span>`;
    }
  }
  return rec(node);
}

// Open the formula tree for the given stream in the inspector. Re-uses
// the inspector (same pattern as
// renderScenarioReport / renderDefaults) so the layout stays
// uniform.
async function openFormulaPanel(stream) {
  try {
    const res = await fetch(mgPath("microgrid/formulas"));
    if (!res.ok) return;
    const map = await res.json();
    const src = map[stream];
    if (!src) return;
    inspectEl.innerHTML = `
      <div class="formula-panel">
        <h2>Formula · <code>${stream}</code></h2>
        <pre class="formula-tree">${formulaToHtml(parseFormula(src))}</pre>
        <p class="hint">Click any <code>#N</code> to jump to that component on the Topology canvas.</p>
      </div>
    `;
    openInspector("formula");
    // Delegate refs: one listener per panel-open, no per-span hookup.
    inspectEl.querySelector(".formula-tree")?.addEventListener("click", (ev) => {
      const t = ev.target.closest(".formula-ref");
      if (!t) return;
      jumpToTopology(Number(t.dataset.id));
    });
  } catch (_) {
    // Best-effort.
  }
}

// ─── Grid frequency bridge ──────────────────────────────────────────────────

// Per-mg URL helper: prefixes /api/mg/{selected_id}/ when a
// microgrid is selected, falls back to /api/{suffix} otherwise
// (used by the loopback HTTP backfill on legacy endpoints that
// haven't been migrated yet, e.g. /api/setpoints + /api/format
// + /api/snapshots).
export function mgPath(suffix) {
  const id = readSelectedMg();
  return id == null ? `/api/${suffix}` : `/api/mg/${id}/${suffix}`;
}

// ─── Microgrids mode (list view + selection) ───────────────────────────────
//
// The Microgrids landing page: a card grid backed by
// /api/microgrids. Clicking a card flips MG_SELECTED_KEY and re-
// enters applyMode, which then shows the per-mg sub-view
// (dashboard / topology). A trailing [+ New microgrid] card opens
// a small create form (D4) — covered in a follow-up.

export async function loadFormulas() {
  try {
    const res = await fetch(mgPath("microgrid/formulas"));
    if (!res.ok) return;
    const map = await res.json();
    for (const [stream, formula] of Object.entries(map)) {
      for (const tile of document.querySelectorAll(`.dash-tile`)) {
        const v = tile.querySelector(`.dash-value[data-stream="${stream}"]`);
        if (v) {
          // Tile-level title so hovering anywhere on the card
          // (number + sparkline + meta) surfaces the formula. The
          // click handler installed below opens the side-panel
          // formula tree with each #N linked to the canvas.
          tile.title = `${stream} = ${formula}`;
          tile.classList.add("dash-tile-interactive");
          tile.dataset.formulaStream = stream;
        }
      }
    }
  } catch (_) {
    // Best-effort — tile tooltips just show their default `title`
    // (none) if this fails.
  }
}

// One delegated click handler covers every formula-bearing tile
// (existing pool tiles + any future ones loadFormulas tags). Tiles
// without a formulaStream are non-interactive and short-circuit
// here.
function setupFormulaTileClicks() {
  document.getElementById("dashboard")?.addEventListener("click", (ev) => {
    const tile = ev.target.closest(".dash-tile-interactive");
    if (!tile) return;
    const stream = tile.dataset.formulaStream;
    if (!stream) return;
    openFormulaPanel(stream);
  });
}



// ─── Dispatches (per-microgrid) ─────────────────────────────────────────────
//
// Read-only table of the dispatches switchyard's dispatch API holds for
// the selected microgrid. Rendered on entering the Dispatches sub-tab
// and refetched when a `dispatch_changed` WS event names this microgrid
// (the dispatch CLI created / updated / deleted one).
const dispatchesPanel = (() => {
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


// ─── Density toggle ────────────────────────────────────────────────────────
//
// CSS-only mode that shrinks tile + pulse-bar paddings and fonts.
// For power users on long soak runs who want more tiles + more
// rows on screen at once. Default = normal (the 32" 4K target
// keeps the comfortable layout the landing one). Preference
// persists in localStorage so a refresh keeps you put.
const DENSITY_KEY = "switchyard-density";

function applyDensity(mode) {
  const compact = mode === "compact";
  document.body.classList.toggle("compact", compact);
  const chip = document.getElementById("density-toggle");
  if (chip) {
    chip.classList.toggle("active", compact);
    chip.textContent = compact ? "compact" : "normal";
  }
}

export function setupDensityToggle() {
  const chip = document.getElementById("density-toggle");
  if (chip) {
    chip.addEventListener("click", () => {
      const next = document.body.classList.contains("compact") ? "normal" : "compact";
      localStorage.setItem(DENSITY_KEY, next);
      applyDensity(next);
    });
  }
  applyDensity(localStorage.getItem(DENSITY_KEY) || "normal");
}

// ─── Mode toggle ────────────────────────────────────────────────────────────
//
// The chrome's [Dashboard] [Topology] buttons swap which main pane
// is visible; CSS hides the other via `body[data-mode]`. Preference
// persists in localStorage so a refresh brings you back where you
// were. Default = dashboard — the new view is the one we want
// developers landing on.
const MODE_KEY = "switchyard-mode";
const MG_SELECTED_KEY = "switchyard-selected-mg";
const MG_SUBVIEW_KEY = "switchyard-mg-subview";
const VALID_MODES = new Set(["microgrids", "scenarios"]);
const VALID_SUBVIEWS = new Set(["dashboard", "topology", "dispatches"]);

function readSelectedMg() {
  const raw = localStorage.getItem(MG_SELECTED_KEY);
  if (raw == null || raw === "" || raw === "null") return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}
function readSubview() {
  const v = localStorage.getItem(MG_SUBVIEW_KEY);
  return VALID_SUBVIEWS.has(v) ? v : "dashboard";
}

// ─── URL routing ────────────────────────────────────────────────────────────
//
// SPA state — mode / selected microgrid / subview — round-trips
// through the URL hash so the browser's back / forward buttons
// walk the user's actual navigation history (not just whatever
// page-loads happened to land on this origin). Shapes:
//
//   #scenarios                       scenarios mode
//   #microgrids                      microgrids list view
//   #microgrids/2200                 mg 2200 selected (default subview = dashboard)
//   #microgrids/2200/topology        mg 2200, topology subview
//
// `localStorage` still carries the same keys so a fresh tab
// without a hash falls back to "wherever the user was last."
// The hash wins when both are set — explicit deep-links shouldn't
// be overridden by stale storage.

function currentRoute() {
  return {
    mode: localStorage.getItem(MODE_KEY) || "microgrids",
    selectedMg: readSelectedMg(),
    subview: readSubview(),
  };
}

function routeToHash({ mode, selectedMg, subview }) {
  if (mode === "scenarios") return "#scenarios";
  if (selectedMg == null) return "#microgrids";
  if (subview === "topology" || subview === "dispatches")
    return `#microgrids/${selectedMg}/${subview}`;
  return `#microgrids/${selectedMg}`;
}

function parseHash(hash) {
  // `#x` strips to `x`; an empty hash returns null so the caller
  // falls through to localStorage / defaults.
  const raw = (hash || "").replace(/^#/, "");
  if (!raw) return null;
  const parts = raw.split("/").filter(Boolean);
  if (parts[0] === "scenarios") return { mode: "scenarios", selectedMg: null, subview: "dashboard" };
  if (parts[0] === "microgrids") {
    const idRaw = parts[1];
    const id = idRaw == null ? null : Number(idRaw);
    const selectedMg = Number.isFinite(id) ? id : null;
    let subview = "dashboard";
    if (parts[2] && VALID_SUBVIEWS.has(parts[2])) subview = parts[2];
    return { mode: "microgrids", selectedMg, subview };
  }
  return null;
}

function writeRouteToStorage({ mode, selectedMg, subview }) {
  if (VALID_MODES.has(mode)) localStorage.setItem(MODE_KEY, mode);
  if (selectedMg == null) localStorage.removeItem(MG_SELECTED_KEY);
  else localStorage.setItem(MG_SELECTED_KEY, String(selectedMg));
  if (VALID_SUBVIEWS.has(subview)) localStorage.setItem(MG_SUBVIEW_KEY, subview);
}

// Navigate the SPA to `next`, pushing a new history entry so the
// browser back button returns to the previous route. `next` is a
// partial — keys you omit inherit from the current route. Skips
// the push when the resulting hash is the same as the current
// location (e.g. clicking the active tab is a no-op).
function navigateTo(next) {
  const route = { ...currentRoute(), ...next };
  writeRouteToStorage(route);
  const hash = routeToHash(route);
  if (window.location.hash !== hash) {
    history.pushState(route, "", hash);
  }
  applyMode(route.mode);
}

function setupRouterPopstate() {
  // Browser back / forward fires popstate. Re-seed storage from
  // the destination route's hash (the state we pushed isn't
  // always present — e.g. after a refresh on a hashed URL) and
  // re-apply without pushing.
  window.addEventListener("popstate", () => {
    const route = parseHash(window.location.hash) || currentRoute();
    writeRouteToStorage(route);
    applyMode(route.mode);
  });
}

function applyInitialRoute() {
  // Deep-link wins over stored state: refresh on a hashed URL
  // restores that exact view. Otherwise fall through to whatever
  // the user was last looking at.
  const hashed = parseHash(window.location.hash);
  if (hashed) writeRouteToStorage(hashed);
  const route = currentRoute();
  // history.replaceState the canonical hash so back-button from
  // here lands on a defined state instead of an empty-hash entry.
  const hash = routeToHash(route);
  history.replaceState(route, "", hash);
  applyMode(route.mode);
}

function applyMode(mode) {
  if (!VALID_MODES.has(mode)) mode = "microgrids";
  const selected = readSelectedMg();
  const subview = readSubview();
  document.body.dataset.mode = mode;
  document.body.dataset.mgView = selected == null ? "list" : "selected";
  document.body.dataset.subview = subview;
  // Switching tab/mode dismisses the floating panels — the inspector's
  // selection no longer applies, and the add panel is topology-only.
  clearSide();
  document.getElementById("add-panel").classList.remove("open");
  for (const btn of document.querySelectorAll("#mode-toggle .mode-btn")) {
    btn.classList.toggle("active", btn.dataset.mode === mode);
  }
  for (const btn of document.querySelectorAll("#mg-subtoggle .mode-btn")) {
    btn.classList.toggle("active", btn.dataset.subview === subview);
  }
  // vis-network needs a redraw nudge when its container goes from
  // display:none back to visible — the canvas was sized to 0×0 while
  // hidden. Same shape the splitter resize handler uses. Defer the
  // fit one animation-frame so the just-flipped `data-subview` has
  // settled the CSS visibility before vis-network measures.
  if (mode === "microgrids" && selected != null && subview === "topology") {
    refitCharts();
    requestAnimationFrame(() => topology.fit());
  }
  if (mode === "microgrids" && selected != null && subview === "dashboard") {
    dashboardTiles.backfill();
    gridFrequency.backfill();
  }
  if (mode === "microgrids" && selected != null && subview === "dispatches") {
    dispatchesPanel.render(selected);
  }
  if (mode === "microgrids") microgridsPanel.refresh();
  if (mode === "scenarios") scenariosPanel.refresh();
}

// Jump to the topology subview within the current mode and select
// `id` on the canvas. Used by dashboard tier rows + the formula-tree
// chip clicks. Pushes a history entry so the back button returns
// the user to where they clicked from.
export function jumpToTopology(id) {
  navigateTo({ subview: "topology" });
  topology.select([id]);
  const c = topology.get(id);
  if (c) showComponent(c);
}

export function selectMicrogrid(id) {
  navigateTo({ mode: "microgrids", selectedMg: id });
  renderReplMgChip();
  // Refetch the per-mg topology so the canvas + the empty-hint
  // overlay (D5) reflect the newly-selected microgrid. Without
  // this the canvas keeps showing the previous microgrid's
  // components until a WS topology_changed event arrives — which
  // never happens just because the selection changed client-side.
  if (id != null) refreshTopology();
}

// REPL chip — surfaces which microgrid the REPL form's POSTs
// route to. Mirrors mgPath()'s logic: shows "→ {name}" when a
// microgrid is selected, "→ enterprise" otherwise. Clicking
// jumps to the Microgrids list so the operator can pick a
// different one.
function renderReplMgChip() {
  const chip = document.getElementById("repl-mg-chip");
  if (!chip) return;
  const id = readSelectedMg();
  if (id == null) {
    chip.textContent = "→ enterprise";
    chip.classList.add("muted");
    return;
  }
  chip.classList.remove("muted");
  // Pull the name from the microgridsPanel's cache if available;
  // fall back to "#id" so the chip never sits empty.
  const cached = (window.__mgPanelCache || []).find((m) => m.id === id);
  chip.textContent = `→ ${cached ? cached.name || `#${id}` : `#${id}`}`;
}

function setupReplMgChip() {
  const chip = document.getElementById("repl-mg-chip");
  if (!chip) return;
  chip.addEventListener("click", () => {
    navigateTo({ mode: "microgrids", selectedMg: null });
    renderReplMgChip();
  });
  renderReplMgChip();
}

function setupModeToggle() {
  for (const btn of document.querySelectorAll("#mode-toggle .mode-btn")) {
    btn.addEventListener("click", () => {
      const mode = btn.dataset.mode;
      // Microgrids button returns the user to the list. Picking a
      // microgrid (D2 cards) re-enters the selected view.
      navigateTo({
        mode,
        selectedMg: mode === "microgrids" ? null : currentRoute().selectedMg,
      });
    });
  }
  for (const btn of document.querySelectorAll("#mg-subtoggle .mode-btn")) {
    btn.addEventListener("click", () => {
      const sv = btn.dataset.subview;
      if (!VALID_SUBVIEWS.has(sv)) return;
      navigateTo({ subview: sv });
    });
  }
  const backBtn = document.getElementById("mg-back");
  if (backBtn) backBtn.addEventListener("click", () => selectMicrogrid(null));
  applyInitialRoute();
  setupRouterPopstate();
  // Keyboard chord — 1 → Microgrids list, 2 → Scenarios. Skip
  // when a text input has focus so digits typed into the REPL /
  // search boxes don't trigger a mode flip.
  document.addEventListener("keydown", (ev) => {
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const t = ev.target;
    const tag = t?.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || t?.isContentEditable) return;
    let mode = null;
    if (ev.key === "1") mode = "microgrids";
    else if (ev.key === "2") mode = "scenarios";
    if (!mode) return;
    ev.preventDefault();
    navigateTo({
      mode,
      selectedMg: mode === "microgrids" ? null : currentRoute().selectedMg,
    });
  });
}

async function refreshTopology() {
  try {
    const res = await fetch(mgPath("topology"));
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    topology.apply(data);
    // Pulse bar's health counters + graph pill read from the
    // same /api/topology fetch — one round-trip carries both
    // signals + a hot-reload's WS topology_changed nudge
    // already drives a refresh.
    pulseBar.applyTopology(data.components || [], data.graph_status);
    batteryPairs.refresh(data);
    pvRows.refresh(data);
    evRows.refresh(data);
    chpRows.refresh(data);
    gridFrequency.applyTopology(data);
  } catch (err) {
    setStatus(`error: ${err.message}`, "error");
  }
}

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
