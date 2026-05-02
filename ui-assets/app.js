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

// Surface a transient toast in the bottom-right. Auto-dismisses after
// ~5s. Use this — not alert() — for action-failure feedback so the
// chrome stays unblocking when the server hiccups during, say, a WS
// reconnect storm. Three places fall outside this rule:
//   * `setStatus` for the persistent connection-state pill (top bar).
//   * `console.error` for diagnostics that only matter in the dev tools.
//   * confirm() prompts that genuinely need a synchronous yes/no.
function notify(message, kind = "error") {
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

// Single-source-of-truth for /api/pending. Three consumers want this
// data (persist pill, pending dialog, inspector overrides section)
// and they all want to refresh after the same triggers (WS
// TopologyChanged, the various × / persist / discard handlers).
// Centralizing avoids 3-fetch fan-out per WS tick and lets dialog +
// inspector render off the same snapshot.
const pendingState = (() => {
  let snapshot = { entries: [], persisted: [], persisted_count: 0 };
  const subs = new Set();
  let inflight = null;
  async function refresh() {
    if (inflight) return inflight;
    inflight = (async () => {
      try {
        const res = await fetch("/api/pending");
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
      li.className = "sp-event " + (ev.accepted ? "accepted" : "rejected");
      const ts = new Date(ev.ts_ms).toLocaleTimeString();
      // The WS event carries the setpoint kind on `setpoint_kind`
      // to dodge collision with the WorldEvent discriminator (also
      // called `kind`).
      const tag = ev.setpoint_kind.replace("_", " ");
      const head = `<span class="sp-ts">${ts}</span> <span class="sp-tag">${tag}</span> <span class="sp-val">${ev.value}</span>`;
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

// vis-network instance + DataSets + last-known component table.
// All accessors that surrounding code needs go through the module's
// public API, so callers don't have to reach into vis-network or
// reconstruct the id → component map themselves. Selection is wired
// up via setSelectionHandler so applyTopology doesn't need to know
// about showComponent / clearSide directly.
const topology = (() => {
  let network = null;
  let nodesDS = null;
  let edgesDS = null;
  const componentById = new Map();
  let onSelect = null;
  let onDeselect = null;

  function buildVisData(data) {
    componentById.clear();
    const visible = data.components.filter((c) => !c.hidden);
    const nodes = visible.map((c) => {
      componentById.set(c.id, c);
      return nodeStyleFor(c);
    });
    const edges = data.connections.map(([p, c]) => ({
      id: `${p}-${c}`,
      from: p,
      to: c,
      arrows: "to",
    }));
    return { nodes, edges };
  }

  function apply(data) {
    const visibleCount = data.components.filter((c) => !c.hidden).length;
    setStatus(
      `${visibleCount} components, ${data.connections.length} connections`,
      "connected",
    );
    const { nodes, edges } = buildVisData(data);
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
          if (onSelect) onSelect(componentById.get(id));
        } else if (onDeselect) {
          onDeselect();
        }
      });
      // Right-click → context menu. Selection acts as the target:
      //   selection non-empty → Copy, Delete (and Cut)
      //   selection empty + clipboard non-empty → Paste
      // Right-clicking a node *not* in the current selection resets
      // the selection to that one node first, matching the standard
      // editor convention.
      network.on("oncontext", (params) => {
        params.event.preventDefault();
        const nodeAt = network.getNodeAt(params.pointer.DOM);
        if (nodeAt != null) {
          const sel = network.getSelectedNodes();
          if (!sel.includes(nodeAt)) {
            network.selectNodes([nodeAt]);
            const c = componentById.get(nodeAt);
            if (c && onSelect) onSelect(c);
          }
        }
        showContextMenu(params.event.clientX, params.event.clientY);
      });
      // Shift toggles vis-network's addEdge mode. Hold Shift, drag
      // from one node to another to wire them. The addEdge callback
      // (defined in visOptions) POSTs world-connect and the WS
      // topology refresh redraws.
      document.addEventListener("keydown", (e) => {
        if (e.key === "Shift" && network) network.addEdgeMode();
      });
      document.addEventListener("keyup", (e) => {
        if (e.key === "Shift" && network) network.disableEditMode();
      });
    } else {
      // Diff the DataSets — preserves selection, layout positions,
      // and any in-flight drag interactions, instead of tearing
      // down the canvas on every WS topology event.
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
          if (onSelect) onSelect(componentById.get(stillThere[0]));
        } else if (onDeselect) {
          onDeselect();
        }
      }
    }
    // vis-network's hierarchical layout is level-stable but
    // within-level it appends new nodes to the bottom of their
    // level's slot list (DataSet insertion order). Run a barycenter
    // pass to pull each node toward its neighbours' average y so
    // the duplicated battery sits next to its inverter rather than
    // dropping to the bottom of the canvas.
    rearrangeVerticallyForShortArrows();
  }

  // Sugiyama-style barycenter sweeps over the level layers: each
  // pass reorders one level's nodes by the mean y of their
  // neighbours in an *adjacent* layer (down-sweep looks at the
  // previous layer, up-sweep at the next), and a final all-neighbour
  // pass smooths out anything the directional sweeps couldn't reach.
  // Reassignment is a within-level permutation onto the level's own
  // existing y-slots, so the layer's vertical extent and spacing
  // stay put — only ordering within a layer changes. Effect: arrows
  // shorten and crossings drop.
  function rearrangeVerticallyForShortArrows() {
    if (!network || !edgesDS || !nodesDS) return;
    const positions = network.getPositions();
    const ids = Object.keys(positions);
    if (ids.length <= 1) return;

    // Group ids by level (rounded x), and remember the level of each
    // node so the per-direction filter is O(1).
    const levelOf = new Map();
    const levels = new Map();
    for (const id of ids) {
      const lvl = Math.round(positions[id].x);
      levelOf.set(id, lvl);
      if (!levels.has(lvl)) levels.set(lvl, []);
      levels.get(lvl).push(id);
    }
    const sortedLevels = [...levels.keys()].sort((a, b) => a - b);

    // Two adjacency maps: predecessors (smaller-x neighbour) and
    // successors (larger-x neighbour). Edges in vis are directed but
    // a node sits between its parent and its children regardless of
    // the arrow direction, so we key by level comparison rather than
    // edge direction.
    const preds = new Map();
    const succs = new Map();
    for (const id of ids) {
      preds.set(id, []);
      succs.set(id, []);
    }
    for (const e of edgesDS.get()) {
      const f = String(e.from);
      const t = String(e.to);
      if (!preds.has(f) || !preds.has(t)) continue;
      const lf = levelOf.get(f);
      const lt = levelOf.get(t);
      if (lf < lt) {
        succs.get(f).push(t);
        preds.get(t).push(f);
      } else if (lf > lt) {
        succs.get(t).push(f);
        preds.get(f).push(t);
      } else {
        // Same-level edge: treat as both predecessor and successor
        // so it pulls in the all-neighbours smoothing pass below.
        succs.get(f).push(t);
        succs.get(t).push(f);
      }
    }

    function reorder(levelIds, neighborMap) {
      if (levelIds.length <= 1) return false;
      const desired = levelIds.map((id) => {
        const ns = neighborMap.get(id);
        if (!ns.length) return positions[id].y;
        let sum = 0;
        for (const n of ns) sum += positions[n].y;
        return sum / ns.length;
      });
      const slotYs = levelIds
        .map((id) => positions[id].y)
        .sort((a, b) => a - b);
      const order = levelIds
        .map((_, i) => i)
        .sort((a, b) => desired[a] - desired[b]);
      let moved = false;
      for (let slot = 0; slot < order.length; slot++) {
        const id = levelIds[order[slot]];
        const newY = slotYs[slot];
        if (Math.abs(positions[id].y - newY) > 0.5) {
          positions[id].y = newY;
          moved = true;
        }
      }
      return moved;
    }

    const ITERATIONS = 24;
    for (let iter = 0; iter < ITERATIONS; iter++) {
      let moved = false;
      // Down-sweep: roots stay put, each subsequent level orders
      // by predecessor barycenter.
      for (let i = 1; i < sortedLevels.length; i++) {
        if (reorder(levels.get(sortedLevels[i]), preds)) moved = true;
      }
      // Up-sweep: leaves stay put, each predecessor level orders
      // by successor barycenter.
      for (let i = sortedLevels.length - 2; i >= 0; i--) {
        if (reorder(levels.get(sortedLevels[i]), succs)) moved = true;
      }
      if (!moved) break;
    }

    for (const id of ids) {
      network.moveNode(id, positions[id].x, positions[id].y);
    }
  }

  return {
    apply,
    get: (id) => componentById.get(id),
    has: (id) => componentById.has(id),
    parentsOf: (id) => (network ? network.getConnectedNodes(id, "from") : []),
    childrenOf: (id) => (network ? network.getConnectedNodes(id, "to") : []),
    selectedIds: () => (network ? network.getSelectedNodes() : []),
    /// Array of every visible node id, in registration order. Reads
    /// the inspector's lookup table so hidden nodes (e.g. internal
    /// load meters) stay out — same filter applied at apply()-time.
    allIds: () => Array.from(componentById.keys()),
    select(ids) {
      if (network) network.selectNodes(ids);
    },
    /// Array of [from, to] edges as rendered. Source-of-truth is the
    /// vis DataSet so this reflects whatever the canvas is actually
    /// showing (post-diff during incremental refreshes).
    connections() {
      if (!edgesDS) return [];
      return edgesDS.get().map((e) => [e.from, e.to]);
    },
    setSelectionHandler(select, deselect) {
      onSelect = select;
      onDeselect = deselect;
    },
  };
})();

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
      // Group by *minimum* depth from a root, not maximum. Default
      // `"leaves"` shoves leaf nodes to the rightmost column, so
      // shorter chains (eg. pv-meter → pv-inverter, with no battery
      // under the inverter) end up sharing a column with longer
      // chains' interior nodes — pv_meter lands next to bat_inverter.
      // `"roots"` keeps each role in its own column: meters at L1,
      // inverters at L2, batteries at L3.
      shakeTowards: "roots",
      nodeSpacing: 120,
      levelSeparation: 180,
      treeSpacing: 140,
    },
  },
  physics: { enabled: false },
  interaction: {
    hover: true,
    dragNodes: true,
    // Ctrl/Cmd-click toggles a node into the existing selection, and
    // a drag on empty canvas rubber-bands a multi-selection — both
    // back the Cmd+D duplicate-selected flow.
    multiselect: true,
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
          if (!res.ok) notify("Connect failed: " + res.error);
        });
      // Don't apply locally — the WS topology refresh will redraw
      // with the new edge once the eval lands on the server.
      callback(null);
    },
  },
};


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
const KNOBS_BY_CATEGORY = {
  meter: [{ label: "power override (W)", defun: "set-meter-power" }],
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
    knobs.unshift({ label: "sunlight (%)", defun: "set-solar-sunlight" });
  }
  return knobs;
}

