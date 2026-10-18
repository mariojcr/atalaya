// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Home: the status Board (one tile per service) and the dense Table (everything
// on one screen, keyboard driven), plus the service drawer they share. Data comes
// from a single /api/v1/overview call; a service's detail from PromQL and /api/v1/logs.
"use strict";

const OVERVIEW_URL = "/api/v1/overview";
const HEALTH_NAME = { ok: t("health.ok"), warn: t("health.warn"), bad: t("health.bad"), unknown: t("health.unknown") };
const HEALTH_RANK = { bad: 0, warn: 1, unknown: 2, ok: 3 };
const HEALTH_VAR = { ok: "--ok", warn: "--warn", bad: "--error", unknown: "--muted" };
const hhmm = (sec) => fmtDate(sec * 1000, { hour: "2-digit", minute: "2-digit" });
const svcKey = (s) => `${s.job}@${s.host}`;

const home = {
  mode: store("atalaya.home") === "table" ? "table" : "tiles",
  data: null,
  ctrl: null,
  filter: "",
  cursor: -1,     // table row selected with the keyboard (index into rows)
  rows: [],       // visible table rows: { kind: "host"|"svc", key, host, svc }
  expanded: null, // key of the service expanded in the table
  inited: false,
};

/** Home range: like dashboards, NOW_DELAY behind "now" so the last rate point never
 *  sees a half-filled window. */
function homeTime() {
  const t = resolveTime();
  return time.preset ? { start: t.start - NOW_DELAY, end: t.end - NOW_DELAY } : t;
}

// ── SVG sparklines (lighter than 20 uPlot instances) ───────────────────────
function homeSpark(points, w = 204, h = 54) {
  const vs = (points || []).filter((p) => p[1] != null && Number.isFinite(p[1]));
  if (vs.length < 2) return `<svg class="hspark" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-hidden="true"></svg>`;
  const x0 = vs[0][0], x1 = vs.at(-1)[0], ys = vs.map((p) => p[1]);
  let lo = Math.min(...ys), hi = Math.max(...ys);
  if (hi === lo) { hi += 1; lo -= 1; }
  const X = (x) => 1 + ((x - x0) / (x1 - x0 || 1)) * (w - 2), Y = (y) => h - 3 - ((y - lo) / (hi - lo)) * (h - 8);
  const line = vs.map((p, i) => `${i ? "L" : "M"}${X(p[0]).toFixed(1)},${Y(p[1]).toFixed(1)}`).join("");
  return `<svg class="hspark" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-hidden="true"><path class="a" d="${line}L${(w - 1).toFixed(1)},${h - 1}L1,${h - 1}Z"/><path class="l" d="${line}" vector-effect="non-scaling-stroke"/></svg>`;
}

// ── Data ───────────────────────────────────────────────────────────────────
async function fetchOverview() {
  home.ctrl?.abort();
  const ctrl = home.ctrl = new AbortController();
  const t = homeTime();
  // /overview takes a duration: the preset as is or, for a fixed range, its length
  const qs = new URLSearchParams({ range: time.preset || `${Math.max(900, t.end - t.start)}s`, lang: LANG });
  const r = await fetch(`${OVERVIEW_URL}?${qs}`, { signal: ctrl.signal });
  const text = await r.text();
  let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 200) || r.statusText); }
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}

/** Worst health of a set of services */
function worstHealth(svcs) {
  return svcs.reduce((w, s) => (HEALTH_RANK[s.health] ?? 2) < (HEALTH_RANK[w] ?? 2) ? s.health : w, "ok");
}
function sortedServices(svcs) {
  return [...svcs].sort((a, b) => (HEALTH_RANK[a.health] ?? 2) - (HEALTH_RANK[b.health] ?? 2) || a.host.localeCompare(b.host) || a.job.localeCompare(b.job));
}

