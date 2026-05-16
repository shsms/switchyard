// vis-network setup + per-category colour palette + the public
// topology API the rest of the SPA drives:
//
// - topology.apply(snapshot)     — replace canvas state with /api/topology data
// - topology.fit()               — recenter on the current graph extent
// - topology.get(id)             — lookup the component object by id
// - topology.parentsOf / childrenOf / connections / allIds / selectedIds
// - topology.mainMeterId()       — the meter flagged :main t (if any)
// - topology.setSelectionHandler — wire showComponent / clearSide to the canvas

import { notify, setStatus } from "./app.js";
import { showContextMenu, undoMgr } from "./editor.js";
import { mgPath } from "./routing.js";

function getCss(name) {
  return getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
}

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
export const topology = (() => {
  let network = null;
  let nodesDS = null;
  let edgesDS = null;
  const componentById = new Map();
  // Id of the meter currently flagged `:main t` (per the snapshot's
  // top-level `main_meter_id`). Captured here so copy/paste can
  // mark the pasted copy as `:main t` when the source meter was.
  let mainMeterId = null;
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
    mainMeterId = typeof data.main_meter_id === "number" ? data.main_meter_id : null;
    // The chrome status pill keeps showing the gRPC-visible count,
    // which is what most operators care about when reasoning about
    // their topology. Hidden meters render on the canvas (dashed)
    // for context but don't bump the official tally.
    const visibleCount = data.components.filter((c) => !c.hidden).length;
    setStatus(
      `${visibleCount} components, ${data.connections.length} connections`,
      "connected",
    );
    // Flip the body's mg-empty flag so the topology canvas's
    // empty-hint overlay (D5) shows/hides without a separate JS
    // pass. A microgrid with zero visible components is treated
    // as empty for hint purposes — hidden meters by themselves
    // don't disqualify the overlay.
    if (visibleCount === 0) {
      document.body.dataset.mgEmpty = "1";
    } else {
      delete document.body.dataset.mgEmpty;
    }
    const { nodes, edges } = buildVisData(data);
    if (!network) {
      nodesDS = new vis.DataSet(nodes);
      edgesDS = new vis.DataSet(edges);
      const container = document.getElementById("topology");
      network = new vis.Network(
        container,
        { nodes: nodesDS, edges: edgesDS },
        visOptions,
      );
      // Re-frame whenever the container resizes — switching subviews
      // (display:none → display:block) and dragging the drawer
      // splitter all fall through here. Without this, vis-
      // network's camera sticks to whatever extent was captured on
      // first paint and a graph that was wider than the canvas at
      // construction shows only half of itself afterwards.
      if (typeof ResizeObserver !== "undefined") {
        const ro = new ResizeObserver(() => {
          if (network && container.offsetWidth > 0 && container.offsetHeight > 0) {
            network.fit({ animation: false });
          }
        });
        ro.observe(container);
      }
      // vis-network's first auto-fit happens on stabilization, but
      // we ship with `physics.enabled = false` so stabilization
      // doesn't actually fire — call fit explicitly once
      // construction is done so the camera lands on the layout
      // extent rather than whatever default zoom vis-network
      // initialised with.
      network.once("afterDrawing", () => network.fit({ animation: false }));
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
      // connect and the WS topology refresh redraws.
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
    mainMeterId: () => mainMeterId,
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
    /// Re-frame the canvas so every visible node fits. vis-network's
    /// auto-fit only fires on stabilization (we have physics off so
    /// it never runs again after the first paint), and the first
    /// paint may have happened while the topology subview was
    /// `display:none` and the canvas measured 0 × 0. Call this on
    /// subview enter and after container resizes.
    fit() {
      if (!network) return;
      network.fit({
        animation: false,
        // Snug padding — the default leaves big margins that make a
        // small graph (3-4 components) look stranded in the canvas.
        nodes: undefined,
      });
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
      undoMgr.record()
        .then(() => fetch(mgPath("eval"), {
          method: "POST",
          body: `(connect ${data.from} ${data.to})`,
        }))
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

