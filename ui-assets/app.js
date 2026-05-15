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
  status.className = `status ${klass || ""}`;
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

const CATEGORY_COLOR = {
  grid: getCss("--cat-grid"),
  meter: getCss("--cat-meter"),
  inverter: getCss("--cat-inverter"),
  battery: getCss("--cat-battery"),
  "ev-charger": getCss("--cat-ev-charger"),
  chp: getCss("--cat-chp"),
};

// Inverters get a subtype-aware shade so battery-inverters and
// solar-inverters read as related-but-distinct on the canvas.
const INVERTER_SUBTYPE_COLOR = {
  battery: getCss("--cat-inverter-battery"),
  solar: getCss("--cat-inverter-solar"),
};

function colorFor(c) {
  if (c.category === "inverter") {
    return INVERTER_SUBTYPE_COLOR[c.subtype] || CATEGORY_COLOR.inverter;
  }
  return CATEGORY_COLOR[c.category] || "#888";
}

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
    if (max >= 1e6) return { div: 1e6, unit: `M${rule.baseUnit}` };
    if (max >= 1e3) return { div: 1e3, unit: `k${rule.baseUnit}` };
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
      li.className = `sp-event ${ev.accepted ? "accepted" : "rejected"}`;
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
  return `#${((r << 16) | (g << 8) | b).toString(16).padStart(6, "0")}`;
}