// ── View ───────────────────────────────────────────────────────────────────
function initHome() {
  if (home.inited) return;
  home.inited = true;
  const seg = $("#home-mode");
  seg.onclick = (e) => { const b = e.target.closest("[data-mode]"); if (b) setHomeMode(b.dataset.mode); };
  seg.onkeydown = (e) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault(); setHomeMode(home.mode === "tiles" ? "table" : "tiles"); $(`#home-mode [data-mode="${home.mode}"]`).focus();
  };
  $("#home-tiles").onclick = (e) => { const tile = e.target.closest(".tile"); if (tile) openDrawerFor(tile.dataset.key); };
  $("#home-tiles").onkeydown = (e) => {
    const tile = e.target.closest(".tile");
    if (tile && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); openDrawerFor(tile.dataset.key); }
  };
  const filter = $("#home-filter");
  filter.oninput = () => { home.filter = filter.value.trim().toLowerCase(); home.cursor = -1; renderHomeTable(); };
  filter.onkeydown = (e) => { if (e.key === "Escape") { filter.value = ""; filter.dispatchEvent(new Event("input")); filter.blur(); } if (e.key === "Enter" || e.key === "ArrowDown") { e.preventDefault(); moveCursor(1, true); } };
  $("#home-table").onclick = (e) => {
    const row = e.target.closest("tr[data-i]"); if (!row) return;
    home.cursor = +row.dataset.i; paintCursor();
    if (home.rows[home.cursor]?.kind === "svc") toggleExpand(home.rows[home.cursor].key);
  };
  document.addEventListener("keydown", homeKeys);
  initDrawer();
  setHomeMode(home.mode, true);
}

function setHomeMode(mode, silent) {
  home.mode = mode; store("atalaya.home", mode);
  $$("#home-mode [data-mode]").forEach((b) => b.setAttribute("aria-checked", String(b.dataset.mode === mode)));
  $("#home-tiles").hidden = mode !== "tiles";
  $("#home-dense").hidden = mode !== "table";
  if (!silent && home.data) render();
}

async function showHome() {
  initHome();
  await homeRefresh();
}

/** Firing alert counts (cheap: the server keeps the state in memory) */
async function fetchAlertCounts() {
  try {
    const r = await fetch(`/api/v1/alerts?lang=${LANG}`);
    const j = await r.json();
    return r.ok ? j.counts || null : null;
  } catch { return null; }
}

async function homeRefresh() {
  const status = $("#home-status");
  try {
    const [data, alerts] = await Promise.all([fetchOverview(), fetchAlertCounts()]);
    home.data = data; home.alerts = alerts;
    status.textContent = "";
    render();
    if (drawer.key) refreshDrawer();
  } catch (e) {
    if (e.name === "AbortError") return;
    status.className = "status error"; status.textContent = e.message;
  }
}

function render() {
  const d = home.data; if (!d) return;
  const s = d.summary || {};
  const warn = s.warnings || 0;
  const firing = home.alerts?.firing || 0;
  const dot = firing ? "bad" : warn ? "warn" : "ok";
  const head = firing ? `<a class="home-alerts" href="#/alerts">${tn("home.alerts_firing", firing)}</a>` : warn ? tn("home.warnings", warn) : t("home.allok");
  // Failed overview queries mean missing data: say so, with the details on hover
  const errs = d.errors || [];
  const degraded = errs.length ? ` · <span class="home-degraded" title="${esc(errs.join("\n"))}">${esc(tn("home.degraded", errs.length))}</span>` : "";
  $("#home-summary").innerHTML = `<i class="hdot ${errs.length && dot === "ok" ? "warn" : dot}" aria-hidden="true"></i><span>${head} · ${t("home.targets", { up: fmtInt(s.targets_up), total: fmtInt(s.targets_total) })} · ${t("n.series", { n: fmtInt(s.series) })}${degraded}</span>`;
  $("#home-updated").textContent = t("home.updated", { t: hhmm(d.time || Date.now() / 1000) });
  if (home.mode === "tiles") renderTiles(); else renderHomeTable();
}

