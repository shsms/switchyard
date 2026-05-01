// Phase-1 SPA. Renders /api/topology with vis-network, and on node
// selection shows category-appropriate live charts in the side panel.
// Visual editing (add / connect / rename / delete) + REPL + Persist
// + Defaults / Scenarios all hang off the same /api/eval mutation
// path so anything done in the UI is also scriptable from outside.

const status = document.getElementById("status");
// `inspect` is the swappable upper half of the side panel; the lower
// half (`add-form`) stays put across selection changes.
const inspectEl = document.getElementById("inspect");

function setStatus(text, klass) {
  status.textContent = text;
  status.className = "status " + (klass || "");
}

const CATEGORY_COLOR = {
  grid: getCss("--cat-grid"),
  meter: getCss("--cat-meter"),
  inverter: getCss("--cat-inverter"),
  battery: getCss("--cat-battery"),
  "ev-charger": getCss("--cat-ev-charger"),
  chp: getCss("--cat-chp"),
};

// Charts to render when a component of this category is selected.
// One uPlot per metric — multi-series (e.g. P + bound envelope on one
// chart) lands when we tackle the merge-by-shared-timestamp problem.
const CHARTS_BY_CATEGORY = {
  grid: ["frequency_hz"],
  meter: ["active_power_w", "reactive_power_var"],
  inverter: ["active_power_w", "reactive_power_var"],
  battery: ["soc_pct"],
  "ev-charger": ["soc_pct"],
  chp: ["active_power_w"],
};

// Per-metric chart presentation. `kind: "power"` triggers W → kW →
// MW auto-scaling based on the data range; `linear` skips scaling
// and just appends the fixed unit. Title doesn't carry a unit
// itself — the unit string from `chooseScale` gets appended at
// chart creation so it reflects the actual displayed magnitudes.
const METRIC_PRESENTATION = {
  active_power_w:     { title: "Active Power",   kind: "power", baseUnit: "W" },
  reactive_power_var: { title: "Reactive Power", kind: "power", baseUnit: "VAR" },
  frequency_hz:       { title: "Frequency",      kind: "linear", unit: "Hz" },
  soc_pct:            { title: "SoC",            kind: "linear", unit: "%" },
};

function chooseScale(rule, values) {
  if (rule.kind === "power" && values.length) {
    const max = Math.max(...values.map((v) => Math.abs(v)));
    if (max >= 1e6) return { div: 1e6, unit: "M" + rule.baseUnit };
    if (max >= 1e3) return { div: 1e3, unit: "k" + rule.baseUnit };
    return { div: 1, unit: rule.baseUnit };
  }
  return { div: 1, unit: rule.unit || "" };
}

function getCss(name) {
  return getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
}

// Live-chart state for whichever component the user has selected.
// Replaced wholesale on every selection change; the previous uPlots
// get destroyed in clearCharts.
let activeCharts = null;

// vis-network instance + DataSets — module-scoped so the WS
// topology-changed handler can refresh in place without tearing
// down the canvas.
let network = null;
let nodesDS = null;
let edgesDS = null;
// Last topology snapshot keyed by id; used to recover category +
// subtype on selection (DataSet stores them but it's clearer to keep
// our own).
let componentById = new Map();

// Brighten a #rrggbb hex by `n` per channel (clamped to 255). Used to
// derive hover + selected node-background tints from the canonical
// category color so the same node visibly reacts to interaction
// without us hand-picking a separate palette per state.
function lighten(hex, n) {
  const c = parseInt(hex.slice(1), 16);
  const r = Math.min(255, ((c >> 16) & 255) + n);
  const g = Math.min(255, ((c >> 8) & 255) + n);
  const b = Math.min(255, (c & 255) + n);
  return "#" + ((r << 16) | (g << 8) | b).toString(16).padStart(6, "0");
}