function renderInspect(d, parentIds, childIds, overrides = []) {
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
        .map(
          (k) => `<dt>${escapeHtml(k.label)}</dt><dd>
            <input type="number" step="any" class="knob-input"
                   data-defun="${k.defun}" placeholder="value" />
          </dd>`,
        )
        .join("")}</dl>`;
    })()}
    ${overrides.length
      ? `<h3>Current overrides</h3>
         <ul class="overrides-list">${overrides
           .map(
             (e) => `<li>
               <pre>${escapeHtml(e.source)}</pre>
               <button class="link-btn" data-undo="${e.id}" title="Remove this override">✕</button>
             </li>`,
           )
           .join("")}</ul>`
      : ""}
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
    evalQuoted(`(world-rename-component ${d.id} "${jsToLispString(name)}")`, {
      affects: d.id,
    });
  });
  for (const [key, defun] of [
    ["health", "set-component-health"],
    ["telemetry-mode", "set-component-telemetry-mode"],
    ["command-mode", "set-component-command-mode"],
  ]) {
    const sel = inspectEl.querySelector(`select[data-knob="${key}"]`);
    if (!sel) continue; // dropdown hidden for this category
    sel.addEventListener("change", (e) => {
      evalQuoted(`(${defun} ${d.id} '${e.target.value})`, { affects: d.id });
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
      evalQuoted(`(${e.target.dataset.defun} ${d.id} ${v})`, { affects: d.id });
      e.target.value = "";
    });
  }
  // Override removal: DELETE /api/pending/<id>. The server replays
  // remaining edits + bumps world-version; the WS topology event
  // re-runs showComponent, which re-fetches /api/pending and the
  // override drops out of the list.
  for (const btn of inspectEl.querySelectorAll("[data-undo]")) {
    btn.addEventListener("click", async () => {
      const id = btn.dataset.undo;
      const res = await fetch(`/api/pending/${id}`, { method: "DELETE" });
      if (res.ok) {
        pendingState.refresh();
      } else {
        notify(`Remove failed: ${res.status} ${await res.text()}`);
      }
    });
  }
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-from]")) {
    btn.addEventListener("click", () =>
      evalQuoted(
        `(world-disconnect ${btn.dataset.disconnectFrom} ${d.id})`,
        { affects: d.id },
      ),
    );
  }
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-to]")) {
    btn.addEventListener("click", () =>
      evalQuoted(
        `(world-disconnect ${d.id} ${btn.dataset.disconnectTo})`,
        { affects: d.id },
      ),
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

async function evalQuoted(expr, opts = {}) {
  // `affects: <id>` tags the resulting pending entry so the
  // inspector can show "current overrides on component X" without
  // parsing the source string. Untagged evals (REPL, defaults
  // editor) just go without the query param.
  const url =
    "/api/eval" + (opts.affects != null ? `?affects=${opts.affects}` : "");
  const res = await fetch(url, { method: "POST", body: expr });
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
    notify(`${expr}: server returned non-JSON (HTTP ${res.status})`);
    return;
  }
  if (!data.ok) notify(`${expr}: ${data.error}`);
}