// ── Board ──────────────────────────────────────────────────────────────────
function tileStatus(s) {
  const unit = s.spark_unit && s.spark_unit !== "percent" ? s.spark_unit : "percent";
  const cpu = s.cpu_pct != null ? `CPU ${fmtDec(s.cpu_pct, s.cpu_pct < 10 ? 1 : 0)} %` : null;
  const mem = s.mem_bytes != null ? fmtUnit(s.mem_bytes, "bytes", 0) : null;
  return { unit, meta: [t("n.series", { n: fmtInt(s.series) }), cpu, mem].filter(Boolean).join(" · ") };
}
function renderTiles() {
  const host = $("#home-tiles");
  const svcs = sortedServices(home.data.services || []);
  const keep = new Set();
  // Tiles are updated in place (same key) so refreshing does not flicker
  svcs.forEach((s) => {
    const key = svcKey(s); keep.add(key);
    let el = host.querySelector(`.tile[data-key="${CSS.escape(key)}"]`);
    if (!el) {
      el = document.createElement("div");
      el.className = "tile"; el.dataset.key = key; el.tabIndex = 0; el.setAttribute("role", "button");
      el.innerHTML = `<header><b class="tname"></b><span class="thost"></span><i class="hdot" aria-hidden="true"></i></header><div class="tspark"></div><p class="tst"><b></b><span></span></p><p class="tmeta"></p>`;
    }
    const health = s.health || "unknown";
    el.className = `tile ${health}`;
    el.setAttribute("aria-label", t("tile.aria", { job: s.job, host: s.host, health: HEALTH_NAME[health] }) + (s.status ? ", " + s.status : ""));
    $(".tname", el).textContent = s.job;
    $(".thost", el).textContent = s.host;
    $(".hdot", el).className = `hdot ${health}`;
    $(".tspark", el).innerHTML = homeSpark(s.spark);
    $(".tst b", el).textContent = HEALTH_NAME[health];
    $(".tst span", el).textContent = s.status ? ` · ${s.status}` : "";
    $(".tmeta", el).textContent = tileStatus(s).meta;
    host.appendChild(el); // in order: down, warning, then by host and name
  });
  $$(".tile", host).forEach((el) => { if (!keep.has(el.dataset.key)) el.remove(); });
  if (!svcs.length) host.innerHTML = `<div class="empty inline"><p>${t("home.empty")}</p></div>`;
}

