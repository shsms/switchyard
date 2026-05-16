// Formula side panel + dashboard tile tooltips. The graph crate's
// `*_formula()` accessors return a rendered string like
//   MAX(#2 - COALESCE(#1002, #1001, 0.0), 0.0)
// — parseFormula chews that into an AST and formulaToHtml renders
// nested HTML with each `#N` as a clickable link back to the
// topology canvas. loadFormulas / setupFormulaTileClicks wire up
// the dashboard tile titles + the click → openFormulaPanel
// handoff.

import { inspectEl, jumpToTopology, mgPath, openInspector } from "./app.js";

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
export function setupFormulaTileClicks() {
  document.getElementById("dashboard")?.addEventListener("click", (ev) => {
    const tile = ev.target.closest(".dash-tile-interactive");
    if (!tile) return;
    const stream = tile.dataset.formulaStream;
    if (!stream) return;
    openFormulaPanel(stream);
  });
}