async function showComponent(d) {
  if (!d) return;
  liveCharts.clear();

  // vis-network's getConnectedNodes(id, direction) returns the
  // ids on either side of the selected node — cheaper than walking
  // /api/topology for the disconnect buttons. Display labels get
  // resolved by renderInspect via topology.get().
  const parentIds = topology.parentsOf(d.id);
  const childIds = topology.childrenOf(d.id);
  // Pending entries that target this component — shown as
  // "Current overrides" with their own ✕ buttons. Read from the
  // shared pendingState cache so we don't fan out a fresh fetch
  // on every selection click.
  const overrides = pendingState
    .get()
    .entries.filter((e) => e.affects === d.id);
  renderInspect(d, parentIds, childIds, overrides);

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

function clearSide() {
  liveCharts.clear();
  inspectEl.innerHTML =
    '<p class="hint">Click a node to inspect. Right-click for the context menu.</p>';
}

// Map from a topology component to its public Lisp constructor.
// Inverters split on subtype ("battery" / "solar"); everything else
// keys off category. Returns null for categories we don't know how
// to clone (e.g. an unrecognised proto-derived kind).
function makeFnFor(c) {
  if (c.category === "inverter") {
    return c.subtype === "solar" ? "make-solar-inverter" : "make-battery-inverter";
  }
  return {
    grid: "make-grid",
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

function snapshotSelection(selectedIds) {
  const components = selectedIds
    .map((id) => topology.get(id))
    .filter(Boolean)
    .map(({ id, category, subtype }) => ({ id, category, subtype }));
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
    .map((c) => `(m${c.id} (${makeFnFor(c)}))`)
    .join(" ");
  const reconnects = snap.edges
    .map(([from, to]) => `(world-connect (component-id m${from}) (component-id m${to}))`)
    .join(" ");
  const src = reconnects
    ? `(let* (${bindings}) ${reconnects})`
    : `(let* (${bindings}) t)`;
  const res = await fetch("/api/eval", { method: "POST", body: src });
  const data = await res.json();
  if (!data.ok) notify(`Paste failed: ${data.error}`);
}

async function deleteSelection() {
  const ids = topology.selectedIds();
  if (!ids.length) {
    notify("Nothing selected to delete.");
    return;
  }
  const removes = ids.map((id) => `(world-remove-component ${id})`).join(" ");
  const src = `(progn ${removes})`;
  const res = await fetch("/api/eval", { method: "POST", body: src });
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
function showContextMenu(x, y) {
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
      const res = await fetch("/api/eval", {
        method: "POST",
        body: `(${fn})`,
      });
      const data = await res.json();
      if (!data.ok) notify("Create failed: " + data.error);
    } finally {
      btn.disabled = false;
    }
  });
}

function escapeHtml(s) {
  return String(s).replace(/[<>&]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;" })[c]);
}

async function showPendingDialog() {
  const dlg = document.getElementById("pending-dialog");
  const content = document.getElementById("pending-dialog-content");
  // Subscribe to live updates so any × or restore re-renders the
  // dialog automatically without each handler having to explicitly
  // call renderPendingDialog. Unsubscribe on close to stop pinging
  // the host element after it's hidden.
  const unsubscribe = pendingState.subscribe((data) =>
    renderPendingDialog(content, data),
  );
  dlg.addEventListener("close", () => unsubscribe(), { once: true });
  dlg.showModal();
  // Refresh once on open so the subscriber sees the latest snapshot.
  pendingState.refresh();
}

function renderPendingDialog(content, data) {
  const sections = [];
  if (data.persisted && data.persisted.length) {
    const rows = data.persisted
      .map((o) => {
        const cls = o.marked_removal
          ? "pending-entry persisted marked-removal"
          : "pending-entry persisted";
        const action = o.marked_removal
          ? `<button class="link-btn persisted-restore" data-idx="${o.idx}" title="Undo removal">⟲</button>`
          : `<button class="link-btn persisted-del" data-idx="${o.idx}" title="Mark for removal on next persist">✕</button>`;
        return `<div class="${cls}">
          <div class="pending-num">#${o.idx + 1}</div>
          <pre>${escapeHtml(o.source)}</pre>
          ${action}
        </div>`;
      })
      .join("");
    sections.push(`<h3>On disk</h3>${rows}`);
  }
  if (data.entries.length) {
    const rows = data.entries
      .map(
        (e, i) =>
          `<div class="pending-entry">
            <div class="pending-num">#${i + 1}</div>
            <pre>${escapeHtml(e.source)}</pre>
            <button class="link-btn pending-del" data-id="${e.id}" title="Remove this edit">✕</button>
          </div>`,
      )
      .join("");
    sections.push(`<h3>Unsaved</h3>${rows}`);
  }
  if (!sections.length) {
    content.innerHTML = '<p class="hint">no active overrides</p>';
    return;
  }
  content.innerHTML = sections.join("");
  for (const btn of content.querySelectorAll(".pending-del")) {
    btn.addEventListener("click", async () => {
      const id = btn.dataset.id;
      const res = await fetch(`/api/pending/${id}`, { method: "DELETE" });
      if (res.ok) {
        pendingState.refresh();
      } else {
        notify(`Remove failed: ${res.status} ${await res.text()}`);
      }
    });
  }
  for (const btn of content.querySelectorAll(".persisted-del")) {
    btn.addEventListener("click", async () => {
      const idx = btn.dataset.idx;
      const res = await fetch(`/api/persisted/${idx}`, { method: "DELETE" });
      if (res.ok) {
        pendingState.refresh();
      } else {
        notify(`Mark failed: ${res.status} ${await res.text()}`);
      }
    });
  }
  for (const btn of content.querySelectorAll(".persisted-restore")) {
    btn.addEventListener("click", async () => {
      const idx = btn.dataset.idx;
      const res = await fetch(`/api/persisted/${idx}`, { method: "POST" });
      if (res.ok) {
        pendingState.refresh();
      } else {
        notify(`Restore failed: ${res.status} ${await res.text()}`);
      }
    });
  }
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

function setupPendingDialog() {
  const dlg = document.getElementById("pending-dialog");
  document
    .getElementById("pending-dialog-close")
    .addEventListener("click", () => dlg.close());
  // Click on the backdrop (target === dialog itself, not inner card)
  // closes the dialog. Keeps click-outside-to-dismiss working.
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg) dlg.close();
  });
}