function nodeStyleFor(c) {
  const healthBorder = {
    ok: "#1c2128",     // matches --bg — subtle outline at rest
    standby: "#c4ad55", // toned-down yellow
    error: "#e58275",   // toned-down red, matches --bad
  }[c.health || "ok"];
  const healthWidth = c.health === "ok" ? 1 : 3;
  const bg = CATEGORY_COLOR[c.category] || "#888";
  return {
    id: c.id,
    label: c.name,
    shape: "ellipse",
    color: {
      background: bg,
      border: healthBorder,
      // Selected: lighter background + accent border. Hover: a
      // smaller lift so it's a softer "you can click this" cue.
      highlight: { background: lighten(bg, 36), border: "#79b8ff" },
      hover: { background: lighten(bg, 18), border: "#b0b8c1" },
    },
    borderWidth: healthWidth,
    borderWidthSelected: 4,
    // vis-network's default `chosen` behaviour bolds the label on
    // selection (and on hover). Drop the label part — color
    // changes (selected border, hover border) carry the signal,
    // we don't need a font-weight shift on top.
    chosen: { node: true, label: false },
    font: {
      color: "#1c2128",
      face: "ui-monospace, monospace",
      size: 14,
    },
    margin: { top: 9, right: 16, bottom: 9, left: 16 },
    // Minimum oval size so short-label nodes (grid-1, meter-2) don't
    // shrink below the readable threshold. Long-label nodes still
    // grow to fit via the margin.
    widthConstraint: { minimum: 78 },
    heightConstraint: { minimum: 34 },
  };
}

function buildVisData(topology) {
  componentById = new Map();
  const visible = topology.components.filter((c) => !c.hidden);
  const nodes = visible.map((c) => {
    componentById.set(c.id, c);
    return nodeStyleFor(c);
  });
  const edges = topology.connections.map(([p, c]) => ({
    id: `${p}-${c}`,
    from: p,
    to: c,
    arrows: "to",
  }));
  return { nodes, edges };
}

// Layout: vis-network's hierarchical mode places nodes on
// integer-numbered levels by edge direction. `LR` reads left → right —
// roots on the left, leaves to the right. Physics off keeps nodes
// pinned where the layout placed them so the canvas stays stable
// across data updates.
const visOptions = {
  layout: {
    hierarchical: {
      enabled: true,
      direction: "LR",
      sortMethod: "directed",
      nodeSpacing: 120,
      levelSeparation: 180,
      treeSpacing: 140,
    },
  },
  physics: { enabled: false },
  interaction: {
    hover: true,
    dragNodes: true,
    multiselect: false,
    selectConnectedEdges: false,
    navigationButtons: false,
    keyboard: { enabled: false },
  },
  edges: {
    color: { color: "#6b7280", highlight: "#79b8ff", hover: "#b0b8c1" },
    width: 1.5,
    smooth: { enabled: true, type: "cubicBezier", forceDirection: "horizontal", roundness: 0.4 },
    arrows: { to: { enabled: true, scaleFactor: 0.6 } },
  },
  // The manipulation API powers shift+drag connect (next handler);
  // keeping the toolbar hidden because we drive edit modes
  // programmatically via key state.
  manipulation: {
    enabled: false,
    addEdge: (data, callback) => {
      if (data.from === data.to) {
        callback(null);
        return;
      }
      fetch("/api/eval", {
        method: "POST",
        body: `(world-connect ${data.from} ${data.to})`,
      })
        .then((r) => r.json())
        .then((res) => {
          if (!res.ok) alert("Connect failed:\n" + res.error);
        });
      // Don't apply locally — the WS topology refresh will redraw
      // with the new edge once the eval lands on the server.
      callback(null);
    },
  },
};

function clearCharts() {
  if (!activeCharts) return;
  for (const ch of activeCharts.charts.values()) ch.plot.destroy();
  activeCharts = null;
}

// Categories that the gRPC server actually accepts setpoints on.
// command-mode (timeout / error fault simulation) only makes sense
// for these — grids and meters have no setpoint surface, so we hide
// the dropdown rather than offering a knob that does nothing.
const ACCEPTS_SETPOINTS = new Set(["battery", "inverter", "ev-charger", "chp"]);

