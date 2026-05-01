// Phase-1 SPA. Renders /api/topology with cytoscape and shows
// per-node info on selection. Charts + REPL panel land in the next
// commits.

const status = document.getElementById("status");
const sideEl = document.getElementById("side");

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

function getCss(name) {
  return getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
}

// Collect children-of-each-id from the connections list so we can
// later reach into a component's downstream chain quickly.
function buildElements(topology) {
  const visible = topology.components.filter((c) => !c.hidden);
  const nodes = visible.map((c) => ({
    data: {
      id: String(c.id),
      name: c.name,
      category: c.category,
      subtype: c.subtype,
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
  // Per-category background. Cytoscape's selector syntax matches the
  // node's `data.category` attribute — same strings the JSON
  // /api/topology emits.
  const perCategory = Object.entries(CATEGORY_COLOR).map(([cat, color]) => ({
    selector: `node[category="${cat}"]`,
    style: { "background-color": color },
  }));

  return [
    {
      selector: "node",
      style: {
        "background-color": "#888",
        label: "data(name)",
        color: "#c9d1d9",
        "text-valign": "bottom",
        "text-margin-y": 6,
        "font-size": 11,
        "font-family": "ui-monospace, monospace",
        width: 30,
        height: 30,
        "border-width": 1,
        "border-color": "#0d1117",
      },
    },
    ...perCategory,
    {
      selector: "node:selected",
      style: {
        "border-width": 3,
        "border-color": "#58a6ff",
      },
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

function renderSide(node) {
  const d = node.data();
  sideEl.innerHTML = `
    <h2>${d.name}</h2>
    <dl>
      <dt>id</dt><dd>${d.id}</dd>
      <dt>category</dt><dd>${d.category}</dd>
      <dt>subtype</dt><dd>${d.subtype || "—"}</dd>
    </dl>
  `;
}

function clearSide() {
  sideEl.innerHTML = '<p class="hint">Click a node to inspect.</p>';
}

async function init() {
  let topology;
  try {
    const res = await fetch("/api/topology");
    if (!res.ok) throw new Error("HTTP " + res.status);
    topology = await res.json();
  } catch (err) {
    setStatus("error: " + err.message, "error");
    return;
  }

  const visibleCount = topology.components.filter((c) => !c.hidden).length;
  setStatus(
    `${visibleCount} components, ${topology.connections.length} connections`,
    "connected",
  );

  const cy = cytoscape({
    container: document.getElementById("topology"),
    elements: buildElements(topology),
    style: cytoscapeStylesheet(),
    layout: {
      name: "breadthfirst",
      directed: true,
      padding: 30,
      spacingFactor: 1.4,
    },
    wheelSensitivity: 0.2,
  });

  cy.on("tap", "node", (evt) => renderSide(evt.target));
  cy.on("tap", (evt) => {
    if (evt.target === cy) clearSide();
  });
}

init();