function setupPersistControls() {
  const pill = document.getElementById("pending-pill");
  pill.addEventListener("click", showPendingDialog);
  const dirty = document.getElementById("pending-dirty");
  const count = document.getElementById("pending-count");
  const persistBtn = document.getElementById("persist-btn");
  const discardBtn = document.getElementById("discard-btn");

  // Pill shows total override count + a `*` (editor-style modified
  // marker) when any are unsaved. Hidden when neither.
  pendingState.subscribe((data) => {
    const pending = data.entries.length;
    const persisted = data.persisted || [];
    const removals = persisted.filter((o) => o.marked_removal).length;
    const total = pending + persisted.length;
    const unsaved = pending > 0 || removals > 0;
    count.textContent = total;
    dirty.textContent = unsaved ? "*" : "";
    pill.hidden = total === 0;
    persistBtn.disabled = !unsaved;
    discardBtn.disabled = !unsaved;
  });

  persistBtn.addEventListener("click", async () => {
    persistBtn.disabled = true;
    const res = await fetch("/api/persist", { method: "POST" });
    const data = await res.json();
    if (res.ok) {
      notify(`Persisted to ${data.path} (${data.persisted} forms)`, "success");
    } else {
      notify(`Persist failed: ${data.error || res.status}`);
    }
    pendingState.refresh();
  });

  discardBtn.addEventListener("click", async () => {
    if (!confirm("Discard all unsaved edits and reload?")) return;
    discardBtn.disabled = true;
    await fetch("/api/discard", { method: "POST" });
    // Discard triggers a server-side reload which fires
    // TopologyChanged on the WS — that handler refreshes too.
    pendingState.refresh();
  });
}