function nodeStyleFor(c) {
  const healthBorder = {
    ok: "#1c2128",     // matches --bg — subtle outline at rest
    standby: "#c4ad55", // toned-down yellow
    error: "#e58275",   // toned-down red, matches --bad
  }[c.health || "ok"];
  // Hidden meters draw with a dashed border + a thicker stroke so
  // the dash pattern reads cleanly. Health-error / standby still
  // win the colour since "this is faulted" is more urgent than
  // "this is hidden". borderDashes accepts an [on, off] array.
  const healthWidth = c.health === "ok" ? (c.hidden ? 2 : 1) : 3;
  const bg = colorFor(c);
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
    borderDashes: c.hidden ? [4, 3] : false,
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
    // Tight vertical padding because the ellipse already adds its
    // own √2-ish inflation to fit the inscribed text rectangle —
    // any extra top/bottom margin compounds and produces fat ovals
    // that overlap at our nodeSpacing of 60.
    margin: { top: 4, right: 16, bottom: 4, left: 16 },
    // Width floor keeps short-label nodes (grid-1, meter-2) from
    // shrinking below the readable threshold. Height floor stays
    // small — long labels grow on their own; we don't need to
    // pad short-label heights.
    widthConstraint: { minimum: 78 },
    heightConstraint: { minimum: 24 },
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
  let selectionAtMousedown = [];

  function buildVisData(data) {
    componentById.clear();
    const nodes = data.components.map((c) => {
      componentById.set(c.id, c);
      return nodeStyleFor(c);
    });
    const visibleEdges = data.connections.map(([p, c]) => ({
      id: `${p}-${c}`,
      from: p,
      to: c,
      arrows: "to",
    }));
    // Hidden edges (parent → hidden child) render dashed so the
    // user can see the link without confusing them with the public
    // gRPC topology — same visual cue the hidden node itself uses.
    const hiddenEdges = (data.hidden_connections || []).map(([p, c]) => ({
      id: `${p}-${c}`,
      from: p,
      to: c,
      arrows: "to",
      dashes: true,
    }));
    return { nodes, edges: [...visibleEdges, ...hiddenEdges] };
  }

  function apply(data) {
    // The chrome status pill keeps showing the gRPC-visible count,
    // which is what most operators care about when reasoning about
    // their topology. Hidden meters render on the canvas (dashed)
    // for context but don't bump the official tally.
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
        const shiftKey = params.event?.srcEvent?.shiftKey;
        if (params.nodes.length) {
          const id = params.nodes[0];
          if (shiftKey) {
            // Shift-click toggles this node in / out of the
            // selection that existed when the mousedown landed.
            // Reading getSelectedNodes() here would see vis-network's
            // single-click auto-select that already ran for this
            // event, so we use the mousedown snapshot instead.
            const sel = new Set(selectionAtMousedown);
            if (sel.has(id)) sel.delete(id);
            else sel.add(id);
            const ids = [...sel];
            network.selectNodes(ids);
            if (ids.length && onSelect) onSelect(componentById.get(id));
            else if (!ids.length && onDeselect) onDeselect();
          } else if (onSelect) {
            onSelect(componentById.get(id));
          }
        } else if (!shiftKey && onDeselect) {
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
      // Ctrl/Cmd toggles vis-network's addEdge mode. Hold Ctrl
      // (Cmd on Mac), drag from one node to another to wire them.
      // The addEdge callback (defined in visOptions) POSTs
      // world-connect and the WS topology refresh redraws.
      document.addEventListener("keydown", (e) => {
        if ((e.key === "Control" || e.key === "Meta") && network) {
          network.addEdgeMode();
        }
      });
      document.addEventListener("keyup", (e) => {
        if ((e.key === "Control" || e.key === "Meta") && network) {
          network.disableEditMode();
        }
      });
      // Capture the selection state at mousedown — vis-network's
      // single-click selection runs before our `click` handler, so
      // by the time we read getSelectedNodes() it's already been
      // overwritten. Snap it here and the alt-click toggle in the
      // click handler can compute against the pre-click set.
      document
        .getElementById("topology")
        .addEventListener(
          "mousedown",
          () => {
            selectionAtMousedown = network ? network.getSelectedNodes() : [];
          },
          true,
        );
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

  // Pull each node's y toward its neighbours' barycenter, then push
  // sibling nodes apart by `MIN_SPACING` if the barycenter put them
  // on top of each other. Sweeps top-down (each level's y comes from
  // its predecessors' y) and bottom-up (parents pull toward their
  // children's centroid). Letting the level's vertical extent grow
  // when needed — instead of permuting onto vis-network's
  // count-dependent slot grid — keeps an L_n+1 child at the same y
  // as its L_n parent, so a chain renders as a horizontal line
  // regardless of how many siblings sit at each level.
  function rearrangeVerticallyForShortArrows() {
    if (!network || !edgesDS || !nodesDS) return;
    const positions = network.getPositions();
    const ids = Object.keys(positions);
    if (ids.length <= 1) return;

    const levelOf = new Map();
    const levels = new Map();
    for (const id of ids) {
      const lvl = Math.round(positions[id].x);
      levelOf.set(id, lvl);
      if (!levels.has(lvl)) levels.set(lvl, []);
      levels.get(lvl).push(id);
    }
    const _sortedLevels = [...levels.keys()].sort((a, b) => a - b);

    // Two adjacency maps split by level direction. Same-level edges
    // are ignored — they don't tell us anything about top-down or
    // bottom-up alignment.
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
      }
    }

    // Matches the visOptions `nodeSpacing` so siblings respect the
    // same gap vis-network would have used in its initial layout.
    const MIN_SPACING = 60;

    function snap(levelIds, neighborMap) {
      // Move each node toward the mean y of its neighbours in the
      // chosen direction; nodes with zero neighbours keep their
      // current y.
      for (const id of levelIds) {
        const ns = neighborMap.get(id);
        if (!ns.length) continue;
        let sum = 0;
        for (const n of ns) sum += positions[n].y;
        positions[id].y = sum / ns.length;
      }
      if (levelIds.length <= 1) return;
      // Sort by current y, then enforce MIN_SPACING as a *floor*
      // by pushing each node down only if it would overlap the one
      // above. Nodes already further apart keep their separation
      // — that's how an L3 child stays aligned with its L2 parent
      // when siblings live at distant y's.
      levelIds.sort((a, b) => positions[a].y - positions[b].y);
      const before =
        levelIds.reduce((s, id) => s + positions[id].y, 0) / levelIds.length;
      for (let i = 1; i < levelIds.length; i++) {
        const prev = positions[levelIds[i - 1]].y;
        if (positions[levelIds[i]].y - prev < MIN_SPACING) {
          positions[levelIds[i]].y = prev + MIN_SPACING;
        }
      }
      // Pushing-down skews the cluster toward the bottom; recentre
      // on the pre-resolve mean so the level's barycenter doesn't
      // drift across iterations.
      const after =
        levelIds.reduce((s, id) => s + positions[id].y, 0) / levelIds.length;
      const shift = before - after;
      if (shift !== 0) {
        for (const id of levelIds) positions[id].y += shift;
      }
    }

    // Hidden components sit out of the barycenter — they bias the
    // visible layout toward themselves otherwise (a hidden meter
    // pulled into L1 would consume a slot and shift its visible
    // siblings to make room). We snap them to a row underneath the
    // visible canvas after the sweeps converge.
    const hiddenIds = ids.filter((id) => {
      const c = componentById.get(Number(id));
      return c?.hidden;
    });
    const visibleLevels = new Map();
    for (const [lvl, lvlIds] of levels) {
      const visibleAtLvl = lvlIds.filter((id) => {
        const c = componentById.get(Number(id));
        return c && !c.hidden;
      });
      if (visibleAtLvl.length) visibleLevels.set(lvl, visibleAtLvl);
    }
    const visibleSortedLevels = [...visibleLevels.keys()].sort((a, b) => a - b);

    const ITERATIONS = 12;
    for (let iter = 0; iter < ITERATIONS; iter++) {
      const before = ids.map((id) => positions[id].y);
      // Down-sweep: align each level with its predecessors.
      for (let i = 1; i < visibleSortedLevels.length; i++) {
        snap(visibleLevels.get(visibleSortedLevels[i]).slice(), preds);
      }
      // Up-sweep: pull predecessor levels toward their children's
      // centroid. Helps when an L_n node has multiple children at
      // L_n+1 with different y's — the parent re-centres on them.
      for (let i = visibleSortedLevels.length - 2; i >= 0; i--) {
        snap(visibleLevels.get(visibleSortedLevels[i]).slice(), succs);
      }
      const after = ids.map((id) => positions[id].y);
      if (before.every((y, i) => Math.abs(y - after[i]) < 0.5)) break;
    }

    if (hiddenIds.length) {
      // Stash hidden nodes in a row directly below the lowest
      // visible node. Each keeps its natural x (so the dashed
      // edge to its parent reads top-down), but they all share a
      // y — separated only when two hidden nodes happen to share
      // an x, in which case we stack them on min-spacing.
      const visibleIds = ids.filter((id) => !hiddenIds.includes(id));
      const maxVisibleY = visibleIds.length
        ? Math.max(...visibleIds.map((id) => positions[id].y))
        : 0;
      const baseY = maxVisibleY + MIN_SPACING * 2;
      const byX = new Map();
      for (const id of hiddenIds) {
        const x = Math.round(positions[id].x);
        if (!byX.has(x)) byX.set(x, []);
        byX.get(x).push(id);
      }
      for (const group of byX.values()) {
        group.sort((a, b) => positions[a].y - positions[b].y);
        for (let i = 0; i < group.length; i++) {
          positions[group[i]].y = baseY + i * MIN_SPACING;
        }
      }
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
      // Center-to-center within-level (vertical for LR). Tight
      // enough to fit denser graphs on one screen while leaving
      // room to grow node height — nodes are ~34px today, so 60px
      // spacing leaves a ~26px gap.
      nodeSpacing: 60,
      levelSeparation: 180,
      // Same axis as nodeSpacing but applied between disconnected
      // sub-trees. Drop in lockstep so a multi-microgrid layout
      // doesn't look stretchy compared to within-tree gaps.
      treeSpacing: 70,
    },
  },
  physics: { enabled: false },
  interaction: {
    hover: true,
    dragNodes: true,
    // Vis-network handles Shift+drag rubber-band on empty canvas
    // when this is on. Its Ctrl-click multi-add normally also
    // triggers here, but the Ctrl-keydown handler that enters
    // addEdgeMode (further below) preempts that branch in favour
    // of edge creation while Ctrl is held.
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
          if (!res.ok) notify(`Connect failed: ${res.error}`);
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
      li.className = `sp-event ${accepted ? "accepted" : "rejected"}`;
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
  if (scenarioReportTimer != null) {
    clearInterval(scenarioReportTimer);
    scenarioReportTimer = null;
  }
  inspectEl.innerHTML =
    '<p class="hint">Click a node to inspect. Right-click for the context menu.</p>';
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
      if (!data.ok) notify(`Create failed: ${data.error}`);
    } finally {
      btn.disabled = false;
    }
  });
}

