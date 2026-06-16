// Microgrids landing-page list and Scenarios mode panel. Both
// poll the corresponding /api endpoint, render a card grid, and
// respond to clicks / WS pushes by re-fetching + re-rendering.

import { escapeHtml, notify, selectMicrogrid } from "./app.js";
import { readSelectedMg, renderReplMgChip } from "./routing.js";

export const microgridsPanel = (() => {
  let cached = []; // last /api/microgrids snapshot
  let pollTimer = null;

  function gridEl() { return document.getElementById("mglist-grid"); }
  function breadcrumbNameEl() { return document.getElementById("mg-breadcrumb-name"); }
  function breadcrumbTsoEl() { return document.getElementById("mg-breadcrumb-tso"); }

  function renderList() {
    const grid = gridEl();
    if (!grid) return;
    grid.innerHTML = "";
    for (const m of cached) {
      const card = document.createElement("button");
      card.type = "button";
      card.className = "mglist-card";
      card.dataset.id = m.id;
      const tso = m.tso ? `<span class="mg-tso">${escapeHtml(m.tso)}</span>` : "";
      card.innerHTML = `
        <span class="mglist-id">#${m.id}</span>
        <h3 class="mglist-name">${escapeHtml(m.name || "(unnamed)")}</h3>
        ${tso}
        <span class="mglist-meta muted">${m.component_count} components · gRPC :${m.grpc_port}</span>
      `;
      card.addEventListener("click", () => selectMicrogrid(m.id));
      grid.appendChild(card);
    }
    // Trailing [+ New microgrid] card: prompts for a name and
    // POSTs /api/microgrids/create, then selects the new entry.
    const newCard = document.createElement("button");
    newCard.type = "button";
    newCard.className = "mglist-card mglist-new";
    newCard.id = "mglist-new-btn";
    newCard.innerHTML = `<span class="mglist-plus">＋</span><span>New microgrid</span>`;
    newCard.addEventListener("click", () => {
      const name = prompt("Name for the new microgrid:");
      if (!name) return;
      fetch("/api/microgrids/create", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name }),
      })
        .then(async (r) => {
          if (!r.ok) throw new Error(await r.text());
          return r.json();
        })
        .then((m) => selectMicrogrid(m.id))
        .catch((e) => notify(`Create failed: ${e.message}`));
    });
    grid.appendChild(newCard);
  }

  function renderBreadcrumb() {
    const id = readSelectedMg();
    if (id == null) return;
    const entry = cached.find((m) => m.id === id);
    if (breadcrumbNameEl()) {
      breadcrumbNameEl().textContent = entry
        ? `#${entry.id} ${entry.name || "(unnamed)"}`
        : `#${id} (unknown)`;
    }
    if (breadcrumbTsoEl()) {
      breadcrumbTsoEl().textContent = entry?.tso ? `· ${entry.tso}` : "";
    }
  }

  async function refresh() {
    try {
      const res = await fetch("/api/microgrids");
      if (res.ok) cached = await res.json();
    } catch (_) {
      cached = [];
    }
    window.__mgPanelCache = cached;
    renderList();
    renderBreadcrumb();
    renderReplMgChip();
    schedulePoll();
  }

  function schedulePoll() {
    if (pollTimer) clearInterval(pollTimer);
    if (document.body.dataset.mode !== "microgrids") return;
    pollTimer = setInterval(async () => {
      if (document.body.dataset.mode !== "microgrids") {
        clearInterval(pollTimer);
        pollTimer = null;
        return;
      }
      try {
        const res = await fetch("/api/microgrids");
        if (res.ok) cached = await res.json();
      } catch (_) {}
      renderList();
      renderBreadcrumb();
    }, 5000);
  }

  return { refresh };
})();