// ── Dense table ────────────────────────────────────────────────────────────
function lastLogText(l) {
  if (!l || !l.body) return "";
  const lvl = l.level && l.level !== "INFO" ? `${l.level} ` : "";
  return `${hhmm(l.time)} ${l.service ? l.service + " · " : ""}${lvl}${l.body}`;
}
function renderHomeTable() {
  const d = home.data; if (!d) return;
  const q = home.filter;
  const byHost = new Map((d.hosts || []).map((h) => [h.name, h]));
  const svcs = sortedServices(d.services || []);
  (d.services || []).forEach((s) => { if (!byHost.has(s.host)) byHost.set(s.host, { name: s.host }); });
  const hosts = [...byHost.values()].sort((a, b) => a.name.localeCompare(b.name));
  const rows = [];
  hosts.forEach((h) => {
    const mine = svcs.filter((s) => s.host === h.name && (!q || s.job.toLowerCase().includes(q) || h.name.toLowerCase().includes(q)));
    if (!mine.length && !(q && h.name.toLowerCase().includes(q)) && q) return;
    rows.push({ kind: "host", key: `host:${h.name}`, host: h, health: worstHealth(svcs.filter((s) => s.host === h.name)) });
    mine.forEach((s) => rows.push({ kind: "svc", key: svcKey(s), svc: s }));
  });
  home.rows = rows;
  const spark = (pts) => homeSpark(pts, 90, 20);
  const html = rows.map((r, i) => {
    if (r.kind === "host") {
      const h = r.host;
      const memPct = h.mem_total_bytes ? (h.mem_used_bytes / h.mem_total_bytes) * 100 : null;
      return `<tr class="host" data-i="${i}"><td class="nm">${esc(h.name)}</td><td><i class="hdot ${r.health}" aria-label="${HEALTH_NAME[r.health]}"></i></td>` +
        `<td><span class="sp">${spark(h.cpu_spark)}<b>${h.cpu_pct != null ? fmtDec(h.cpu_pct) + " %" : "—"}</b></span></td>` +
        `<td class="c-mem"><span class="sp"><b>${memPct != null ? fmtDec(memPct, 0) + " %" : "—"}</b><span class="dim">${h.mem_total_bytes ? fmtUnit(h.mem_used_bytes, "bytes", 1) : ""}</span></span></td>` +
        `<td class="c-net"><b>${h.net_rx_bps != null ? fmtUnit(h.net_rx_bps, "Bps", 0) : "—"}</b></td>` +
        `<td class="num ${h.err_per_min ? "warn" : ""}">${h.err_per_min != null ? fmtDec(h.err_per_min) : "—"}</td><td class="num">${fmtInt(h.series)}</td>` +
        `<td class="c-last last" data-tip="${esc(lastLogText(h.last_log))}">${esc(lastLogText(h.last_log))}</td></tr>`;
    }
    const s = r.svc, health = s.health || "unknown";
    return `<tr class="sub ${home.expanded === r.key ? "sel open" : ""}" data-i="${i}" data-key="${esc(r.key)}" aria-expanded="${home.expanded === r.key}"><td class="nm"><span class="tree">└</span>${esc(s.job)}</td><td><i class="hdot ${health}" aria-label="${HEALTH_NAME[health]}"></i></td>` +
      `<td><span class="sp">${spark(s.spark)}<b>${s.cpu_pct != null ? fmtDec(s.cpu_pct) + " %" : "—"}</b></span></td>` +
      `<td class="c-mem"><b>${s.mem_bytes != null ? fmtUnit(s.mem_bytes, "bytes", 0) : "—"}</b></td><td class="c-net dim">—</td>` +
      `<td class="num ${s.err_per_min ? "warn" : ""}">${s.err_per_min != null ? fmtDec(s.err_per_min) : "—"}</td><td class="num">${fmtInt(s.series)}</td>` +
      `<td class="c-last last" data-tip="${esc(lastLogText(s.last_log))}">${esc(lastLogText(s.last_log))}</td></tr>`;
  }).join("");
  const tbody = $("#home-table tbody");
  // The expanded row (with its charts) survives a refresh: it is moved, not recreated
  const openRow = tbody.querySelector("tr.expand");
  tbody.innerHTML = html || `<tr><td colspan="8" class="dim">${t("home.nomatch", { q: esc(q) })}</td></tr>`;
  if (home.expanded) {
    const anchor = tbody.querySelector(`tr[data-key="${CSS.escape(home.expanded)}"]`);
    if (anchor && openRow) anchor.after(openRow);
    else if (anchor) expandRow(anchor, home.expanded);
    else { if (openRow) $$(".chart-host", openRow).forEach(destroyChart); home.expanded = null; }
  }
  $("#home-count").textContent = t("home.rows", { n: fmtInt(rows.length), s: fmtInt(d.summary?.series) });
  if (home.cursor >= rows.length) home.cursor = rows.length - 1;
  paintCursor();
}