function renderInspect(d, parentIds, childIds) {
  const renderEdgeRow = (id, dataAttr) => {
    const c = componentById.get(id);
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
    evalQuoted(`(world-rename-component ${d.id} "${jsToLispString(name)}")`);
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
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-from]")) {
    btn.addEventListener("click", () =>
      evalQuoted(`(world-disconnect ${btn.dataset.disconnectFrom} ${d.id})`),
    );
  }
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-to]")) {
    btn.addEventListener("click", () =>
      evalQuoted(`(world-disconnect ${d.id} ${btn.dataset.disconnectTo})`),
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
  const res = await fetch("/api/eval", { method: "POST", body: expr });
  // res.json() can throw "JSON.parse: unexpected character" if the
  // server returned an empty / non-JSON body (e.g. a 5xx with HTML
  // error page, or a connection that died mid-response). Surface the
  // raw text so the actual culprit shows up in the console instead
  // of an opaque parse error.
  const text = await res.text();
  let data;
  try {
    data = JSON.parse(text);
  } catch (e) {
    console.error(`evalQuoted: bad JSON for ${expr.slice(0, 60)}…`, {
      status: res.status,
      body: text,
    });
    alert(`${expr}\n\nserver returned non-JSON (HTTP ${res.status}):\n${text}`);
    return;
  }
  if (!data.ok) alert(`${expr}\n\nfailed:\n${data.error}`);
}