// ─── Scenarios mode ─────────────────────────────────────────────────────────
//
// Driven by /api/scenarios (snapshot) + the POST endpoints for
// start / stop / next / prev / jump. Renders a 24-h horizontal
// timeline strip with one block per stage, a "now" marker pinned
// to the current local-hour, a stage-row list below, and Start /
// Prev / Next / Stop controls in the header. Pollers refresh the
// snapshot every 5 s while the mode is active — auto-advance
// transitions and journal events otherwise wouldn't update the
// timeline since they happen server-side without a WS push.
// Driven by the unified registry: /api/scenarios (the registered
// scenarios + their cue/check timeline) plus the journal readers
// /api/scenario (lifecycle), /api/scenario/report (live metrics + the
// scenario-expect ledger), and /api/scenario/events (activity feed).
// The journal tracks one scenario at a time; Run starts a scenario on
// the wall clock, Stop ends it. Headless/deterministic runs are a
// `swctl scenario run --stepped` / CI concern, not a UI action.
export const scenariosPanel = (() => {
  let scenarios = []; // /api/scenarios snapshot
  let summary = null; // /api/scenario (running/last journal)
  let report = null; // /api/scenario/report
  let events = []; // /api/scenario/events
  let csv = { dir: null, files: [] }; // /api/scenario/csv
  let pollTimer = null;

  function listEl() { return document.getElementById("scenarios-list"); }

  // The journal holds one scenario; it's running when a name is set and
  // it hasn't ended. `journalName` is the name it currently reflects
  // (running or just-stopped) — drives the last-run badge + run view.
  function runningName() {
    return summary?.name && !summary.ended_at ? summary.name : null;
  }
  function journalName() { return summary?.name || null; }
  function scenarioByName(n) { return scenarios.find((s) => s.name === n) || null; }

  function fmtSecs(s) {
    if (s == null) return "open";
    return s < 90 ? `${Math.round(s)}s` : `${Math.round(s / 60)}min`;
  }
  function mkBtn(label, onClick) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "hdr-btn";
    b.textContent = label;
    b.addEventListener("click", onClick);
    return b;
  }

  // ── registered-scenario list ────────────────────────────────────────
  function renderList() {
    const el = listEl();
    if (!el) return;
    const countEl = document.getElementById("sc-count");
    if (countEl) countEl.textContent = scenarios.length ? `${scenarios.length} registered` : "";
    if (scenarios.length === 0) {
      el.innerHTML =
        `<p class="muted">No scenarios registered. Load a config that calls <code>define-scenario</code>.</p>`;
      return;
    }
    const running = runningName();
    el.innerHTML = "";
    for (const s of scenarios) {
      const isRunning = s.name === running;
      const sections = [
        s.has_setup ? "setup" : null,
        s.n_drive ? `drive×${s.n_drive}` : null,
        s.n_agents ? `agents×${s.n_agents}` : null,
        s.n_cues ? `cues×${s.n_cues}` : null,
        s.n_expect ? `checks×${s.n_expect}` : null,
        s.records ? "rec" : null,
      ].filter(Boolean).join(" · ");
      const row = document.createElement("div");
      row.className = "sc-row";
      if (s.name === journalName()) row.classList.add("selected");
      row.innerHTML = `
        <div class="sc-row-main">
          <span class="sc-row-name">${escapeHtml(s.name)}</span>
          <span class="sc-row-meta">${s.schedule}/${s.clock} · ${fmtSecs(s.length_s)}${
            s.seed != null ? ` · seed ${s.seed}` : ""
          }</span>
          <span class="sc-row-desc muted">${escapeHtml(s.description || "")}</span>
        </div>
        <div class="sc-row-sections muted">${sections}</div>
        <div class="sc-row-badge">${badgeHtml(s.name, isRunning)}</div>
        <div class="sc-row-actions"></div>
      `;
      const actions = row.querySelector(".sc-row-actions");
      if (isRunning) {
        actions.appendChild(mkBtn("Stop", stopRun));
      } else {
        const run = mkBtn("Run", () => startRun(s.name));
        run.disabled = !!running; // one journal at a time
        run.title = running ? `${running} is running — stop it first` : "Run live";
        actions.appendChild(run);
      }
      el.appendChild(row);
    }
  }

  function badgeHtml(name, isRunning) {
    if (isRunning) return `<span class="sc-badge running">running</span>`;
    // Last-run result lives on the single journal, so only the
    // most-recently-run scenario carries a badge.
    if (name === journalName() && summary?.ended_at && report) {
      const p = report.checks_passed || 0;
      const f = report.checks_failed || 0;
      if (p + f === 0) return `<span class="sc-badge">ran</span>`;
      return `<span class="sc-badge ${f === 0 ? "pass" : "fail"}">✓${p} ✗${f}</span>`;
    }
    return "";
  }

  // ── run view ────────────────────────────────────────────────────────
  function renderRunView() {
    const view = document.getElementById("sc-run-view");
    if (!view) return;
    const name = journalName();
    const sc = name && scenarioByName(name);
    if (!name || !sc) { view.hidden = true; return; }
    view.hidden = false;

    const running = runningName() === name;
    const elapsed = report ? report.scenario_elapsed_s : summary ? summary.elapsed_s : 0;
    document.getElementById("sc-run-name").textContent = name;
    const status = document.getElementById("sc-run-status");
    status.textContent = running ? "running" : "stopped";
    status.className = `sc-badge ${running ? "running" : ""}`;
    document.getElementById("sc-run-elapsed").textContent =
      `${Math.round(elapsed || 0)}s${sc.length_s ? ` / ${fmtSecs(sc.length_s)}` : ""}`;
    const stopBtn = document.getElementById("sc-run-stop");
    if (stopBtn) stopBtn.disabled = !running;

    renderRunTimeline(sc, elapsed || 0);
    renderMetrics();
    renderChecks();
    renderEvents();
    renderCsv();
  }

  // Cues + checks positioned over the run length; fired once elapsed
  // passes their time, and checks coloured by their recorded result.
  function renderRunTimeline(sc, elapsed) {
    const track = document.getElementById("sc-run-timeline");
    if (!track) return;
    track.innerHTML = "";
    const tl = sc.timeline || [];
    if (tl.length === 0) {
      track.innerHTML = `<span class="muted">no cues or checks</span>`;
      return;
    }
    const span = sc.length_s || Math.max(...tl.map((t) => t.at_s), 1);
    // Recorded checks arrive oldest-first; correlate to timeline checks
    // (also oldest-first) by position.
    const reportChecks = report?.checks || [];
    let checkIdx = 0;
    for (const t of tl) {
      const dot = document.createElement("div");
      dot.className = `sc-tl-mark sc-tl-${t.kind}`;
      dot.style.left = `${Math.min(100, (t.at_s / span) * 100).toFixed(1)}%`;
      let state = elapsed >= t.at_s ? "fired" : "pending";
      let detail = "";
      if (t.kind === "check") {
        const rc = reportChecks[checkIdx++];
        if (rc) {
          state = rc.passed ? "pass" : "fail";
          detail = ` — ${rc.expectation}${rc.actual != null ? ` (got ${rc.actual})` : ""}`;
        }
      }
      dot.classList.add(`sc-tl-${state}`);
      dot.title = `${t.label} @${t.at_s}s${detail}`;
      track.appendChild(dot);
    }
  }

  function renderMetrics() {
    const el = document.getElementById("sc-run-metrics");
    if (!el) return;
    if (!report) { el.innerHTML = ""; return; }
    const rows = [
      ["peak import", `${(report.peak_main_meter_w / 1000).toFixed(1)} kW`],
      ["battery charged", `${report.total_battery_charged_wh.toFixed(0)} Wh`],
      ["battery discharged", `${report.total_battery_discharged_wh.toFixed(0)} Wh`],
      ["PV produced", `${report.total_pv_produced_wh.toFixed(0)} Wh`],
    ];
    if (report.soc_stats) rows.push(["SoC mean", `${report.soc_stats.mean_pct.toFixed(0)}%`]);
    el.innerHTML =
      `<h3>metrics</h3>` +
      rows.map(([k, v]) => `<div class="sc-metric"><span>${k}</span><b>${v}</b></div>`).join("");
  }

  function renderChecks() {
    const el = document.getElementById("sc-run-checks");
    if (!el) return;
    const checks = report?.checks || [];
    const head = `<h3>checks ${report ? `(${report.checks_passed}✓ ${report.checks_failed}✗)` : ""}</h3>`;
    if (checks.length === 0) { el.innerHTML = `${head}<p class="muted">none yet</p>`; return; }
    const rows = checks.map((c) =>
      `<div class="sc-check ${c.passed ? "pass" : "fail"}">
        <span>${c.passed ? "✓" : "✗"}</span>
        <span>#${c.component_id} ${escapeHtml(c.metric)}</span>
        <span class="muted">${escapeHtml(c.expectation)}${c.actual != null ? ` · got ${c.actual}` : ""}</span>
      </div>`).join("");
    el.innerHTML = `${head}${rows}`;
  }

  function renderEvents() {
    const el = document.getElementById("sc-run-events");
    if (!el) return;
    if (events.length === 0) { el.innerHTML = `<h3>events</h3><p class="muted">none</p>`; return; }
    const recent = events.slice(-12).reverse();
    el.innerHTML =
      `<h3>events</h3>` +
      recent.map((e) =>
        `<div class="sc-event"><code>${escapeHtml(e.kind)}</code> ${escapeHtml(String(e.payload))}</div>`,
      ).join("");
  }

  function renderCsv() {
    const el = document.getElementById("sc-run-csv");
    if (!el) return;
    if (!csv.files || csv.files.length === 0) { el.innerHTML = ""; return; }
    const links = csv.files.map((f) =>
      `<a class="sc-csv-link" href="/api/scenario/csv/${encodeURIComponent(f)}" download>${escapeHtml(f)}</a>`,
    ).join("");
    el.innerHTML = `<h3>recorded csv${csv.dir ? ` <span class="muted">${escapeHtml(csv.dir)}</span>` : ""}</h3>${links}`;
  }

  function updateActiveChip() {
    const chip = document.getElementById("active-scenarios");
    if (!chip) return;
    const name = runningName();
    if (!name) { chip.hidden = true; chip.textContent = ""; return; }
    chip.hidden = false;
    chip.textContent = `running: ${name}`;
    chip.title = "Click to view it in Scenarios mode";
  }

  // ── actions ─────────────────────────────────────────────────────────
  async function startRun(name) {
    const r = await fetch(`/api/scenarios/${encodeURIComponent(name)}/start`, { method: "POST" });
    if (!r.ok) notify(`run failed: ${await r.text()}`);
    await refresh();
  }
  async function stopRun() {
    const r = await fetch("/api/scenarios/stop", { method: "POST" });
    if (!r.ok) notify(`stop failed: ${await r.text()}`);
    await refresh();
  }

  function render() {
    renderList();
    renderRunView();
    updateActiveChip();
  }

  async function refresh() {
    try { scenarios = await (await fetch("/api/scenarios")).json(); } catch (_) { scenarios = []; }
    try { summary = await (await fetch("/api/scenario")).json(); } catch (_) { summary = null; }
    if (journalName()) {
      try { report = await (await fetch("/api/scenario/report")).json(); } catch (_) { report = null; }
      try {
        const e = await (await fetch("/api/scenario/events?limit=50")).json();
        events = e.events || [];
      } catch (_) { events = []; }
      try { csv = await (await fetch("/api/scenario/csv")).json(); } catch (_) { csv = { dir: null, files: [] }; }
    } else {
      report = null;
      events = [];
      csv = { dir: null, files: [] };
    }
    render();
    schedulePoll();
  }

  function schedulePoll() {
    if (pollTimer) return;
    // Cheap 3 s poll — server-side scenario time + checks have no WS
    // push, so the run view + header chip stay live by polling.
    pollTimer = setInterval(refresh, 3000);
  }

  function setup() {
    document.getElementById("sc-run-stop")?.addEventListener("click", stopRun);
    document.getElementById("active-scenarios")?.addEventListener("click", () => {
      document.querySelector("#mode-toggle .mode-btn[data-mode='scenarios']")?.click();
    });
    refresh();
  }
  return { setup, refresh };
})();