function paintCursor() {
  $$("#home-table tr[data-i]").forEach((tr) => tr.classList.toggle("sel", +tr.dataset.i === home.cursor || (tr.dataset.key && tr.dataset.key === home.expanded)));
}
function moveCursor(delta, fromFilter) {
  if (!home.rows.length) return;
  home.cursor = fromFilter && home.cursor < 0 ? 0 : Math.max(0, Math.min(home.rows.length - 1, home.cursor + delta));
  paintCursor();
  const tr = $(`#home-table tr[data-i="${home.cursor}"]`);
  tr?.scrollIntoView({ block: "nearest" });
  if (fromFilter) $("#home-filter").blur();
}
function cursorService() {
  const r = home.rows[home.cursor];
  return r?.kind === "svc" ? r.svc : null;
}
function homeKeys(e) {
  if (current !== "home" || home.mode !== "table" || !drawer.el.hidden) return;
  const typing = e.target.matches("input, textarea, select, [contenteditable]");
  if (typing) return;
  if (e.key === "/") { e.preventDefault(); $("#home-filter").focus(); return; }
  if (e.key === "j" || e.key === "ArrowDown") { e.preventDefault(); moveCursor(1); }
  else if (e.key === "k" || e.key === "ArrowUp") { e.preventDefault(); moveCursor(-1); }
  else if (e.key === "Enter") { const r = home.rows[home.cursor]; if (r?.kind === "svc") { e.preventDefault(); toggleExpand(r.key); } }
  else if (e.key === "Escape" && home.expanded) { toggleExpand(home.expanded); }
  else if (e.key === "g") { const s = cursorService(); if (s) location.hash = explorerHash(s); }
  else if (e.key === "l") { const s = cursorService(); if (s) goLogs(s); }
}

function toggleExpand(key) {
  const tbody = $("#home-table tbody");
  tbody.querySelectorAll("tr.expand").forEach((tr) => { $$(".chart-host", tr).forEach(destroyChart); tr.remove(); });
  tbody.querySelectorAll("tr.open").forEach((tr) => { tr.classList.remove("open"); tr.setAttribute("aria-expanded", "false"); });
  if (home.expanded === key) { home.expanded = null; paintCursor(); return; }
  home.expanded = key;
  const anchor = tbody.querySelector(`tr[data-key="${CSS.escape(key)}"]`);
  if (anchor) expandRow(anchor, key);
  paintCursor();
}
function expandRow(anchor, key) {
  const s = (home.data?.services || []).find((x) => svcKey(x) === key); if (!s) return;
  anchor.classList.add("open"); anchor.setAttribute("aria-expanded", "true");
  const tr = document.createElement("tr"); tr.className = "expand";
  tr.innerHTML = `<td colspan="8"><div class="exp">
    <div class="pane"><span class="lb">${t("detail.cpu")}</span><div class="chart-host xchart" data-k="cpu"></div></div>
    <div class="pane"><span class="lb">${t("detail.mem")}</span><div class="chart-host xchart" data-k="mem"></div></div>
    <div class="pane"><span class="lb">${t("detail.err")}</span><div class="chart-host xchart" data-k="err"></div></div>
    <div class="pane logs"><span class="lb">${t("detail.logs")}</span><div class="log-list compact" data-k="logs"><p class="dim">${t("loading")}</p></div></div>
  </div></td>`;
  anchor.after(tr);
  fillDetail(s, tr, 4).catch((e) => { $("[data-k=logs]", tr).innerHTML = `<p class="werror">${esc(e.message)}</p>`; });
}