async function showComponent(d) {
  if (!d) return;
  clearCharts();

  // vis-network's getConnectedNodes(id, direction) returns the
  // ids on either side of the selected node — cheaper than walking
  // /api/topology for the disconnect buttons. Display labels get
  // resolved by renderInspect via componentById lookups.
  const parentIds = network ? network.getConnectedNodes(d.id, "from") : [];
  const childIds = network ? network.getConnectedNodes(d.id, "to") : [];
  renderInspect(d, parentIds, childIds);

  const metrics = CHARTS_BY_CATEGORY[d.category] || [];
  const container = document.getElementById("charts");
  const charts = new Map(); // metric → { plot, xs, ys }

  for (const metric of metrics) {
    const slot = document.createElement("div");
    slot.className = "chart";
    container.appendChild(slot);
    const url = `/api/history?id=${d.id}&metric=${metric}&window_s=300`;
    const samples = (await (await fetch(url)).json()).samples || [];
    const xs = samples.map(([t]) => t / 1000);
    const ys = samples.map(([, v]) => v);
    const { plot, scale } = makePlot(slot, metric, xs, ys);
    // Stored ys are pre-scaled (already divided by scale.div) so the
    // live push path can append by dividing each new sample once.
    charts.set(metric, { plot, xs, ys: ys.map((y) => y / scale.div), scale });
  }
  activeCharts = { id: d.id, charts };

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
      li.className = "sp-event " + (accepted ? "accepted" : "rejected");
      const ts = new Date(e.ts).toLocaleTimeString();
      const tag = e.kind.replace("_", " ");
      const head = `<span class="sp-ts">${ts}</span> <span class="sp-tag">${tag}</span> <span class="sp-val">${e.value}</span>`;
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

function makePlot(container, metric, xs, ys) {
  const rule = METRIC_PRESENTATION[metric] || { title: metric, kind: "linear", unit: "" };
  const scale = chooseScale(rule, ys);
  const scaledYs = ys.map((y) => y / scale.div);
  const opts = {
    width: container.clientWidth || 280,
    height: 140,
    title: scale.unit ? `${rule.title} (${scale.unit})` : rule.title,
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

function pushSetpoint(ev) {
  // Only render if the event is for the currently-inspected
  // component; otherwise it'll be picked up next time the user
  // selects that node (the server's per-component log is the source
  // of truth).
  if (!activeCharts || activeCharts.id !== Number(ev.id)) return;
  const list = inspectEl.querySelector(".sp-list");
  if (!list) return;
  // Drop the "none" placeholder if it's still showing.
  const empty = list.querySelector(".sp-empty");
  if (empty) empty.remove();
  const li = document.createElement("li");
  li.className = "sp-event " + (ev.accepted ? "accepted" : "rejected");
  const ts = new Date(ev.ts_ms).toLocaleTimeString();
  // The WS event carries the setpoint kind on `setpoint_kind` to
  // dodge collision with the WorldEvent discriminator (also called
  // `kind`).
  const tag = ev.setpoint_kind.replace("_", " ");
  const head = `<span class="sp-ts">${ts}</span> <span class="sp-tag">${tag}</span> <span class="sp-val">${ev.value}</span>`;
  const body = ev.accepted
    ? '<span class="sp-ok">✓ accepted</span>'
    : `<span class="sp-bad">✕ ${escapeHtml(ev.reason || "")}</span>`;
  li.innerHTML = `${head}<br/>${body}`;
  list.prepend(li);
  // Trim if the list is getting long — match the 600s window used
  // by the initial fetch.
  while (list.children.length > 100) list.removeChild(list.lastChild);
}

function pushSample(id, metric, ts_ms, value) {
  if (!activeCharts || activeCharts.id !== Number(id)) return;
  const series = activeCharts.charts.get(metric);
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
}

function clearSide() {
  clearCharts();
  inspectEl.innerHTML =
    '<p class="hint">Click a node to inspect. Right-click to delete.</p>';
}

function setupAddForm() {
  const sel = document.getElementById("add-category");
  const btn = document.getElementById("add-btn");
  btn.addEventListener("click", async () => {
    const fn = sel.value;
    btn.disabled = true;
    try {
      const res = await fetch("/api/eval", {
        method: "POST",
        body: `(${fn})`,
      });
      const data = await res.json();
      if (!data.ok) alert("Create failed:\n" + data.error);
    } finally {
      btn.disabled = false;
    }
  });
}

function escapeHtml(s) {
  return String(s).replace(/[<>&]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;" })[c]);
}

function setupPersistControls() {
  const pill = document.getElementById("pending-pill");
  const count = document.getElementById("pending-count");
  const persistBtn = document.getElementById("persist-btn");
  const discardBtn = document.getElementById("discard-btn");

  async function refresh() {
    try {
      const res = await fetch("/api/pending");
      const data = await res.json();
      const n = data.entries.length;
      count.textContent = n;
      pill.hidden = n === 0;
      persistBtn.disabled = n === 0;
      discardBtn.disabled = n === 0;
    } catch (_) {
      // Best-effort — server unreachable just leaves last known state.
    }
  }

  persistBtn.addEventListener("click", async () => {
    persistBtn.disabled = true;
    try {
      const res = await fetch("/api/persist", { method: "POST" });
      const data = await res.json();
      console.log(`persisted ${data.persisted} entries to ${data.path}`);
    } finally {
      refresh();
    }
  });

  discardBtn.addEventListener("click", async () => {
    if (!confirm("Discard all unsaved edits and reload?")) return;
    discardBtn.disabled = true;
    try {
      await fetch("/api/discard", { method: "POST" });
    } finally {
      // Discard triggers a server-side reload which fires
      // TopologyChanged on the WS — that handler re-fetches
      // /api/topology and we'll see the rolled-back state.
      refresh();
    }
  });

  return refresh;
}

// Defaults editor — toggled from the chrome button. Replaces the
// inspect+add-form view of the side panel while open; toggling off
// (or selecting a node) restores the inspect view.
function setupDefaultsToggle() {
  const btn = document.getElementById("defaults-btn");
  let open = false;
  btn.addEventListener("click", async () => {
    open = !open;
    btn.classList.toggle("primary", open);
    if (open) {
      await renderDefaults();
    } else {
      clearSide();
      document.getElementById("add-form").style.display = "";
    }
  });
  return () => (open = false);
}

function setupScenariosToggle() {
  const btn = document.getElementById("scenarios-btn");
  let open = false;
  btn.addEventListener("click", async () => {
    open = !open;
    btn.classList.toggle("primary", open);
    if (open) {
      await renderScenarios();
    } else {
      clearSide();
      document.getElementById("add-form").style.display = "";
    }
  });
}

async function renderScenarios() {
  const res = await fetch("/api/scenarios");
  const data = await res.json();
  document.getElementById("add-form").style.display = "none";
  const items = data.names.length
    ? data.names
        .map(
          (n) =>
            `<li><span class="sc-name">${escapeHtml(n)}</span>
             <button class="hdr-btn" data-load="${escapeHtml(n)}">Load</button></li>`,
        )
        .join("")
    : '<li class="hint">no scenarios saved yet</li>';
  inspectEl.innerHTML = `
    <h2>Scenarios</h2>
    <p class="hint">
      Save the current pending edits as a named recipe; load to replay
      them into a new pending log (then Persist or Discard).
    </p>
    <div class="sc-save">
      <input id="sc-save-name" placeholder="scenario-name" spellcheck="false" />
      <button id="sc-save-btn" class="hdr-btn primary">Save current</button>
    </div>
    <h3>Saved</h3>
    <ul class="sc-list">${items}</ul>
  `;
  document.getElementById("sc-save-btn").addEventListener("click", async () => {
    const name = document.getElementById("sc-save-name").value.trim();
    if (!name) return;
    const r = await fetch(
      `/api/scenarios/save?name=${encodeURIComponent(name)}`,
      { method: "POST" },
    );
    if (r.ok) {
      renderScenarios();
    } else {
      alert(`Save failed: ${await r.text()}`);
    }
  });
  for (const btn of inspectEl.querySelectorAll("[data-load]")) {
    btn.addEventListener("click", async () => {
      const name = btn.dataset.load;
      const r = await fetch(
        `/api/scenarios/load?name=${encodeURIComponent(name)}`,
        { method: "POST" },
      );
      if (!r.ok) alert(`Load failed: ${await r.text()}`);
    });
  }
}

async function renderDefaults() {
  const res = await fetch("/api/defaults");
  const data = await res.json();
  document.getElementById("add-form").style.display = "none";
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

/// Drag the splitter between the topology canvas and the side panel.
/// Updates main's grid-template-columns live; on each frame, also
/// nudges any open uPlot charts (uPlot doesn't auto-resize) to the
/// new container width. vis-network handles its own resize via
/// ResizeObserver on the topology container.
function setupSplitter() {
  const splitter = document.getElementById("splitter");
  const main = document.getElementById("app");
  const sideEl = document.getElementById("side");
  const SIDE_MIN = 300; // anything narrower and the inspect form wraps badly
  const SIDE_MAX_FRAC = 0.7; // don't let the canvas drop below 30% of width

  let dragging = false;
  let startX = 0;
  let startWidth = 0;

  splitter.addEventListener("mousedown", (e) => {
    dragging = true;
    startX = e.clientX;
    startWidth = sideEl.getBoundingClientRect().width;
    splitter.classList.add("dragging");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    e.preventDefault();
  });
  document.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    const dx = startX - e.clientX;
    const sideMax = window.innerWidth * SIDE_MAX_FRAC;
    const newWidth = Math.min(sideMax, Math.max(SIDE_MIN, startWidth + dx));
    main.style.gridTemplateColumns = `1fr 5px ${newWidth}px`;
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

function refitCharts() {
  if (!activeCharts) return;
  for (const series of activeCharts.charts.values()) {
    const parent = series.plot.root.parentElement;
    if (!parent) continue;
    series.plot.setSize({
      width: parent.clientWidth,
      height: 140,
    });
  }
}

function setupRepl() {
  const form = document.getElementById("repl-form");
  const input = document.getElementById("repl-input");
  const output = document.getElementById("repl-output");

  async function run() {
    const src = input.value.trim();
    if (!src) return;
    const entry = document.createElement("div");
    entry.className = "repl-entry";
    entry.innerHTML = `<pre class="repl-prompt">▸ ${escapeHtml(src)}</pre>`;
    output.appendChild(entry);
    output.scrollTop = output.scrollHeight;
    try {
      const res = await fetch("/api/eval", { method: "POST", body: src });
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
      out.textContent = "transport error: " + err.message;
      entry.appendChild(out);
    }
    input.value = "";
    output.scrollTop = output.scrollHeight;
  }

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    run();
  });
  input.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      run();
    }
  });
}

function openWebSocket(onTopologyChanged) {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(`${proto}//${location.host}/ws/events`);
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
    if (ev.kind === "sample") {
      pushSample(ev.id, ev.metric, ev.ts_ms, ev.value);
    } else if (ev.kind === "topology_changed") {
      onTopologyChanged(ev.version);
    } else if (ev.kind === "setpoint") {
      pushSetpoint(ev);
    }
  };
  ws.onclose = () => setStatus("disconnected", "error");
  ws.onerror = () => setStatus("ws error", "error");
  return ws;
}