/// Generic side-panel toggle: a chrome button that swaps the
/// inspect+add-form view for some custom render. Clicking the
/// button while open restores the default inspect view (and
/// re-shows the add-form, which got hidden during render).
function makeSidePanelToggle(btnId, render) {
  const btn = document.getElementById(btnId);
  let open = false;
  btn.addEventListener("click", async () => {
    open = !open;
    btn.classList.toggle("primary", open);
    if (open) {
      await render();
    } else {
      clearSide();
      document.getElementById("add-form").style.display = "";
    }
  });
}

// Both side-panel toggles use the same chrome-button + swap-side-
// panel pattern. The render functions below own the actual content.
const setupDefaultsToggle = () => makeSidePanelToggle("defaults-btn", renderDefaults);
const setupScenariosToggle = () => makeSidePanelToggle("scenarios-btn", renderScenarios);

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
      notify(`Save failed: ${await r.text()}`);
    }
  });
  for (const btn of inspectEl.querySelectorAll("[data-load]")) {
    btn.addEventListener("click", async () => {
      const name = btn.dataset.load;
      const r = await fetch(
        `/api/scenarios/load?name=${encodeURIComponent(name)}`,
        { method: "POST" },
      );
      if (!r.ok) notify(`Load failed: ${await r.text()}`);
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

/// Generic drag-to-resize handler. Both splitters in the chrome
/// (vertical between topology + side panel, horizontal between
/// topology row + drawer) follow the same pattern: capture the
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

/// Vertical splitter between topology canvas and side panel.
/// Updates main's grid-template-columns to resize the third (side)
/// column.
function setupSplitter() {
  const main = document.getElementById("app");
  const sideEl = document.getElementById("side");
  const SIDE_MIN = 300; // anything narrower and the inspect form wraps badly
  const SIDE_MAX_FRAC = 0.7; // don't let the canvas drop below 30% of width
  makeSplitter({
    axis: "x",
    splitter: document.getElementById("splitter"),
    getStart: () => sideEl.getBoundingClientRect().width,
    apply: (w) => {
      main.style.gridTemplateColumns = `1fr 5px ${w}px`;
    },
    clamp: (w, vw) => Math.min(vw * SIDE_MAX_FRAC, Math.max(SIDE_MIN, w)),
  });
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
      main.style.gridTemplateRows = `1fr 5px ${h}px`;
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
  el.className = "log-line " + (ev.level || "info").toLowerCase();
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
const COMPLETIONS = [
  // World mutations
  "world-connect",
  "world-disconnect",
  "world-remove-component",
  "world-rename-component",
  "world-reset",
  // Make-* primitives
  "%make-grid",
  "%make-meter",
  "%make-battery",
  "%make-battery-inverter",
  "%make-solar-inverter",
  "%make-ev-charger",
  "%make-chp",
  // Make-* lisp wrappers
  "make-grid",
  "make-meter",
  "make-battery",
  "make-battery-inverter",
  "make-solar-inverter",
  "make-ev-charger",
  "make-chp",
  // Setters
  "set-component-health",
  "set-component-telemetry-mode",
  "set-component-command-mode",
  "set-active-power",
  "set-meter-power",
  "set-solar-sunlight",
  "set-reactive-pf-limit",
  "set-reactive-apparent-va",
  "set-physics-tick-ms",
  "set-voltage-per-phase",
  "set-frequency",
  // Metadata
  "set-microgrid-id",
  "set-enterprise-id",
  "set-microgrid-name",
  "set-socket-addr",
  "set-default-request-lifetime-ms",
  "get-microgrid-id",
  // Utilities
  "every",
  "run-with-timer",
  "cancel-timer",
  "sleep-for",
  "now-seconds",
  "window-elapsed",
  "load",
  "load-overrides",
  "watch-file",
  "file-exists-p",
  "reset-state",
  "log.info",
  "log.warn",
  "log.error",
  "log.debug",
  "log.trace",
  "ceiling",
  "floor",
  "random",
  "csv-load",
  "csv-lookup",
  "csv-fields",
  "component-id",
  // Per-category defaults variables
  "grid-defaults",
  "meter-defaults",
  "battery-defaults",
  "battery-inverter-defaults",
  "solar-inverter-defaults",
  "ev-charger-defaults",
  "chp-defaults",
  // Common Lisp built-ins
  "defun",
  "defmacro",
  "setq",
  "let",
  "let*",
  "if",
  "when",
  "unless",
  "cond",
  "lambda",
  "progn",
  "quote",
  "list",
  "cons",
  "car",
  "cdr",
  "nth",
  "length",
  "append",
  "reverse",
  "mapcar",
  "dolist",
  "dotimes",
  "while",
  "and",
  "or",
  "not",
  "eq",
  "equal",
  "format",
  "concat",
  "intern",
  "symbol-value",
  "plist-get",
  "alist-get",
  "assoc",
  "boundp",
  "fboundp",
  "null",
  "consp",
  "listp",
  "stringp",
  "numberp",
  "symbolp",
];

function wordAtCursor(input) {
  const v = input.value;
  const c = input.selectionStart;
  let start = c;
  // Lisp identifiers: alnum + - _ % . :
  while (start > 0 && /[a-zA-Z0-9_%\-.:]/.test(v[start - 1])) start--;
  return { prefix: v.slice(start, c), start, end: c };
}

function setupRepl() {
  const form = document.getElementById("repl-form");
  const input = document.getElementById("repl-input");
  const output = document.getElementById("repl-output");
  const completions = document.getElementById("repl-completions");
  let selectedIdx = 0;
  let active = []; // current list of candidates

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
  input.addEventListener("input", refresh);
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
      liveCharts.pushSample(ev.id, ev.metric, ev.ts_ms, ev.value);
    } else if (ev.kind === "topology_changed") {
      onTopologyChanged(ev.version);
    } else if (ev.kind === "setpoint") {
      liveCharts.pushSetpoint(ev);
    } else if (ev.kind === "log") {
      appendLog(ev);
    }
  };
  ws.onclose = () => setStatus("disconnected", "error");
  ws.onerror = () => setStatus("ws error", "error");
  return ws;
}

async function refreshTopology() {
  try {
    const res = await fetch("/api/topology");
    if (!res.ok) throw new Error("HTTP " + res.status);
    topology.apply(await res.json());
  } catch (err) {
    setStatus("error: " + err.message, "error");
  }
}

async function init() {
  setupAddForm();
  setupDefaultsToggle();
  setupScenariosToggle();
  setupSplitter();
  setupDrawerSplitter();
  setupPendingDialog();
  backfillLogs();
  setupPersistControls();
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
    if (meta && key === "c") {
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
      // Topology's own click handler clears the side panel; mirror
      // that here for keyboard parity.
      topology.select([]);
      clearSide();
    }
  });
  setupContextMenu();
  setupHelpButton();
  await refreshTopology();
  await pendingState.refresh();
  // WS push: refresh both the topology (so the canvas reflects the
  // mutation) and the pending state (so the pill, dialog, and
  // inspector all update) on every TopologyChanged. Sample events
  // go straight into the live-charts router.
  openWebSocket((_v) => {
    refreshTopology();
    pendingState.refresh();
  });
  setupRepl();
}

init();
