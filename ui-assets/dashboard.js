// Dashboard panels: top-level tile aggregator + per-component row
// modules (battery pairs, PV / EV / CHP rows). Each module exposes
// `refresh(snapshot)` for topology-driven shape changes + `applySample(ev)`
// for the per-tick stream updates. All renders are direct DOM
// innerHTML — no virtual DOM; the grid is small and the writes are
// batched per render() call.

import { jumpToTopology, mgPath } from "./app.js";
import { loadFormulas } from "./formulas.js";

// Aggregated metrics from the loopback Microgrid client flow into the
// Dashboard pane via two paths: (a) /api/microgrid/latest at mode-
// enter time so the tiles paint immediately with a real number, and
// (b) microgrid_sample WS frames for the per-second updates. Every
// tile selects its source via `data-stream="..."`; new tiles only
// have to declare the right stream name to participate.
export const dashboardTiles = (() => {
  // 900 samples × 1 Hz cadence = 15 min sparkline window. Backfilled
  // from `/api/microgrid/history` on each backfill() (page load + mode
  // re-enter) so the trace shows the past quarter-hour immediately
  // instead of growing from empty. Per-tick noise from formula
  // sample-time misalignment looks like signal at 60 s windows; at
  // 15 min the trend dominates. Stored as a flat Float32Array of
  // length SPARK_LEN with a write cursor; on each push we overwrite
  // the oldest slot and bump the cursor. Cheaper than Array.shift on
  // a long array. NaN means "no sample at this slot" (the ring isn't
  // yet full).
  const SPARK_LEN = 900;
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
    // Any non-svg element tagged with this stream — covers the main
    // .dash-value number plus envelope `.env-lo` / `.env-hi`
    // siblings that share the same stream's value formatting.
    return document.querySelectorAll(`[data-stream="${stream}"]:not(svg)`);
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
      // Past 15 min of samples per stream, server-side. Pre-populate
      // the ring so the spark shows the historical trend right away
      // instead of growing from empty (60 s of jitter dominates an
      // unbackfilled trace; 15 min flattens it into a small bar).
      try {
        const hres = await fetch(mgPath("microgrid/history"));
        if (hres.ok) {
          const hmap = await hres.json();
          for (const [stream, samples] of Object.entries(hmap)) {
            const b = buf(stream);
            b.values.fill(NaN);
            // Keep only the last SPARK_LEN samples — the server cap
            // sits a hair over 900, but a slow client tab could lag
            // and pull more than that on a future endpoint version.
            const slice = samples.slice(-SPARK_LEN);
            const start = SPARK_LEN - slice.length;
            for (let i = 0; i < slice.length; i++) {
              const v = slice[i]?.value;
              b.values[start + i] = v == null ? NaN : v;
            }
            b.cursor = 0;
            renderSpark(stream);
          }
        }
      } catch (_) {
        // Best-effort. WS frames will fill the ring forward from here.
      }
      try {
        const res = await fetch(mgPath("microgrid/latest"));
        if (!res.ok) return;
        const map = await res.json();
        // Paint the latest readouts in the tile value boxes — but
        // don't append again (the history backfill above already
        // included the most recent sample).
        for (const [stream, snap] of Object.entries(map)) {
          for (const el of findEls(stream)) {
            el.textContent = fmt(snap.quantity, snap.unit, snap.value);
            el.classList.toggle("muted", snap.value == null);
          }
        }
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

// Shared formatters reused by all per-component dashboard rows.
function fmtRowPower(v) {
  if (v == null || !Number.isFinite(v)) return "—";
  const a = Math.abs(v);
  if (a >= 1e6) return `${(v / 1e6).toFixed(2)} MW`;
  if (a >= 1e3) return `${(v / 1e3).toFixed(2)} kW`;
  return `${v.toFixed(1)} W`;
}
function fmtRowSoc(v) {
  return v == null || !Number.isFinite(v) ? "—" : `${v.toFixed(1)}%`;
}
function socClass(v) {
  if (v == null || !Number.isFinite(v)) return "muted";
  if (v < 10 || v > 95) return "soc-warn";
  return "soc-ok";
}
function invPinned(d) {
  if (d.measured == null) return false;
  const span = Math.max(Math.abs(d.upper ?? 0), Math.abs(d.lower ?? 0), 1);
  const tol = 0.005 * span;
  return (
    (d.upper != null && d.measured >= d.upper - tol) ||
    (d.lower != null && d.measured <= d.lower + tol)
  );
}

// ─── Battery pairs: battery + paired battery-inverter, one row each ───────
//
// Each row pairs a battery (`tier2-row` CSS, left side) with the
// battery inverter wired immediately upstream of it (`tier3-row`
// CSS, right side). The pairing is read off the topology snapshot's
// connections list: walk parents of each visible battery, keep the
// first inverter with subtype "battery". Multi-battery inverters
// produce one row per battery; bare batteries with no inverter
// upstream still render (right cell muted).
//
// Refreshed on every /api/topology fetch + live-updated via
// applySample. Clicking the battery cell selects the battery;
// clicking the inverter cell selects the inverter — both jump
// the canvas to Topology with that node selected.
export const batteryPairs = (() => {
  // id -> { battery: {…}, inverterId?: u64 }
  const pairs = new Map();
  // inverterId -> { name, subtype, health, measured, lower, upper }
  const inverters = new Map();
  // battery id -> backref to the inverter id it pairs with (for
  // sample dispatch). Keeps lookup O(1) on the WS hot path.
  const invByBattery = new Map();
  let order = []; // battery ids, sort by SoC ascending then id
  const TRACKED_BATTERY = new Set(["soc_pct", "dc_power_w"]);
  const TRACKED_INVERTER = new Set([
    "active_power_w",
    "active_power_lower_bound_w",
    "active_power_upper_bound_w",
  ]);

  function sortKey(id) {
    const s = pairs.get(id)?.battery?.soc;
    return s == null ? Infinity : s;
  }
  function resort() {
    order = [...pairs.keys()].sort((a, b) => sortKey(a) - sortKey(b) || a - b);
  }
  function render() {
    const grid = document.getElementById("battery-rows");
    const section = grid?.closest(".dash-batteries");
    if (!grid || !section) return;
    section.hidden = pairs.size === 0;
    grid.innerHTML = "";
    for (const id of order) {
      const { battery: b, inverterId } = pairs.get(id);
      const inv = inverterId != null ? inverters.get(inverterId) : null;
      const wrap = document.createElement("div");
      wrap.className = "bat-pair";
      const socPct = b.soc == null ? 0 : Math.max(0, Math.min(100, b.soc));
      const bhCls = b.health === "ok" ? "health-ok" : "health-bad";
      const batCell = document.createElement("div");
      batCell.className = "tier2-row";
      batCell.dataset.id = id;
      batCell.innerHTML = `
        <span class="tier2-name">${b.name}</span>
        <span class="tier2-subtype">—</span>
        <span class="tier2-health ${bhCls}">${b.health}</span>
        <span class="tier2-soc-wrap">
          <span class="tier2-soc-bar ${socClass(b.soc)}" style="width:${socPct.toFixed(1)}%"></span>
          <span class="tier2-soc-text">${fmtRowSoc(b.soc)}</span>
        </span>
        <span class="tier2-power">${fmtRowPower(b.power_w)}</span>
      `;
      batCell.addEventListener("click", () => jumpToTopology(id));
      wrap.appendChild(batCell);
      const invCell = document.createElement("div");
      invCell.className = "tier3-row bat-pair-inv";
      if (inv) {
        invCell.dataset.id = inverterId;
        if (invPinned(inv)) invCell.classList.add("pinned");
        const ihCls = inv.health === "ok" ? "health-ok" : "health-bad";
        invCell.innerHTML = `
          <span class="tier3-name">${inv.name}</span>
          <span class="tier3-subtype muted">${inv.subtype || "—"}</span>
          <span class="tier3-health ${ihCls}">${inv.health}</span>
          ${envelopeBar(inv.lower, inv.measured, inv.upper, fmtRowPower)}
        `;
        invCell.addEventListener("click", () => jumpToTopology(inverterId));
      } else {
        invCell.classList.add("muted");
        invCell.innerHTML = `<span class="tier3-name muted">no battery inverter</span>`;
      }
      wrap.appendChild(invCell);
      grid.appendChild(wrap);
    }
  }
  async function seedBattery(id) {
    try {
      const [soc, dc] = await Promise.all([
        fetch(`${mgPath("history")}?id=${id}&metric=soc_pct&window_s=10`).then((r) => r.json()),
        fetch(`${mgPath("history")}?id=${id}&metric=dc_power_w&window_s=10`).then((r) => r.json()),
      ]);
      const p = pairs.get(id);
      if (!p) return;
      p.battery.soc = soc.samples?.at(-1)?.[1] ?? null;
      p.battery.power_w = dc.samples?.at(-1)?.[1] ?? null;
    } catch (_) {}
  }
  async function seedInverter(id) {
    try {
      const [m, lo, hi] = await Promise.all([
        fetch(`${mgPath("history")}?id=${id}&metric=active_power_w&window_s=10`).then((r) => r.json()),
        fetch(`${mgPath("history")}?id=${id}&metric=active_power_lower_bound_w&window_s=10`).then((r) => r.json()),
        fetch(`${mgPath("history")}?id=${id}&metric=active_power_upper_bound_w&window_s=10`).then((r) => r.json()),
      ]);
      const inv = inverters.get(id);
      if (!inv) return;
      inv.measured = m.samples?.at(-1)?.[1] ?? null;
      inv.lower = lo.samples?.at(-1)?.[1] ?? null;
      inv.upper = hi.samples?.at(-1)?.[1] ?? null;
    } catch (_) {}
  }
  return {
    async refresh(snapshot) {
      const components = snapshot?.components || [];
      const allConns = [
        ...(snapshot?.connections || []),
        ...(snapshot?.hidden_connections || []),
      ];
      const byId = new Map(components.map((c) => [c.id, c]));
      // Map each battery id → its first parent that's a battery
      // inverter (walking edges where dest == battery id). Multi-
      // parent batteries land on the first matching parent in
      // edge order; same heuristic the loopback's BatteryPool uses.
      function findInverter(batteryId) {
        for (const [from, to] of allConns) {
          if (to !== batteryId) continue;
          const parent = byId.get(from);
          if (parent?.category === "inverter" && parent.subtype === "battery") {
            return parent.id;
          }
        }
        return null;
      }
      const nextPairs = new Map();
      const nextInverters = new Map();
      const nextInvByBattery = new Map();
      const batteries = components.filter(
        (c) => c.category === "battery" && !c.hidden,
      );
      for (const b of batteries) {
        const inverterId = findInverter(b.id);
        const prev = pairs.get(b.id);
        nextPairs.set(b.id, {
          battery: {
            name: b.name,
            health: b.health,
            soc: prev?.battery?.soc ?? null,
            power_w: prev?.battery?.power_w ?? null,
          },
          inverterId,
        });
        if (inverterId != null) {
          const invMeta = byId.get(inverterId);
          const prevInv = inverters.get(inverterId);
          nextInverters.set(inverterId, {
            name: invMeta?.name ?? `#${inverterId}`,
            subtype: invMeta?.subtype ?? null,
            health: invMeta?.health ?? "unknown",
            measured: prevInv?.measured ?? null,
            lower: prevInv?.lower ?? null,
            upper: prevInv?.upper ?? null,
          });
          nextInvByBattery.set(b.id, inverterId);
        }
      }
      pairs.clear();
      for (const [k, v] of nextPairs) pairs.set(k, v);
      inverters.clear();
      for (const [k, v] of nextInverters) inverters.set(k, v);
      invByBattery.clear();
      for (const [k, v] of nextInvByBattery) invByBattery.set(k, v);
      resort();
      render();
      await Promise.all([
        ...batteries.map((b) => seedBattery(b.id)),
        ...[...inverters.keys()].map((id) => seedInverter(id)),
      ]);
      resort();
      render();
    },
    applySample(ev) {
      if (TRACKED_BATTERY.has(ev.metric)) {
        const p = pairs.get(ev.id);
        if (!p) return;
        if (ev.metric === "soc_pct") p.battery.soc = ev.value;
        else if (ev.metric === "dc_power_w") p.battery.power_w = ev.value;
        if (ev.metric === "soc_pct") resort();
        render();
      } else if (TRACKED_INVERTER.has(ev.metric)) {
        const inv = inverters.get(ev.id);
        if (!inv) return;
        if (ev.metric === "active_power_w") inv.measured = ev.value;
        else if (ev.metric === "active_power_lower_bound_w") inv.lower = ev.value;
        else if (ev.metric === "active_power_upper_bound_w") inv.upper = ev.value;
        render();
      }
    },
  };
})();

// Shared envelope renderer for a (lower, current, upper) triple.
// Returns an HTML fragment that draws a horizontal track with a
// marker at `current`'s position between `lower` and `upper`,
// pinned-hi / pinned-lo classes when the marker hits either edge
// within 0.5 % of the span. Falls back to a muted "—" placeholder
// when bounds are missing or degenerate so the row still aligns.
//
// `fmtValue` formats both the marker readout and the hover-tooltip
// endpoints; callers pass it pre-bound to whatever unit family the
// row deals in (W / kW for Power, % for Percentage, etc.) — keeps
// the helper agnostic of the tiles' quantity table.
function envelopeBar(lower, current, upper, fmtValue) {
  const finite = (v) => v != null && Number.isFinite(v);
  if (!finite(lower) || !finite(upper) || upper <= lower) {
    return `<div class="envelope muted"><span class="envelope-current">—</span></div>`;
  }
  const hasCurrent = finite(current);
  const span = upper - lower;
  const pos = hasCurrent ? Math.max(0, Math.min(1, (current - lower) / span)) : 0.5;
  const tol = 0.005 * span;
  let markerCls = "envelope-marker";
  if (hasCurrent && current >= upper - tol) markerCls += " pinned-hi";
  else if (hasCurrent && current <= lower + tol) markerCls += " pinned-lo";
  const readout = hasCurrent ? fmtValue(current) : "—";
  const title = `${fmtValue(lower)} → ${fmtValue(upper)}`;
  return `
    <div class="envelope" title="${title}">
      <div class="envelope-track">
        <span class="${markerCls}" style="left:${(pos * 100).toFixed(1)}%"></span>
      </div>
      <span class="envelope-current">${readout}</span>
    </div>
  `;
}

// ─── PV inverter rows ─────────────────────────────────────────────────────
//
// One row per visible solar inverter. Measured AC active power
// highlights when it clips against either envelope bound — same
// operator-visible signal that the upstream control app's setpoint
// is being held back by the inverter's own clamp. Battery inverters
// are intentionally absent from this section; they pair with their
// batteries in the Batteries section above.
export const pvRows = (() => {
  const data = new Map(); // id -> { name, subtype, health, measured, lower, upper }
  let order = [];
  const TRACKED = new Set([
    "active_power_w",
    "active_power_lower_bound_w",
    "active_power_upper_bound_w",
  ]);

  function resort() {
    order = [...data.keys()].sort((a, b) => {
      const A = data.get(a);
      const B = data.get(b);
      const pa = invPinned(A) ? 0 : 1;
      const pb = invPinned(B) ? 0 : 1;
      if (pa !== pb) return pa - pb;
      return a - b;
    });
  }
  function render() {
    const grid = document.getElementById("pv-rows");
    const section = grid?.closest(".dash-pv");
    if (!grid || !section) return;
    section.hidden = data.size === 0;
    grid.innerHTML = "";
    for (const id of order) {
      const d = data.get(id);
      const row = document.createElement("div");
      row.className = "tier3-row";
      row.dataset.id = id;
      if (invPinned(d)) row.classList.add("pinned");
      const healthCls = d.health === "ok" ? "health-ok" : "health-bad";
      row.innerHTML = `
        <span class="tier3-name">${d.name}</span>
        <span class="tier3-subtype muted">${d.subtype || "—"}</span>
        <span class="tier3-health ${healthCls}">${d.health}</span>
        ${envelopeBar(d.lower, d.measured, d.upper, fmtRowPower)}
      `;
      row.addEventListener("click", () => jumpToTopology(id));
      grid.appendChild(row);
    }
  }
  async function seedFromHistory(id) {
    try {
      const [m, lo, hi] = await Promise.all([
        fetch(`${mgPath("history")}?id=${id}&metric=active_power_w&window_s=10`).then((r) => r.json()),
        fetch(`${mgPath("history")}?id=${id}&metric=active_power_lower_bound_w&window_s=10`).then((r) => r.json()),
        fetch(`${mgPath("history")}?id=${id}&metric=active_power_upper_bound_w&window_s=10`).then((r) => r.json()),
      ]);
      const d = data.get(id);
      if (!d) return;
      d.measured = m.samples?.at(-1)?.[1] ?? null;
      d.lower = lo.samples?.at(-1)?.[1] ?? null;
      d.upper = hi.samples?.at(-1)?.[1] ?? null;
    } catch (_) {}
  }
  return {
    async refresh(snapshot) {
      const components = snapshot?.components || [];
      const inverters = components.filter(
        (c) => c.category === "inverter" && c.subtype === "solar" && !c.hidden,
      );
      const next = new Map();
      for (const c of inverters) {
        const prev = data.get(c.id);
        next.set(c.id, {
          name: c.name,
          subtype: c.subtype,
          health: c.health,
          measured: prev?.measured ?? null,
          lower: prev?.lower ?? null,
          upper: prev?.upper ?? null,
        });
      }
      data.clear();
      for (const [k, v] of next) data.set(k, v);
      resort();
      render();
      await Promise.all(inverters.map((c) => seedFromHistory(c.id)));
      resort();
      render();
    },
    applySample(ev) {
      if (!TRACKED.has(ev.metric)) return;
      const d = data.get(ev.id);
      if (!d) return;
      if (ev.metric === "active_power_w") d.measured = ev.value;
      else if (ev.metric === "active_power_lower_bound_w") d.lower = ev.value;
      else if (ev.metric === "active_power_upper_bound_w") d.upper = ev.value;
      resort();
      render();
    },
  };
})();

// ─── EV charger rows ──────────────────────────────────────────────────────
//
// EV rows mirror the battery row shape: name + health pill + SoC bar
// + DC power. Click → jump to Topology with the EV selected.
export const evRows = (() => {
  const data = new Map(); // id -> { name, health, soc, power_w }
  const TRACKED = new Set(["soc_pct", "dc_power_w"]);

  function render() {
    const grid = document.getElementById("ev-rows");
    const section = grid?.closest(".dash-ev");
    if (!grid || !section) return;
    section.hidden = data.size === 0;
    grid.innerHTML = "";
    const ids = [...data.keys()].sort((a, b) => a - b);
    for (const id of ids) {
      const d = data.get(id);
      const row = document.createElement("div");
      row.className = "tier5-row cat-ev-charger";
      row.dataset.id = id;
      const healthCls = d.health === "ok" ? "health-ok" : "health-bad";
      const socPct = d.soc == null ? 0 : Math.max(0, Math.min(100, d.soc));
      row.innerHTML = `
        <span class="tier5-name">${d.name}</span>
        <span class="tier5-cat muted">ev-charger</span>
        <span class="tier5-health ${healthCls}">${d.health}</span>
        <span class="tier5-soc-wrap">
          <span class="tier5-soc-bar" style="width:${socPct.toFixed(1)}%"></span>
          <span class="tier5-soc-text">${fmtRowSoc(d.soc)}</span>
        </span>
        <span class="tier5-power">${fmtRowPower(d.power_w)}</span>
      `;
      row.addEventListener("click", () => jumpToTopology(id));
      grid.appendChild(row);
    }
  }
  async function seedFromHistory(id) {
    try {
      const [p, soc] = await Promise.all([
        fetch(`${mgPath("history")}?id=${id}&metric=dc_power_w&window_s=10`).then((r) => r.json()),
        fetch(`${mgPath("history")}?id=${id}&metric=soc_pct&window_s=10`).then((r) => r.json()),
      ]);
      const d = data.get(id);
      if (!d) return;
      d.power_w = p.samples?.at(-1)?.[1] ?? null;
      d.soc = soc.samples?.at(-1)?.[1] ?? null;
    } catch (_) {}
  }
  return {
    async refresh(snapshot) {
      const components = snapshot?.components || [];
      const rows = components.filter((c) => c.category === "ev-charger" && !c.hidden);
      const next = new Map();
      for (const c of rows) {
        const prev = data.get(c.id);
        next.set(c.id, {
          name: c.name,
          health: c.health,
          soc: prev?.soc ?? null,
          power_w: prev?.power_w ?? null,
        });
      }
      data.clear();
      for (const [k, v] of next) data.set(k, v);
      render();
      await Promise.all(rows.map((c) => seedFromHistory(c.id)));
      render();
    },
    applySample(ev) {
      if (!TRACKED.has(ev.metric)) return;
      const d = data.get(ev.id);
      if (!d) return;
      if (ev.metric === "soc_pct") d.soc = ev.value;
      else if (ev.metric === "dc_power_w") d.power_w = ev.value;
      render();
    },
  };
})();

// ─── CHP rows ─────────────────────────────────────────────────────────────
//
// CHP rows show name + health + AC active power; no SoC field. The
// AC reading is signed (-ve when generating into the grid).
export const chpRows = (() => {
  const data = new Map(); // id -> { name, health, power_w }
  const TRACKED = new Set(["active_power_w"]);

  function render() {
    const grid = document.getElementById("chp-rows");
    const section = grid?.closest(".dash-chp");
    if (!grid || !section) return;
    section.hidden = data.size === 0;
    grid.innerHTML = "";
    const ids = [...data.keys()].sort((a, b) => a - b);
    for (const id of ids) {
      const d = data.get(id);
      const row = document.createElement("div");
      row.className = "tier5-row cat-chp";
      row.dataset.id = id;
      const healthCls = d.health === "ok" ? "health-ok" : "health-bad";
      row.innerHTML = `
        <span class="tier5-name">${d.name}</span>
        <span class="tier5-cat muted">chp</span>
        <span class="tier5-health ${healthCls}">${d.health}</span>
        <span class="tier5-soc-wrap muted">—</span>
        <span class="tier5-power">${fmtRowPower(d.power_w)}</span>
      `;
      row.addEventListener("click", () => jumpToTopology(id));
      grid.appendChild(row);
    }
  }
  async function seedFromHistory(id) {
    try {
      const p = await fetch(
        `${mgPath("history")}?id=${id}&metric=active_power_w&window_s=10`,
      ).then((r) => r.json());
      const d = data.get(id);
      if (!d) return;
      d.power_w = p.samples?.at(-1)?.[1] ?? null;
    } catch (_) {}
  }
  return {
    async refresh(snapshot) {
      const components = snapshot?.components || [];
      const rows = components.filter((c) => c.category === "chp" && !c.hidden);
      const next = new Map();
      for (const c of rows) {
        const prev = data.get(c.id);
        next.set(c.id, {
          name: c.name,
          health: c.health,
          power_w: prev?.power_w ?? null,
        });
      }
      data.clear();
      for (const [k, v] of next) data.set(k, v);
      render();
      await Promise.all(rows.map((c) => seedFromHistory(c.id)));
      render();
    },
    applySample(ev) {
      if (!TRACKED.has(ev.metric)) return;
      const d = data.get(ev.id);
      if (!d) return;
      d.power_w = ev.value;
      render();
    },
  };
})();
