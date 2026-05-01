// Phase-1 SPA. Renders /api/topology with cytoscape, and on node
// selection shows category-appropriate live charts in the side panel.
// REPL panel + topology editing land in the next commits.

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

const METRIC_TITLES = {
  active_power_w: "Active Power (W)",
  reactive_power_var: "Reactive Power (VAR)",
  frequency_hz: "Frequency (Hz)",
  soc_pct: "SoC (%)",
};

function getCss(name) {
  return getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
}

// Live-chart state for whichever component the user has selected.
// Replaced wholesale on every selection change; the previous uPlots
// get destroyed in clearCharts.
let activeCharts = null;

// Cytoscape instance — module-scoped so the WS topology-changed
// handler can refresh it without tearing the whole panel down.
let cy = null;

function buildElements(topology) {
  const visible = topology.components.filter((c) => !c.hidden);
  const nodes = visible.map((c) => ({
    data: {
      id: String(c.id),
      name: c.name,
      category: c.category,
      subtype: c.subtype,
      // Drives the health-color border via cytoscape selectors.
      health: c.health,
    },
  }));
  const edges = topology.connections.map(([p, c]) => ({
    data: {
      id: `${p}-${c}`,
      source: String(p),
      target: String(c),
    },
  }));
  return [...nodes, ...edges];
}

function cytoscapeStylesheet() {
  const perCategory = Object.entries(CATEGORY_COLOR).map(([cat, color]) => ({
    selector: `node[category="${cat}"]`,
    style: { "background-color": color },
  }));

  return [
    {
      selector: "node",
      style: {
        shape: "ellipse",
        "background-color": "#888",
        label: "data(name)",
        // Centered inside the oval. The label drives node sizing —
        // `width: label` + padding makes every node fit its name
        // snugly, no truncation.
        color: "#0d1117",
        "text-valign": "center",
        "text-halign": "center",
        "font-size": 11,
        "font-family": "ui-monospace, monospace",
        "font-weight": "bold",
        width: "label",
        height: "label",
        "padding-left": "12px",
        "padding-right": "12px",
        "padding-top": "8px",
        "padding-bottom": "8px",
        "border-width": 1,
        "border-color": "#0d1117",
      },
    },
    ...perCategory,
    // Health-driven border. Subtle on healthy, loud on faults so an
    // engineer can spot a degraded component without clicking each one.
    {
      selector: 'node[health = "ok"]',
      style: { "border-color": "#0d1117", "border-width": 1 },
    },
    {
      selector: 'node[health = "standby"]',
      style: { "border-color": "#d2a106", "border-width": 3 },
    },
    {
      selector: 'node[health = "error"]',
      style: { "border-color": "#f85149", "border-width": 3 },
    },
    // Hover lift — slight darkening + border so the user sees what
    // they're about to click. Cheap signal of interactivity.
    {
      selector: "node.hovered",
      style: { "border-color": "#c9d1d9", "border-width": 2 },
    },
    {
      selector: "node:selected",
      style: { "border-width": 4, "border-color": "#58a6ff" },
    },
    {
      selector: "edge",
      style: {
        "curve-style": "bezier",
        "target-arrow-shape": "triangle",
        "line-color": "#3a3f48",
        "target-arrow-color": "#3a3f48",
        width: 1.5,
      },
    },
  ];
}

// breadthfirst's transform option swaps the (x, y) of every node
// after layout. Default direction is top → bottom; this flips to
// left → right, which reads more like a one-line flow chart.
const layoutOptions = {
  name: "breadthfirst",
  directed: true,
  padding: 30,
  spacingFactor: 1.4,
  transform: (_node, pos) => ({ x: pos.y, y: pos.x }),
};

function clearCharts() {
  if (!activeCharts) return;
  for (const ch of activeCharts.charts.values()) ch.plot.destroy();
  activeCharts = null;
}

