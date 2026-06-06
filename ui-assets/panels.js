// Microgrids landing-page list and Scenarios mode panel. Both
// poll the corresponding /api endpoint, render a card grid, and
// respond to clicks / WS pushes by re-fetching + re-rendering.

import { escapeHtml, notify, selectMicrogrid } from "./app.js";
import { clockState } from "./chrome.js";

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
export const scenariosPanel = (() => {
  let cached = []; // last /api/scenarios snapshot
  let selectedName = null;
  let pollTimer = null;
  let lastSig = ""; // signature of the last paint to avoid thrash

  function selectEl()       { return document.getElementById("scenarios-select"); }
  function timelineEl()     { return document.getElementById("sc-timeline-track"); }
  function stageListEl()    { return document.getElementById("sc-stage-list"); }
  function manualBadgeEl()  { return document.getElementById("sc-manual-badge"); }
  function descEl()         { return document.getElementById("sc-description"); }

  function selected() {
    return cached.find((s) => s.name === selectedName) || null;
  }

  // A scenario is "running" once `(scenario-start)` has fired and
  // until the matching `(scenario-stop)`. `current_stage` is the
  // distinguishing field: start sets it (always to some valid index,
  // even if wallclock is outside the stage windows), stop clears it.
  // `started_at` survives a stop as a historical marker, so it can't
  // be used here.
  function isRunning(sc) {
    return !!(sc && sc.runtime && sc.runtime.current_stage != null);
  }

  // Reflect the running state in the header controls: Start is
  // active only while stopped; Stop / Next / Prev are active only
  // while running. Jump (clicking timeline blocks or stage rows)
  // mirrors Stop / Next / Prev — it's a no-op when nothing's
  // started.
  function renderButtons() {
    const sc = selected();
    const running = isRunning(sc);
    const startBtn = document.getElementById("sc-start");
    if (startBtn) startBtn.disabled = !sc || running;
    for (const id of ["sc-stop", "sc-next", "sc-prev"]) {
      const b = document.getElementById(id);
      if (b) b.disabled = !running;
    }
  }

  function fmtHour(h) {
    const hh = Math.floor(h);
    const mm = Math.round((h - hh) * 60);
    return `${String(hh).padStart(2, "0")}:${String(mm).padStart(2, "0")}`;
  }
  function localHour() {
    // Best-effort local-hour in the configured zone. Falls back to
    // the browser-local time. clockState exposes the IANA name in
    // simTz; Intl.DateTimeFormat with `hourCycle: "h23"` keeps the
    // 0..24 framing the timeline uses.
    const now = new Date();
    const tz = clockState.tzInUse();
    try {
      const parts = new Intl.DateTimeFormat("en-US", {
        timeZone: tz,
        hour12: false,
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      }).formatToParts(now);
      const get = (t) => Number(parts.find((p) => p.type === t)?.value || 0);
      const h = get("hour") % 24;
      return h + get("minute") / 60 + get("second") / 3600;
    } catch (_) {
      return now.getHours() + now.getMinutes() / 60 + now.getSeconds() / 3600;
    }
  }

  function renderSelect() {
    const sel = selectEl();
    if (!sel) return;
    const prev = selectedName;
    sel.innerHTML = "";
    if (cached.length === 0) {
      sel.disabled = true;
      const opt = document.createElement("option");
      opt.textContent = "(none registered)";
      sel.appendChild(opt);
      return;
    }
    sel.disabled = false;
    for (const s of cached) {
      const opt = document.createElement("option");
      opt.value = s.name;
      opt.textContent = s.name;
      sel.appendChild(opt);
    }
    if (cached.some((s) => s.name === prev)) {
      sel.value = prev;
    } else {
      sel.value = cached[0].name;
      selectedName = cached[0].name;
    }
  }

  function renderTimeline() {
    const track = timelineEl();
    if (!track) return;
    track.innerHTML = "";
    const sc = selected();
    if (!sc) return;
    const running = isRunning(sc);
    const total = 24;
    for (let i = 0; i < sc.stages.length; i++) {
      const st = sc.stages[i];
      const left = (st.hour_from / total) * 100;
      const width = ((st.hour_to - st.hour_from) / total) * 100;
      const block = document.createElement("button");
      block.type = "button";
      block.className = "sc-timeline-block";
      block.style.left = `${left.toFixed(2)}%`;
      block.style.width = `${width.toFixed(2)}%`;
      if (sc.runtime.current_stage === i) block.classList.add("active");
      block.textContent = st.name;
      block.title = `${st.name}: ${fmtHour(st.hour_from)} → ${fmtHour(st.hour_to)}`;
      // jump-to-stage is meaningful only when the scenario is running.
      block.disabled = !running;
      block.addEventListener("click", () => jumpTo(i));
      track.appendChild(block);
    }
    // "Now" marker positioned by local-hour. Hidden if outside any
    // covered range — pure annotation aid.
    const marker = document.createElement("div");
    marker.className = "sc-timeline-now";
    marker.style.left = `${(localHour() / total) * 100}%`;
    marker.title = `now: ${fmtHour(localHour())}`;
    track.appendChild(marker);
  }

  function renderStageList() {
    const list = stageListEl();
    if (!list) return;
    list.innerHTML = "";
    const sc = selected();
    if (!sc) return;
    const running = isRunning(sc);
    for (let i = 0; i < sc.stages.length; i++) {
      const st = sc.stages[i];
      const row = document.createElement("div");
      row.className = "sc-stage-row";
      if (sc.runtime.current_stage === i) row.classList.add("active");
      if (!running) row.classList.add("disabled");
      row.innerHTML = `
        <span class="sc-stage-idx">${i}</span>
        <span class="sc-stage-name">${escapeHtml(st.name)}</span>
        <span class="sc-stage-window">${fmtHour(st.hour_from)} → ${fmtHour(st.hour_to)}</span>
        <span class="sc-stage-action">${st.has_on ? "<code>on</code>" : "—"}</span>
      `;
      if (running) row.addEventListener("click", () => jumpTo(i));
      list.appendChild(row);
    }
    const badge = manualBadgeEl();
    if (badge) badge.hidden = !sc.runtime.manual_override;
    if (descEl()) descEl().textContent = sc.description || "";
  }

  async function refresh() {
    try {
      const res = await fetch("/api/scenarios");
      if (!res.ok) return;
      cached = await res.json();
    } catch (_) {
      cached = [];
    }
    renderSelect();
    repaint();
    schedulePoll();
  }
  // List every scenario whose runtime says it's running. Drives the
  // header chip alongside the Scenarios mode toggle so the operator
  // sees which scenarios are live without flipping into Scenarios
  // mode first.
  function updateActiveChip() {
    const chip = document.getElementById("active-scenarios");
    if (!chip) return;
    const running = cached.filter(
      (s) => s.runtime && s.runtime.current_stage != null,
    );
    if (running.length === 0) {
      chip.hidden = true;
      chip.textContent = "";
      return;
    }
    chip.hidden = false;
    const names = running.map((s) => s.name).join(", ");
    chip.textContent = `running: ${names}`;
    chip.title = `Click to view ${running.length === 1 ? "it" : "them"} in Scenarios mode`;
  }

  function repaint() {
    const sc = selected();
    const sig = sc ? `${JSON.stringify(sc.runtime)}|${sc.stages.length}` : "";
    // Buttons reflect runtime state every time — cheap toggle, and
    // the fast-path below skips re-rendering DOM that hasn't
    // changed, which would otherwise leave the buttons stuck on
    // their initial HTML state when no transition has happened.
    renderButtons();
    updateActiveChip();
    if (sig === lastSig) {
      // Cheap path: still nudge the now marker so it tracks time
      // even when no transition happened.
      const marker = document.querySelector(".sc-timeline-now");
      if (marker) marker.style.left = `${(localHour() / 24) * 100}%`;
      return;
    }
    lastSig = sig;
    renderTimeline();
    renderStageList();
  }
  function schedulePoll() {
    // Always run — the active-scenarios chip in the header needs to
    // stay current even when the user is in Microgrids mode, and a
    // 5 s poll is cheap. Inside Scenarios mode, repaint() also keeps
    // the timeline / stage list / buttons in sync.
    if (pollTimer) return;
    pollTimer = setInterval(async () => {
      try {
        const res = await fetch("/api/scenarios");
        if (res.ok) cached = await res.json();
      } catch (_) {}
      repaint();
    }, 5000);
  }
  async function post(action) {
    if (!selectedName) return;
    const r = await fetch(`/api/scenarios/${encodeURIComponent(selectedName)}/${action}`, {
      method: "POST",
    });
    if (!r.ok) {
      const body = await r.text();
      notify(`${action} failed: ${body}`);
    }
    await refresh();
  }
  function jumpTo(idx) {
    if (!selectedName) return;
    post(`jump/${idx}`);
  }
  function setup() {
    selectEl()?.addEventListener("change", (e) => {
      selectedName = e.target.value;
      lastSig = ""; // force repaint
      repaint();
    });
    document.getElementById("sc-start")?.addEventListener("click", () => post("start"));
    document.getElementById("sc-stop")?.addEventListener("click", () => post("stop"));
    document.getElementById("sc-next")?.addEventListener("click", () => post("next"));
    document.getElementById("sc-prev")?.addEventListener("click", () => post("prev"));
    // Header chip → jump to Scenarios mode. Clicking is the natural
    // next step when the user notices something's running.
    document
      .getElementById("active-scenarios")
      ?.addEventListener("click", () => {
        const btn = document.querySelector(
          "#mode-toggle .mode-btn[data-mode='scenarios']",
        );
        btn?.click();
      });
    // Kick the polling loop + first chip paint immediately so the
    // header chip shows the right thing even before the user enters
    // Scenarios mode for the first time.
    refresh();
  }
  return { setup, refresh };
})();