function escapeHtml(s) {
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
const setupScenarioReportToggle = () =>
  makeSidePanelToggle("scenario-report-btn", renderScenarioReport);

async function renderScenarioReport() {
  document.getElementById("add-form").style.display = "none";
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
  // Scenarios — lifecycle, journal + reporter, CSV recording.
  // Lifecycle defuns are Rust-side; the *-end-after / random-*
  // helpers are Lisp wrappers in sim/common.lisp + sim/scenarios.lisp.
  "scenario-start",
  "scenario-stop",
  "scenario-event",
  "scenario-elapsed",
  "scenario-end-after",
  "scenario-record-csv",
  "scenario-stop-csv",
  "random-outage",
  "random-pick",
  "random-uniform",
  // Utilities
  "every",
  "run-with-timer",
  "cancel-timer",
  "sleep-for",
  "timerp",
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

// Render `src` as HTML with paren depth highlighting + simple
// string / comment colouring. Walks character-by-character so we
// don't have to ship a real parser. Mismatched closes (more
// closes than opens at some prefix) get their own class so they
// stand out instead of silently absorbing whatever colour the
// stack happened to be at.
const RAINBOW_DEPTHS = 7;
// Symbols that head a list and are syntax keywords rather than
// callable functions. Drives the .repl-special-form class so the
// shape of a form is visible at a glance: `defun`, `let`, `when`
// pop one colour; ordinary function calls get a different one.
const SPECIAL_FORMS = new Set([
  "defun", "defmacro", "defvar", "defconst", "defspecial",
  "let", "let*", "letrec",
  "if", "when", "unless", "cond", "case", "pcase",
  "progn", "prog1", "prog2",
  "lambda", "function",
  "while", "dolist", "dotimes",
  "condition-case", "catch", "throw", "unwind-protect",
  "setq", "setq-default",
  "and", "or", "not",
  "quote",
  "if-let", "when-let", "while-let",
  "save-excursion", "save-restriction", "with-current-buffer",
]);
function rainbowHighlight(src) {
  let out = "";
  let depth = 0;
  let inString = false;
  let inComment = false;
  let buf = "";
  // True when the next non-whitespace symbol token in `buf` is the
  // head of a freshly-opened list. Set on `(`, cleared once the
  // head is emitted (or on `)` for safety).
  let expectingHead = false;
  // Flush `buf` as plain text, except when `expectingHead` is set
  // — then split off the first non-whitespace token, classify it
  // as a special-form or function-call head, and clear the flag.
  // String / comment / mismatched-paren spans bypass this path
  // and pass an explicit class.
  const flush = (cls) => {
    if (!buf) return;
    if (cls) {
      out += `<span class="${cls}">${escapeHtml(buf)}</span>`;
    } else if (expectingHead) {
      const m = buf.match(/^(\s*)(\S+)([\s\S]*)$/);
      if (m) {
        const [, ws, head, rest] = m;
        const headCls = SPECIAL_FORMS.has(head)
          ? "repl-special-form"
          : "repl-function-head";
        out += escapeHtml(ws);
        out += `<span class="${headCls}">${escapeHtml(head)}</span>`;
        out += escapeHtml(rest);
        expectingHead = false;
      } else {
        // Buffer is whitespace-only; the head is still pending.
        out += escapeHtml(buf);
      }
    } else {
      out += escapeHtml(buf);
    }
    buf = "";
  };
  const opens = new Set(["(", "[", "{"]);
  const closes = new Set([")", "]", "}"]);
  for (let i = 0; i < src.length; i++) {
    const ch = src[i];
    if (inComment) {
      buf += ch;
      if (ch === "\n") {
        flush("repl-comment");
        inComment = false;
      }
      continue;
    }
    if (inString) {
      buf += ch;
      if (ch === "\\" && i + 1 < src.length) {
        buf += src[++i];
        continue;
      }
      if (ch === "\"") {
        flush("repl-string");
        inString = false;
      }
      continue;
    }
    if (ch === ";") {
      flush(null);
      buf = ch;
      inComment = true;
      continue;
    }
    if (ch === "\"") {
      flush(null);
      buf = ch;
      inString = true;
      continue;
    }
    if (opens.has(ch)) {
      flush(null);
      const cls = `paren-${depth % RAINBOW_DEPTHS}`;
      out += `<span class="${cls}">${ch}</span>`;
      depth++;
      expectingHead = true;
      continue;
    }
    if (closes.has(ch)) {
      flush(null);
      if (depth === 0) {
        out += `<span class="paren-mismatch">${ch}</span>`;
      } else {
        depth--;
        const cls = `paren-${depth % RAINBOW_DEPTHS}`;
        out += `<span class="${cls}">${ch}</span>`;
      }
      // The head of the just-closed form was already consumed (or
      // the form was empty); the parent's head was consumed
      // earlier. Either way, no head is pending here.
      expectingHead = false;
      continue;
    }
    buf += ch;
  }
  // Flush trailing text (string / comment / plain).
  flush(inString ? "repl-string" : inComment ? "repl-comment" : null);
  // Browsers swallow a textarea's trailing newline visually; add a
  // sentinel so the overlay's height matches the textarea row count.
  if (src.endsWith("\n")) out += " ";
  return out;
}

// Walk text[0..cursor] tracking columns and a stack of open-paren
// columns, skipping over string and ;-line-comment regions. The
// indent for a newline at `cursor` is the innermost still-open
// paren's column + 2; if no paren is open we land at column 0.
function indentForNewline(text, cursor) {
  let col = 0;
  const stack = [];
  let inString = false;
  let inComment = false;
  for (let i = 0; i < cursor; i++) {
    const ch = text[i];
    if (inComment) {
      if (ch === "\n") {
        inComment = false;
        col = 0;
      } else {
        col++;
      }
      continue;
    }
    if (inString) {
      if (ch === "\\" && i + 1 < cursor) {
        col += 2;
        i++;
        continue;
      }
      if (ch === "\"") inString = false;
      if (ch === "\n") col = 0;
      else col++;
      continue;
    }
    if (ch === ";") {
      inComment = true;
      col++;
      continue;
    }
    if (ch === "\"") {
      inString = true;
      col++;
      continue;
    }
    if (ch === "\n") {
      col = 0;
      continue;
    }
    if (ch === "(" || ch === "[" || ch === "{") {
      stack.push(col);
    } else if (ch === ")" || ch === "]" || ch === "}") {
      stack.pop();
    }
    col++;
  }
  if (stack.length === 0) return 0;
  return stack[stack.length - 1] + 2;
}

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
      if (ev.kind === "sample") {
        liveCharts.pushSample(ev.id, ev.metric, ev.ts_ms, ev.value);
      } else if (ev.kind === "microgrid_sample") {
        dashboardTiles.applySample(ev);
      } else if (ev.kind === "topology_changed") {
        onTopologyChanged(ev.version);
      } else if (ev.kind === "setpoint") {
        liveCharts.pushSetpoint(ev);
        pulseBar.recordSetpoint();
      } else if (ev.kind === "log") {
        appendLog(ev);
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
const dashboardTiles = (() => {
  // 60 samples × 1 Hz cadence = 60 s sparkline window. Wide enough
  // to see "did the value just change?" without dominating the
  // tile visually. Stored as a flat Float32Array of length
  // SPARK_LEN with a write cursor; on each push we overwrite the
  // oldest slot and bump the cursor. Cheaper than Array.shift on a
  // long array. NaN means "no sample at this slot" (page just
  // loaded — most of the window is still empty).
  const SPARK_LEN = 60;
  const sparkBuf = new Map(); // stream -> { values: Float32Array, cursor: int }
  function buf(stream) {
    let b = sparkBuf.get(stream);
    if (!b) {
      b = { values: new Float32Array(SPARK_LEN).fill(NaN), cursor: 0 };
      sparkBuf.set(stream, b);
    }
    return b;
  }
  function pushSample(stream, value) {
    const b = buf(stream);
    b.values[b.cursor] = value == null ? NaN : value;
    b.cursor = (b.cursor + 1) % SPARK_LEN;
  }
  // Ordered iterator over the ring — oldest to newest, skipping
  // empty slots before the first sample lands. Returns array of
  // {idx, value} where idx is the linearised position 0..SPARK_LEN-1.
  function orderedSamples(b) {
    const out = [];
    for (let i = 0; i < SPARK_LEN; i++) {
      const slot = (b.cursor + i) % SPARK_LEN;
      const v = b.values[slot];
      if (!Number.isNaN(v)) out.push({ idx: i, value: v });
    }
    return out;
  }
  function findEls(stream) {
    return document.querySelectorAll(`.dash-value[data-stream="${stream}"]`);
  }
  function findSparks(stream) {
    return document.querySelectorAll(`.dash-spark[data-stream="${stream}"]`);
  }
  // Power auto-scale: W → kW → MW based on magnitude. Mirrors the
  // existing chooseScale() logic for per-component charts so the
  // Dashboard reads in the same units a developer sees in the
  // inspector panel.
  function fmt(quantity, unit, value) {
    if (value == null || !Number.isFinite(value)) return "—";
    if (quantity === "Power" || unit === "W" || unit === "VAR") {
      const a = Math.abs(value);
      if (a >= 1e6) return `${(value / 1e6).toFixed(2)} M${unit}`;
      if (a >= 1e3) return `${(value / 1e3).toFixed(2)} k${unit}`;
      return `${value.toFixed(1)} ${unit}`;
    }
    // Voltage, frequency, percentage etc. — fixed unit, modest precision.
    return `${value.toFixed(2)} ${unit}`;
  }
  function renderSpark(stream) {
    const b = buf(stream);
    const samples = orderedSamples(b);
    for (const svg of findSparks(stream)) {
      if (samples.length < 2) {
        // Not enough points to draw a line — show nothing rather
        // than a misleading single dot.
        svg.innerHTML = "";
        continue;
      }
      const vals = samples.map((s) => s.value);
      const min = Math.min(...vals);
      const max = Math.max(...vals);
      const range = max - min || 1;
      // viewBox = 0..100 wide, 0..30 tall. 1 px padding top + bottom
      // so the line never clips at the edges.
      const points = samples
        .map((s) => {
          const x = (s.idx / (SPARK_LEN - 1)) * 100;
          const y = 30 - (((s.value - min) / range) * 28 + 1);
          return `${x.toFixed(1)},${y.toFixed(1)}`;
        })
        .join(" ");
      // Draw a y=0 baseline only when the window crosses zero —
      // for power tiles this is the import/export divider, and
      // it's noise on a constant-positive (e.g. consumer) tile.
      let baseline = "";
      if (min < 0 && max > 0) {
        const yZero = 30 - (((0 - min) / range) * 28 + 1);
        baseline = `<line class="baseline" x1="0" y1="${yZero.toFixed(1)}" x2="100" y2="${yZero.toFixed(1)}" />`;
      }
      svg.innerHTML = `${baseline}<polyline class="trace" points="${points}" />`;
    }
  }
  function paint(stream, snap) {
    for (const el of findEls(stream)) {
      el.textContent = fmt(snap.quantity, snap.unit, snap.value);
      el.classList.toggle("muted", snap.value == null);
    }
    pushSample(stream, snap.value);
    renderSpark(stream);
  }
  return {
    applySample(ev) {
      // WS frame shape matches the snapshot shape, minus the kind
      // discriminator. Pass straight through.
      paint(ev.stream, ev);
    },
    async backfill() {
      try {
        const res = await fetch("/api/microgrid/latest");
        if (!res.ok) return;
        const map = await res.json();
        for (const [stream, snap] of Object.entries(map)) paint(stream, snap);
      } catch (_) {
        // Best-effort. If the loopback isn't up yet (503 elsewhere),
        // the tiles stay on "—" until the first WS tick lands.
      }
      // Same path picks up the rendered formula strings for each
      // tile's hover tooltip. Static across samples (the formula
      // doesn't change per tick), so one fetch per mode-enter is
      // enough — topology mutations re-trigger this via the
      // refreshTopology path in init().
      await loadFormulas();
    },
  };
})();

async function loadFormulas() {
  try {
    const res = await fetch("/api/microgrid/formulas");
    if (!res.ok) return;
    const map = await res.json();
    for (const [stream, formula] of Object.entries(map)) {
      for (const tile of document.querySelectorAll(`.dash-tile`)) {
        const v = tile.querySelector(`.dash-value[data-stream="${stream}"]`);
        if (v) {
          // Tile-level title so hovering anywhere on the card
          // (number + sparkline + meta) surfaces the same
          // formula. Click later (F4 stage 2 in UI-design.org)
          // will navigate to a side-panel formula tree.
          tile.title = `${stream} = ${formula}`;
        }
      }
    }
  } catch (_) {
    // Best-effort — tile tooltips just show their default `title`
    // (none) if this fails.
  }
}

// ─── Clock + TZ toggle ─────────────────────────────────────────────────────
//
// switchyard's physics + gRPC boundary speak UTC. The UI displays
// timestamps in either UTC or the IANA zone the operator set via
// (set-timezone …) — defaulting to "Europe/Berlin" matching the
// configured demo target. clockState pulls the zone name once at
// boot via /api/clock; the TZ chip in the pulse bar flips between
// the local-zone short label (CET / CEST / EST / etc., picked via
// Intl) and "UTC". Persists in localStorage.
const TZ_PREF_KEY = "switchyard-tz";
const clockState = (() => {
  let simTz = "Europe/Berlin";
  let simLabel = "local";
  let mode = "local"; // "local" or "utc"
  function probeShortLabel(tz) {
    // Try `short` (CEST / EST) and `shortGeneric` (CET / EST) in
    // sequence, preferring a compact 3-4-char abbreviation. Some
    // browser/CLDR combinations return offset notation ("GMT+2")
    // or wordy generics ("Germany Time"); both are uglier than
    // the IANA city segment for chip display, so fall back to
    // that whenever the probe is offset-y or multi-word.
    for (const kind of ["short", "shortGeneric"]) {
      try {
        const parts = new Intl.DateTimeFormat("en-US", {
          timeZone: tz,
          timeZoneName: kind,
        }).formatToParts(new Date());
        const tag = parts.find((p) => p.type === "timeZoneName");
        if (tag && !/^GMT[+\-]/i.test(tag.value) && !/\s/.test(tag.value)) {
          return tag.value;
        }
      } catch (_) {
        /* try next */
      }
    }
    const seg = tz.split("/").pop();
    return seg ? seg.replace(/_/g, " ") : tz;
  }
  function timeZoneInUse() {
    return mode === "utc" ? "UTC" : simTz;
  }
  function updateChip() {
    const chip = document.getElementById("tz-toggle");
    if (!chip) return;
    chip.textContent = mode === "utc" ? "UTC" : simLabel.toLowerCase();
    chip.classList.toggle("active", mode === "utc");
  }
  function applyMode(next) {
    mode = next === "utc" ? "utc" : "local";
    updateChip();
  }
  return {
    async init() {
      try {
        const res = await fetch("/api/clock");
        if (res.ok) {
          const j = await res.json();
          if (j.tz) simTz = j.tz;
        }
      } catch (_) {
        // Keep the default; the chip label will show "local" + the
        // browser's local zone short. Not ideal but harmless.
      }
      simLabel = probeShortLabel(simTz);
      applyMode(localStorage.getItem(TZ_PREF_KEY) || "local");
      const chip = document.getElementById("tz-toggle");
      if (chip) {
        chip.addEventListener("click", () => {
          const next = mode === "utc" ? "local" : "utc";
          localStorage.setItem(TZ_PREF_KEY, next);
          applyMode(next);
          renderClockNow();
        });
      }
    },
    formatNow() {
      const d = new Date();
      try {
        return d.toLocaleTimeString("en-GB", {
          hour: "2-digit",
          minute: "2-digit",
          second: "2-digit",
          hour12: false,
          timeZone: timeZoneInUse(),
        });
      } catch (_) {
        return d.toTimeString().slice(0, 8);
      }
    },
    tzInUse() {
      return timeZoneInUse();
    },
  };
})();

function renderClockNow() {
  const el = document.getElementById("pulse-clock");
  if (el) el.textContent = clockState.formatNow();
}

// ─── Pulse bar ─────────────────────────────────────────────────────────────
//
// Always-on system pulse strip. Three live sources today:
//   - Setpoint sparkbar: rate of /ws/events kind="setpoint" frames,
//     bucketed into 12 × 5 s windows over the last minute.
//   - Health pill: rolling counters from /api/topology's health
//     field — recomputed every refreshTopology() call (WS push on
//     topology_changed already drives this).
//   - Loopback pill: /api/microgrid/status polled every 5 s. ✓ when
//     connected, ⚠ when still booting. The future Z6 graph-pill is
//     a sibling.
//   - Wall clock at the right edge, ticked every second.
//
// All four panels are read-only and tolerant of partial data — a
// page loaded before the loopback comes up shows ⚠ and flips to
// ✓ on the next poll. Mirrors tradingsim's `.pulse` shape so the
// developer sees the same "is the sim alive" pattern across both
// simulators.
const pulseBar = (() => {
  const SPARK_BUCKETS = 12;
  const BUCKET_MS = 5000;
  const buckets = new Array(SPARK_BUCKETS).fill(0);
  let lastSpan = pulseBucketIndex();
  function pulseBucketIndex() {
    // Floor of (now / BUCKET_MS) — when this rolls forward, every
    // bucket between lastSpan and now shifts in as a 0.
    return Math.floor(Date.now() / BUCKET_MS);
  }
  function rotateIfNeeded() {
    const idx = pulseBucketIndex();
    const advance = Math.min(idx - lastSpan, SPARK_BUCKETS);
    for (let i = 0; i < advance; i++) {
      buckets.shift();
      buckets.push(0);
    }
    lastSpan = idx;
  }
  function recordSetpoint() {
    rotateIfNeeded();
    buckets[SPARK_BUCKETS - 1] += 1;
    renderSpark();
  }
  function renderSpark() {
    const svg = document.getElementById("pulse-spark");
    if (!svg) return;
    const max = Math.max(1, ...buckets);
    // SVG viewBox is 0..60 wide × 0..16 tall. 5 px wide per bar
    // with no gap (the trace reads as a continuous histogram). Bar
    // height proportional to bucket / max; minimum 1 px so a single
    // event is still visible.
    const bw = 60 / SPARK_BUCKETS;
    const bars = buckets
      .map((v, i) => {
        const h = v === 0 ? 0 : Math.max(1, (v / max) * 16);
        const x = i * bw;
        const y = 16 - h;
        return `<rect class="bar" x="${x.toFixed(2)}" y="${y.toFixed(2)}" width="${(bw - 0.5).toFixed(2)}" height="${h.toFixed(2)}" />`;
      })
      .join("");
    svg.innerHTML = bars;
  }
  function renderHealth(components) {
    const counts = { ok: 0, standby: 0, error: 0 };
    for (const c of components) {
      const h = (c.health || "ok").toLowerCase();
      if (h in counts) counts[h] += 1;
    }
    const el = document.getElementById("pulse-health");
    if (!el) return;
    el.innerHTML = `
      <span class="health-chip ok"      title="ok components">OK ${counts.ok}</span>
      <span class="health-chip standby" title="standby components">STDBY ${counts.standby}</span>
      <span class="health-chip error"   title="error components">ERR ${counts.error}</span>`;
  }
  function renderGraph(status) {
    const el = document.getElementById("pulse-graph");
    if (!el) return;
    if (status == null) {
      el.textContent = "✓";
      el.className = "pulse-pill ok";
      el.title = "frequenz-microgrid-component-graph accepted the topology";
      el.onclick = null;
    } else {
      // Compact for the pill, full message in the title + alert on
      // click so the dev can read past the truncation.
      el.textContent = "⚠ rejected";
      el.className = "pulse-pill bad";
      el.title = status;
      el.onclick = () => alert(`Graph validator rejected the topology:\n\n${status}`);
    }
  }
  async function refreshLoopback() {
    const el = document.getElementById("pulse-loopback");
    if (!el) return;
    try {
      const res = await fetch("/api/microgrid/status");
      const j = await res.json();
      if (res.ok && j.connected) {
        el.textContent = `✓ ${j.component_count ?? "?"} nodes`;
        el.className = "pulse-pill ok";
      } else {
        el.textContent = "⚠ connecting";
        el.className = "pulse-pill warn";
      }
    } catch (_) {
      el.textContent = "✗ unreachable";
      el.className = "pulse-pill bad";
    }
  }
  function renderClock() {
    const el = document.getElementById("pulse-clock");
    if (!el) return;
    el.textContent = clockState.formatNow();
  }
  return {
    setup() {
      renderSpark();
      renderHealth([]);
      renderGraph(null);
      refreshLoopback();
      renderClock();
      setupDensityToggle();
      // Loopback poll: every 5 s while not connected, every 15 s
      // once connected (cheap heartbeat, picks up a server restart
      // within one cycle). Constants kept generous so a slow page
      // doesn't see the pill flicker on a stalled fetch.
      setInterval(refreshLoopback, 5000);
      // 1 Hz clock + spark rotation; the spark rotator also handles
      // the case where no setpoints fire for a while (buckets
      // advance + drop off the left).
      setInterval(() => {
        renderClock();
        rotateIfNeeded();
        renderSpark();
      }, 1000);
    },
    recordSetpoint,
    applyTopology(components, graphStatus) {
      renderHealth(components);
      // `graphStatus === undefined` keeps the existing display
      // (e.g. an older server build without the field); the field
      // is reported as `null` for healthy graphs.
      if (graphStatus !== undefined) renderGraph(graphStatus);
    },
  };
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

function setupDensityToggle() {
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
const VALID_MODES = new Set(["dashboard", "topology"]);

function applyMode(mode) {
  if (!VALID_MODES.has(mode)) mode = "dashboard";
  document.body.dataset.mode = mode;
  for (const btn of document.querySelectorAll(".mode-btn")) {
    btn.classList.toggle("active", btn.dataset.mode === mode);
  }
  // vis-network needs a redraw nudge when its container goes from
  // display:none back to visible — the canvas was sized to 0×0 while
  // hidden. Same shape the splitter resize handler uses.
  if (mode === "topology") refitCharts();
  if (mode === "dashboard") dashboardTiles.backfill();
}

function setupModeToggle() {
  for (const btn of document.querySelectorAll(".mode-btn")) {
    btn.addEventListener("click", () => {
      const mode = btn.dataset.mode;
      localStorage.setItem(MODE_KEY, mode);
      applyMode(mode);
    });
  }
  applyMode(localStorage.getItem(MODE_KEY) || "dashboard");
}

async function refreshTopology() {
  try {
    const res = await fetch("/api/topology");
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    topology.apply(data);
    // Pulse bar's health counters + graph pill read from the
    // same /api/topology fetch — one round-trip carries both
    // signals + a hot-reload's WS topology_changed nudge
    // already drives a refresh.
    pulseBar.applyTopology(data.components || [], data.graph_status);
  } catch (err) {
    setStatus(`error: ${err.message}`, "error");
  }
}

async function init() {
  setupAddForm();
  setupDefaultsToggle();
  setupScenarioReportToggle();
  setupSplitter();
  setupDrawerSplitter();
  setupOverridesDialog();
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
  setupModeToggle();
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
  });
  setupRepl();
}

init();