// ── Service detail (table and drawer) ──────────────────────────────────────
async function serviceDetail(s, t, signal) {
  const step = niceStep(t.end - t.start, 160);
  const sel = `{exported_job="${s.job}"}`;
  const [cpu, mem, logs] = await Promise.all([
    promRangeBatched(`sum(nomad_client_allocs_cpu_total_percent${sel})`, t.start, t.end, step, signal).catch(() => ({ series: [] })),
    promRangeBatched(`sum(nomad_client_allocs_memory_rss${sel})`, t.start, t.end, step, signal).catch(() => ({ series: [] })),
    api(`/api/v1/logs?${new URLSearchParams({ start: t.start, end: t.end, service: s.job, limit: 10 })}`),
  ]);
  // Errors per hour from the per-severity histogram (server buckets may be finer)
  const hours = new Map();
  for (let h = Math.floor(t.start / 3600) * 3600; h <= t.end; h += 3600) hours.set(h, 0);
  (logs.histogram || []).forEach((b) => {
    if (b.level !== "ERROR" && b.level !== "FATAL") return;
    const h = Math.floor(b.ts / 3600) * 3600; hours.set(h, (hours.get(h) || 0) + b.n);
  });
  const lines = [...(logs.logs || [])].sort((a, b) => b.ts - a.ts);
  return { cpu: cpu.series[0]?.points || [], mem: mem.series[0]?.points || [], errors: [...hours.entries()], logs: lines };
}
function logLines(lines, n) {
  if (!lines.length) return `<p class="dim">${t("nologs")}</p>`;
  return lines.slice(0, n).map((l) => `<div class="log-row"><span class="sev ${esc(l.level)}" aria-label="${esc(SEV_NAME[l.level] ?? l.level)}"></span><time>${esc(fmtDate(l.ts, { hour: "2-digit", minute: "2-digit", second: "2-digit" }))}</time><span class="svc">${esc(l.service)}</span><span class="msg" data-tip="${esc(l.body.length > 160 ? l.body : "")}">${esc(l.body.length > 160 ? l.body.slice(0, 160) + "…" : l.body)}</span></div>`).join("");
}
/** Renders CPU, memory, errors/hour and the latest `nLogs` logs into the container's data-k slots */
async function fillDetail(s, root, nLogs) {
  const tr = homeTime();
  const d = await serviceDetail(s, tr, root._ctrl?.signal);
  if (!root.isConnected) return d;
  // Charts fit their slot (height set by CSS): with a fixed height uPlot added the axes below it
  const accent = css("--accent"), opts = { height: "fit", start: tr.start, end: tr.end, grafana: true };
  const one = (name, points, color) => [{ name: "", keys: [], timeSeries: [{ labels: {}, name, points, style: { color } }] }];
  const cpuHost = $("[data-k=cpu]", root), memHost = $("[data-k=mem]", root), errHost = $("[data-k=err]", root);
  if (cpuHost) renderChart(cpuHost, null, one(t("chart.cpu"), d.cpu, accent), { ...opts, type: "area", unit: "percent", decimals: 1 });
  if (memHost) renderChart(memHost, null, one(t("chart.mem"), d.mem, css("--ok")), { ...opts, type: "line", unit: "bytes", decimals: 0 });
  if (errHost) renderChart(errHost, null, one(t("chart.err"), d.errors, css("--error")), { ...opts, type: "bar", unit: "short", decimals: 0 });
  const logsHost = $("[data-k=logs]", root); if (logsHost) logsHost.innerHTML = logLines(d.logs, nLogs);
  return d;
}

// ── Links to other views ───────────────────────────────────────────────────
function explorerHash(s) {
  const expr = `sum by (task) (nomad_client_allocs_cpu_total_percent{exported_job="${s.job}"})`;
  return `#/metrics?s=${encodeState({ ...defaultEx(), queries: [{ ...newQuery("A"), mode: "promql", expr }] })}`;
}
function goLogs(s) {
  logState.service = s.job; logState.host = ""; logState.level = ""; $("#log-q").value = "";
  closeDrawer();
  location.hash = "#/logs";
}