function renderInspect(d, parentIds, childIds) {
  const renderEdgeRow = (id, dataAttr) => {
    const node = cy.getElementById(id);
    const label = node.nonempty() ? node.data("name") : `id ${id}`;
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
      <dt>commands</dt><dd>${selectField("command-mode", d.command_mode, ["normal", "timeout", "error"])}</dd>
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
  const data = await res.json();
  if (!data.ok) alert(`${expr}\n\nfailed:\n${data.error}`);
}

async function showComponent(node) {
  const d = node.data();
  clearCharts();

  // Walk the live cytoscape graph for parent/child ids. Cheaper than
  // re-fetching /api/topology, and good enough for the disconnect
  // buttons. Display strings get computed inside renderInspect via
  // cy.getElementById(id).data('name').
  const parentIds = node.incomers("node").map((n) => n.id());
  const childIds = node.outgoers("node").map((n) => n.id());
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
    const plot = makePlot(slot, metric, xs, ys);
    charts.set(metric, { plot, xs, ys });
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
  const opts = {
    width: container.clientWidth || 280,
    height: 140,
    title: METRIC_TITLES[metric] || metric,
    cursor: { drag: { x: false, y: false } },
    legend: { show: false },
    scales: { x: { time: true } },
    axes: [
      { stroke: "#8b949e", grid: { stroke: "#30363d", width: 0.5 } },
      { stroke: "#8b949e", grid: { stroke: "#30363d", width: 0.5 } },
    ],
    series: [
      {},
      { stroke: "#58a6ff", width: 1.5, points: { show: false } },
    ],
  };
  return new uPlot(opts, [xs, ys], container);
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
  series.ys.push(value);
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
    let ev;
    try {
      ev = JSON.parse(msg.data);
    } catch (e) {
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
  const elements = buildElements(topology);
  if (!cy) {
    cy = cytoscape({
      container: document.getElementById("topology"),
      elements,
      style: cytoscapeStylesheet(),
      layout: layoutOptions,
      wheelSensitivity: 0.2,
    });
    cy.on("tap", "node", (evt) => showComponent(evt.target));
    cy.on("tap", (evt) => {
      if (evt.target === cy) clearSide();
    });
    // Hover lift — the .hovered class is picked up by the
    // stylesheet selector to draw a lighter border.
    cy.on("mouseover", "node", (evt) => evt.target.addClass("hovered"));
    cy.on("mouseout",  "node", (evt) => evt.target.removeClass("hovered"));
    // Right-click → delete confirm → eval the removal. The WS
    // TopologyChanged event the eval fires takes care of re-rendering.
    cy.on("cxttap", "node", async (evt) => {
      const node = evt.target;
      const d = node.data();
      if (!confirm(`Delete ${d.name} (id ${d.id})?`)) return;
      const res = await fetch("/api/eval", {
        method: "POST",
        body: `(world-remove-component ${d.id})`,
      });
      const data = await res.json();
      if (!data.ok) alert("Delete failed: " + data.error);
    });
    // Shift+drag from one node to another → world-connect.
    // No live-drawn ghost edge for v1; the new edge appears on
    // release via the WS topology refresh.
    let connectSource = null;
    cy.on("tapstart", "node", (evt) => {
      const e = evt.originalEvent;
      if (!e || !e.shiftKey) return;
      connectSource = evt.target.id();
    });
    cy.on("tapend", async (evt) => {
      if (!connectSource) return;
      const source = connectSource;
      connectSource = null;
      if (evt.target === cy) return; // released over empty canvas
      if (!evt.target.isNode || !evt.target.isNode()) return;
      const target = evt.target.id();
      if (source === target) return; // self-loops disallowed
      const res = await fetch("/api/eval", {
        method: "POST",
        body: `(world-connect ${source} ${target})`,
      });
      const data = await res.json();
      if (!data.ok) alert("Connect failed:\n" + data.error);
    });
  } else {
    // Remember what the user had selected so we can re-highlight it
    // after the rebuild — or clear the side panel if the component
    // got removed.
    const prevSelected = cy.$("node:selected").map((n) => n.id());
    cy.elements().remove();
    cy.add(elements);
    cy.layout(layoutOptions).run();
    if (prevSelected.length) {
      const stillThere = prevSelected.filter((id) => cy.getElementById(id).nonempty());
      if (stillThere.length) {
        const node = cy.getElementById(stillThere[0]);
        node.select();
        showComponent(node);
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