function applyTopology(topology) {
  const visibleCount = topology.components.filter((c) => !c.hidden).length;
  setStatus(
    `${visibleCount} components, ${topology.connections.length} connections`,
    "connected",
  );
  const { nodes, edges } = buildVisData(topology);
  if (!network) {
    nodesDS = new vis.DataSet(nodes);
    edgesDS = new vis.DataSet(edges);
    network = new vis.Network(
      document.getElementById("topology"),
      { nodes: nodesDS, edges: edgesDS },
      visOptions,
    );

    network.on("click", (params) => {
      if (params.nodes.length) {
        const id = params.nodes[0];
        showComponent(componentById.get(id));
      } else {
        clearSide();
      }
    });

    // Right-click → delete confirm. vis-network gives the canvas
    // pointer; getNodeAt translates to a node id.
    network.on("oncontext", (params) => {
      params.event.preventDefault();
      const id = network.getNodeAt(params.pointer.DOM);
      if (id == null) return;
      const c = componentById.get(id);
      if (!c) return;
      if (!confirm(`Delete ${c.name} (id ${c.id})?`)) return;
      fetch("/api/eval", {
        method: "POST",
        body: `(world-remove-component ${c.id})`,
      })
        .then((r) => r.json())
        .then((res) => {
          if (!res.ok) alert("Delete failed: " + res.error);
        });
    });

    // Shift toggles vis-network's addEdge mode. Hold Shift, drag
    // from one node to another to wire them. The addEdge callback
    // (defined in visOptions) POSTs world-connect and the WS topology
    // refresh redraws.
    document.addEventListener("keydown", (e) => {
      if (e.key === "Shift" && network) network.addEdgeMode();
    });
    document.addEventListener("keyup", (e) => {
      if (e.key === "Shift" && network) network.disableEditMode();
    });
  } else {
    // Diff the DataSets — preserves selection, layout positions, and
    // any in-flight drag interactions, instead of tearing down the
    // canvas on every WS topology event.
    const prevSelected = network.getSelectedNodes();
    const newIds = new Set(nodes.map((n) => n.id));
    const stale = nodesDS.getIds().filter((id) => !newIds.has(id));
    if (stale.length) nodesDS.remove(stale);
    nodesDS.update(nodes);

    const newEdgeIds = new Set(edges.map((e) => e.id));
    const staleEdges = edgesDS.getIds().filter((id) => !newEdgeIds.has(id));
    if (staleEdges.length) edgesDS.remove(staleEdges);
    edgesDS.update(edges);

    if (prevSelected.length) {
      const stillThere = prevSelected.filter((id) => componentById.has(id));
      if (stillThere.length) {
        network.selectNodes(stillThere);
        showComponent(componentById.get(stillThere[0]));
      } else {
        clearSide();
      }
    }
  }
}

async function refreshTopology() {
  try {
    const res = await fetch("/api/topology");
    if (!res.ok) throw new Error("HTTP " + res.status);
    applyTopology(await res.json());
  } catch (err) {
    setStatus("error: " + err.message, "error");
  }
}

async function init() {
  setupAddForm();
  setupDefaultsToggle();
  setupScenariosToggle();
  setupSplitter();
  const refreshPending = setupPersistControls();
  await refreshTopology();
  await refreshPending();
  // WS push: refresh both the topology (so the canvas reflects the
  // mutation) and the pending pill (so the count + button state
  // updates) on every TopologyChanged. Sample events go straight
  // into the live-charts router.
  openWebSocket((_v) => {
    refreshTopology();
    refreshPending();
  });
  setupRepl();
}

init();