// ── Service drawer ─────────────────────────────────────────────────────────
const drawer = { el: null, key: null, ctrl: null, returnTo: null };
function initDrawer() {
  drawer.el = $("#drawer");
  $("#drawer-close").onclick = closeDrawer;
  drawer.el.addEventListener("click", (e) => { if (e.target === drawer.el) closeDrawer(); });
  document.addEventListener("keydown", (e) => {
    if (drawer.el.hidden) return;
    if (e.key === "Escape") { e.preventDefault(); closeDrawer(); return; }
    if (e.key === "Tab") { // keep focus inside the drawer
      const f = $$("a[href], button:not([disabled]), [tabindex='0']", drawer.el).filter((x) => x.offsetParent !== null);
      if (!f.length) return;
      if (e.shiftKey && document.activeElement === f[0]) { e.preventDefault(); f.at(-1).focus(); }
      else if (!e.shiftKey && document.activeElement === f.at(-1)) { e.preventDefault(); f[0].focus(); }
    }
  });
  // On mobile it is a bottom sheet: swiping down closes it
  const sheet = $(".drawer", drawer.el);
  let y0 = null;
  $(".drawer-head", drawer.el).addEventListener("touchstart", (e) => { y0 = e.touches[0].clientY; sheet.style.transition = "none"; }, { passive: true });
  $(".drawer-head", drawer.el).addEventListener("touchmove", (e) => { if (y0 == null) return; const dy = Math.max(0, e.touches[0].clientY - y0); sheet.style.transform = `translateY(${dy}px)`; }, { passive: true });
  $(".drawer-head", drawer.el).addEventListener("touchend", (e) => {
    if (y0 == null) return;
    const dy = e.changedTouches[0].clientY - y0; y0 = null; sheet.style.transition = ""; sheet.style.transform = "";
    if (dy > 80) closeDrawer();
  });
}
function openDrawerFor(key) {
  const s = (home.data?.services || []).find((x) => svcKey(x) === key); if (!s) return;
  drawer.key = key; drawer.returnTo = document.activeElement;
  const el = drawer.el, health = s.health || "unknown";
  $("#drawer-host").textContent = s.host;
  $("#drawer-svc").textContent = s.job;
  $("#drawer-dot").className = `hdot ${health}`;
  $("#drawer-sub").textContent = [s.status, s.alloc ? t("drawer.alloc", { id: s.alloc }) : null, t("n.series", { n: fmtInt(s.series) })].filter(Boolean).join(" · ");
  $("#drawer-explore").href = explorerHash(s);
  const dash = (dashList || []).find((d) => d.name === s.job);
  const dashBtn = $("#drawer-dash"); dashBtn.hidden = !dash; if (dash) dashBtn.href = `#/dashboards/${dash.name}`;
  $("#drawer-logs").onclick = (e) => { e.preventDefault(); goLogs(s); };
  $("#drawer-timeline").href = `#/timeline?service=${encodeURIComponent(s.job)}`;
  $("#drawer-timeline").onclick = () => closeDrawer();
  $("#d-cpu").textContent = s.cpu_pct != null ? `${fmtDec(s.cpu_pct)} %` : "—";
  $("#d-mem").textContent = s.mem_bytes != null ? fmtUnit(s.mem_bytes, "bytes", 0) : "—";
  const errEl = $("#d-err"); errEl.textContent = s.err_per_min != null ? fmtDec(s.err_per_min) : "—"; errEl.classList.toggle("warn", !!s.err_per_min);
  $("#d-range").textContent = time.preset ? (PRESETS.find((p) => p[0] === time.preset)?.[1] || time.preset).toLowerCase() : t("drawer.range_custom");
  $$(".chart-host", el).forEach((h) => { destroyChart(h); h.innerHTML = ""; });
  $("#d-logs").innerHTML = `<p class="dim">${t("loading")}</p>`;
  el.hidden = false; document.body.classList.add("drawer-open");
  $("#drawer-close").focus();
  if (!dashList) api("/api/v1/dashboards").then((j) => { dashList = j.dashboards; if (drawer.key === key) openDrawerFor(key); }).catch(() => {});
  refreshDrawer();
}
function refreshDrawer() {
  const s = (home.data?.services || []).find((x) => svcKey(x) === drawer.key); if (!s) return;
  drawer.ctrl?.abort(); drawer.ctrl = new AbortController();
  const body = $(".drawer-body", drawer.el); body._ctrl = drawer.ctrl;
  fillDetail(s, body, 10).catch((e) => { if (e.name !== "AbortError") $("#d-logs").innerHTML = `<p class="werror">${esc(e.message)}</p>`; });
}
function closeDrawer() {
  if (!drawer.el || drawer.el.hidden) return;
  drawer.ctrl?.abort();
  drawer.el.hidden = true; drawer.key = null; document.body.classList.remove("drawer-open");
  $$(".chart-host", drawer.el).forEach(destroyChart);
  drawer.returnTo?.focus?.(); drawer.returnTo = null;
}
