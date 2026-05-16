// The drawer splitter: horizontal between the topology row and the
// bottom drawer. Goes through `makeSplitter` so the
// mousedown / mousemove / mouseup handshake stays in one place.

import { refitCharts } from "./inspect.js";

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

/// Horizontal splitter between topology row and bottom drawer.
/// Updates main's grid-template-rows to resize the drawer.
export function setupDrawerSplitter() {
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

