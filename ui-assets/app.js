// Phase-1 SPA entry point. Today: fetches /api/topology and dumps it
// JSON-formatted. The next commits replace this with a cytoscape
// topology view + uPlot per-component charts + a Lisp REPL panel.

const app = document.getElementById("app");
const status = document.getElementById("status");

function setStatus(text, klass) {
  status.textContent = text;
  status.className = "status " + (klass || "");
}

async function loadTopology() {
  try {
    const res = await fetch("/api/topology");
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    setStatus(
      `${data.components.length} components, ${data.connections.length} connections`,
      "connected",
    );
    app.innerHTML = `<pre>${JSON.stringify(data, null, 2)}</pre>`;
  } catch (err) {
    setStatus("error: " + err.message, "error");
    app.innerHTML = `<pre>${err.stack || err}</pre>`;
  }
}

loadTopology();
