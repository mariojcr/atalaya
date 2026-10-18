// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Timeline: what happened in the range, in time order. A shared time axis with
// lanes below it: deploys and restarts, logs per service, alerts (errors and
// bursts) and metrics (those in timeline.yaml plus any added during the session).
// A cursor shows what happened at that minute. Data comes from /api/v1/events;
// the cursor's logs from /api/v1/logs.
"use strict";

const EVENTS_URL = "/api/v1/events";
const EV_KIND = {
  alloc_started: { lane: "deploy", cls: "start", name: t("ev.alloc_started") },
  alloc_stopped: { lane: "deploy", cls: "stop", name: t("ev.alloc_stopped") },
  restart: { lane: "deploy", cls: "restart", name: t("ev.restart") },
  errors: { lane: "alert", cls: "error", name: t("ev.errors") },
  log_burst: { lane: "alert", cls: "burst", name: t("ev.log_burst") },
  // alerts.yaml alerts: diamond in the severity colour; hollow once resolved
  alert_firing: { lane: "alert", cls: "alert", name: t("ev.alert_firing") },
  alert_resolved: { lane: "alert", cls: "alert-ok", name: t("ev.alert_resolved") },
};
/** Marker class: the type's class plus, for alerts, the severity */
const evCls = (ev) => `${(EV_KIND[ev.type] || { cls: "start" }).cls}${ev.type === "alert_firing" && ev.severity ? ` ${ev.severity}` : ""}`;
const TL_STEPS = [60, 300, 600, 900, 1800, 3600, 3 * 3600, 6 * 3600, 12 * 3600, 86400, 2 * 86400, 7 * 86400, 14 * 86400, 30 * 86400];

const tl = {
  data: null,
  ctrl: null,
  logsCtrl: null,
  service: "",
  cursor: null,      // cursor position in Unix seconds, or null
  fixed: false,      // the cursor was pinned with a click
  zoomStack: [],     // previous ranges for "back"
  extra: [],         // lanes added during the session: { title, query, unit, points }
  inited: false,
  drag: null,
};

const tlNarrow = () => matchMedia("(max-width: 860px)").matches;

/** View range: the global one, NOW_DELAY behind "now" like the home page */
function tlTime() {
  const t = resolveTime();
  return time.preset ? { start: t.start - NOW_DELAY, end: t.end - NOW_DELAY } : t;
}

// ── Data ───────────────────────────────────────────────────────────────────
async function fetchEvents() {
  tl.ctrl?.abort();
  const ctrl = tl.ctrl = new AbortController();
  const t = tlTime();
  const qs = new URLSearchParams({ start: t.start, end: t.end, lang: LANG });
  if (tl.service) qs.set("service", tl.service);
  const r = await fetch(`${EVENTS_URL}?${qs}`, { signal: ctrl.signal });
  const text = await r.text();
  let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 200) || r.statusText); }
  if (!r.ok) throw new Error(j.error || r.statusText);
  // Session lanes are fetched separately, with the same step
  const extra = await Promise.all(tl.extra.map(async (l) => {
    try {
      const series = await promRange(l.query, t.start, t.end, j.bucket, ctrl.signal);
      return { ...l, points: series[0]?.points || [] };
    } catch (e) { return { ...l, points: [], error: e.message }; }
  }));
  if (ctrl.signal.aborted) throw Object.assign(new Error("abort"), { name: "AbortError" });
  tl.extra = extra;
  return j;
}

// ── View ───────────────────────────────────────────────────────────────────
function initTimeline() {
  if (tl.inited) return;
  tl.inited = true;
  const lanes = $("#tl-lanes");
  // Cursor: follows the mouse and is pinned with a click; dragging zooms
  lanes.addEventListener("mousemove", (e) => {
    if (tl.drag) { dragMove(e); return; }
    if (tl.fixed) return;
    const t = xToTime(e.clientX); if (t == null) return;
    setCursor(t, false);
  });
  lanes.addEventListener("mouseleave", () => { if (!tl.fixed && !tl.drag) { tl.cursor = null; paintCursorLine(); } });
  lanes.addEventListener("mousedown", (e) => {
    if (e.button !== 0 || e.target.closest(".tl-ev")) return;
    tl.drag = { x0: e.clientX, moved: false };
    e.preventDefault();
  });
  document.addEventListener("mouseup", (e) => {
    if (!tl.drag) return;
    const d = tl.drag; tl.drag = null;
    const sel = $("#tl-select"); sel.hidden = true;
    if (d.moved && Math.abs(e.clientX - d.x0) > 8) {
      const a = xToTime(Math.min(d.x0, e.clientX), true), b = xToTime(Math.max(d.x0, e.clientX), true);
      if (a != null && b != null && b - a >= 300) zoomTo(a, b);
      return;
    }
    // A click pins or releases the cursor
    const t = xToTime(e.clientX); if (t == null) return;
    if (tl.fixed && Math.abs(t - tl.cursor) < (tl.data?.bucket || 60)) { tl.fixed = false; setCursor(t, false); }
    else setCursor(t, true);
  });
  lanes.addEventListener("dblclick", (e) => { if (!e.target.closest(".tl-ev")) zoomReset(); });
  lanes.addEventListener("touchstart", (e) => { const t = xToTime(e.touches[0].clientX); if (t != null) setCursor(t, true); }, { passive: true });
  // Lane events: click → zoom around them
  lanes.addEventListener("click", (e) => {
    const b = e.target.closest(".tl-ev"); if (!b) return;
    e.stopPropagation();
    const ev = tl.data?.events?.[+b.dataset.i]; if (!ev) return;
    zoomTo(ev.time - 900, (ev.end || ev.time) + 900);
  });
  $("#tl-back").onclick = zoomBack;
  $("#tl-service-clear").onclick = () => { tl.service = ""; syncUrl(); timelineRefresh(); };
  // Event list (mobile) and cursor events open the drawer
  $("#view-timeline").addEventListener("click", (e) => {
    const b = e.target.closest("[data-ev]"); if (!b) return;
    const ev = tl.data?.events?.[+b.dataset.ev]; if (ev) openServiceDrawer(ev.service, ev.host);
  });
  // "+ lane": PromQL for this session
  const pop = $("#tl-lane-pop");
  $("#tl-add").onclick = (e) => { e.stopPropagation(); pop.hidden = !pop.hidden; if (!pop.hidden) $("#tl-lane-q").focus(); };
  pop.onclick = (e) => e.stopPropagation();
  document.addEventListener("click", () => { pop.hidden = true; });
  $("#tl-lane-ok").onclick = () => {
    const query = $("#tl-lane-q").value.trim(); if (!query) return;
    const title = $("#tl-lane-t").value.trim() || query.slice(0, 40);
    tl.extra.push({ title, query, unit: $("#tl-lane-u").value, points: [], session: true });
    $("#tl-lane-q").value = ""; $("#tl-lane-t").value = ""; pop.hidden = true;
    timelineRefresh();
  };
  lanes.addEventListener("click", (e) => {
    const x = e.target.closest(".tl-lane-x"); if (!x) return;
    tl.extra.splice(+x.dataset.x, 1); tlRender();
  });
  document.addEventListener("keydown", (e) => {
    if (current !== "timeline" || (drawer.el && !drawer.el.hidden)) return;
    if (e.target.matches("input, textarea, select")) return;
    if (e.key === "Escape") { if (!pop.hidden) { pop.hidden = true; return; } if (tl.fixed) { tl.fixed = false; tl.cursor = null; paintCursorLine(); paintStrip(); } return; }
    if (!tl.data) return;
    const b = tl.data.bucket || 60;
    if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
      e.preventDefault();
      const t0 = tl.cursor ?? tl.data.end;
      setCursor(Math.min(tl.data.end, Math.max(tl.data.start, t0 + (e.key === "ArrowLeft" ? -b : b))), true);
    }
  });
}

function dragMove(e) {
  const d = tl.drag;
  if (Math.abs(e.clientX - d.x0) > 4) d.moved = true;
  const body = $("#tl-body-ref"); const r = body.getBoundingClientRect();
  const sel = $("#tl-select");
  const x1 = Math.max(r.left, Math.min(d.x0, e.clientX)), x2 = Math.min(r.right, Math.max(d.x0, e.clientX));
  sel.hidden = !d.moved;
  sel.style.left = `${laneX((x1 - r.left) / r.width)}px`;
  sel.style.width = `${x2 - x1}px`;
}

/** Range fraction → pixels within #tl-lanes (the cursor and selection live there so
 *  they span every lane, but they are scaled to the data column) */
function laneX(f) {
  const lanes = $("#tl-lanes"), r = $("#tl-body-ref").getBoundingClientRect();
  return r.left - lanes.getBoundingClientRect().left - lanes.clientLeft + f * r.width;
}

/** Pixel → seconds over the lane column; `clamp` for drags that leave it */
function xToTime(clientX, clamp) {
  const d = tl.data; if (!d) return null;
  const r = $("#tl-body-ref").getBoundingClientRect();
  let f = (clientX - r.left) / r.width;
  if (clamp) f = Math.max(0, Math.min(1, f)); else if (f < 0 || f > 1) return null;
  return Math.round(d.start + f * (d.end - d.start));
}
const pctOf = (t) => { const d = tl.data; return ((t - d.start) / (d.end - d.start)) * 100; };
/** Event time; includes the day when the range spans more than a day */
const tlStamp = (sec) => (tl.data && tl.data.end - tl.data.start > 86400 ? fmtDate(sec * 1000, { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" }) : hhmm(sec));
const tlDated = () => (tl.data && tl.data.end - tl.data.start > 86400 ? " dated" : "");

function setCursor(t, fixed) {
  tl.cursor = t; tl.fixed = fixed;
  paintCursorLine();
  paintStrip();
}

function syncUrl() {
  const qs = new URLSearchParams(); if (tl.service) qs.set("service", tl.service);
  history.replaceState(null, "", `#/timeline${qs.toString() ? "?" + qs : ""}`);
}

async function showTimeline(params) {
  initTimeline();
  if (params.has("service")) tl.service = params.get("service");
  // With no range chosen in the session, the timeline opens on the last day
  if (!timeChosen() && time.preset === "1h") { time = { preset: "1d" }; $("#time-label").textContent = timeLabel(); }
  await timelineRefresh();
}

async function timelineRefresh() {
  const status = $("#tl-status");
  status.className = "status"; status.textContent = tl.data ? "" : t("loading");
  try {
    tl.data = await fetchEvents();
    status.textContent = "";
    tlRender();
  } catch (e) {
    if (e.name === "AbortError") return;
    status.className = "status error"; status.textContent = e.message;
  }
}

// ── Zoom ───────────────────────────────────────────────────────────────────
function zoomTo(a, b) {
  tl.zoomStack.push(time);
  tl.fixed = false; tl.cursor = null;
  setTime({ start: Math.floor(a), end: Math.ceil(b) });
  $("#tl-back").hidden = false;
}
function zoomBack() {
  const prev = tl.zoomStack.pop(); if (!prev) return;
  tl.fixed = false; tl.cursor = null;
  setTime(prev);
  $("#tl-back").hidden = tl.zoomStack.length === 0;
}
function zoomReset() {
  if (!tl.zoomStack.length) return;
  const first = tl.zoomStack[0]; tl.zoomStack = [];
  tl.fixed = false; tl.cursor = null;
  setTime(first);
  $("#tl-back").hidden = true;
}

// ── Rendering ──────────────────────────────────────────────────────────────
/** Axis ticks: round local hours or days */
function axisTicks(start, end) {
  const span = end - start;
  const step = TL_STEPS.find((s) => span / s <= 8) || TL_STEPS.at(-1);
  const tz = new Date().getTimezoneOffset() * 60;
  const ticks = [];
  for (let x = Math.ceil((start - tz) / step) * step + tz; x <= end; x += step) ticks.push(x);
  const fmt = (x) => {
    const d = new Date(x * 1000);
    if (step >= 86400) return fmtDate(d, { day: "numeric", month: "short" });
    const hm = fmtDate(d, { hour: "2-digit", minute: "2-digit" });
    return span > 86400 && d.getHours() === 0 && d.getMinutes() === 0 ? fmtDate(d, { day: "numeric", month: "short" }) : hm;
  };
  return ticks.map((x) => ({ x, label: fmt(x) }));
}

/** Log lane bars: the total, with errors overlaid in red */
function logLaneSvg(points, bucket) {
  const d = tl.data, W = 1000, H = 36;
  const max = Math.max(1, ...points.map((p) => p[1]));
  const bw = Math.max(1.5, (bucket / (d.end - d.start)) * W - 1);
  let out = "";
  for (const [t, n, e] of points) {
    const x = ((t - d.start) / (d.end - d.start)) * W;
    const h = (n / max) * (H - 2), he = (e / max) * (H - 2);
    out += `<rect x="${x.toFixed(1)}" y="${(H - h).toFixed(1)}" width="${bw.toFixed(1)}" height="${h.toFixed(1)}" class="b"/>`;
    if (e > 0) out += `<rect x="${x.toFixed(1)}" y="${(H - h).toFixed(1)}" width="${bw.toFixed(1)}" height="${Math.max(1.5, he).toFixed(1)}" class="e"/>`;
  }
  return `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true">${out}</svg>`;
}

/** Area and line for a metric lane. A gap longer than 1.5 buckets breaks the line
 *  (no value is invented between distant samples); percentages start at 0 even if
 *  a spurious value is negative (the SVG clips it). */
function metricLaneSvg(points, unit, bucket) {
  const d = tl.data, W = 1000, H = 56;
  const vs = points.filter((p) => p[1] != null && Number.isFinite(p[1])).sort((a, b) => a[0] - b[0]);
  if (vs.length < 2) return `<span class="tl-nodata">${t("nodata")}</span>`;
  const vals = vs.map((p) => p[1]);
  let lo = unit === "percent" ? 0 : Math.min(0, ...vals), hi = Math.max(...vals);
  if (hi <= lo) hi = lo + 1;
  const X = (t) => ((t - d.start) / (d.end - d.start)) * W, Y = (v) => H - 2 - ((v - lo) / (hi - lo)) * (H - 6);
  const gap = (bucket || 60) * 1.5;
  const runs = [];
  for (const p of vs) {
    const r = runs.at(-1);
    if (r && p[0] - r.at(-1)[0] <= gap) r.push(p); else runs.push([p]);
  }
  let area = "", line = "";
  for (const r of runs) {
    const seg = r.map((p, i) => `${i ? "L" : "M"}${X(p[0]).toFixed(1)},${Y(p[1]).toFixed(1)}`).join("");
    line += seg;
    area += `${seg}L${X(r.at(-1)[0]).toFixed(1)},${H}L${X(r[0][0]).toFixed(1)},${H}Z`;
  }
  return `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true"><path class="a" d="${area}"/><path class="l" d="${line}" vector-effect="non-scaling-stroke"/></svg>`;
}

function evMarker(ev, i) {
  const k = EV_KIND[ev.type] || { cls: "start", name: ev.type };
  const left = pctOf(ev.time);
  // Counts only on deploys and restarts; on alerts they would overlap the neighbours
  const count = k.lane === "deploy" && ev.count && ev.count > 1 ? `<i>${ev.count}</i>` : "";
  return `<button class="tl-ev ${evCls(ev)}" data-i="${i}" data-tip="${esc(k.name)}: ${esc(ev.title)} · ${esc(tlStamp(ev.time))}" aria-label="${esc(k.name)}: ${esc(ev.title)}, ${esc(tlStamp(ev.time))}" data-left="${left.toFixed(3)}">${count}</button>`;
}

function tlRender() {
  const d = tl.data; if (!d) return;
  const chip = $("#tl-service"); chip.hidden = !tl.service; $("#tl-service-name").textContent = tl.service;
  $("#tl-back").hidden = tl.zoomStack.length === 0;
  $("#tl-count").textContent = `${tn("tl.count", d.events.length)} · ${t("tl.buckets", { b: d.bucket >= 3600 ? fmtDec(d.bucket / 3600, 1) + " h" : d.bucket / 60 + " min" })}`;
  if (tlNarrow()) { renderMobile(); } else { renderLanes(); }
  paintCursorLine();
  paintStrip();
}

function renderLanes() {
  const d = tl.data;
  const axis = axisTicks(d.start, d.end);
  const deploys = d.events.map((e, i) => [e, i]).filter(([e]) => EV_KIND[e.type]?.lane === "deploy");
  const alerts = d.events.map((e, i) => [e, i]).filter(([e]) => EV_KIND[e.type]?.lane === "alert");
  const lanes = [];
  lanes.push(`<div class="tl-row axis"><div class="tl-lb"></div><div class="tl-body" id="tl-body-ref">${axis.map((t) => `<span class="tick" data-left="${pctOf(t.x).toFixed(3)}">${esc(t.label)}</span>`).join("")}</div></div>`);
  lanes.push(`<div class="tl-row ev"><div class="tl-lb">${t("tl.deploys")}</div><div class="tl-body">${deploys.map(([e, i]) => evMarker(e, i)).join("")}</div></div>`);
  for (const l of d.log_lanes || []) {
    lanes.push(`<div class="tl-row log"><div class="tl-lb"><span class="dim">${t("tl.logs")}</span> ${esc(l.service)}</div><div class="tl-body">${logLaneSvg(l.points, d.bucket)}</div></div>`);
  }
  lanes.push(`<div class="tl-row ev"><div class="tl-lb"><a href="#/alerts">${t("tl.alerts")}</a></div><div class="tl-body">${alerts.map(([e, i]) => evMarker(e, i)).join("")}</div></div>`);
  const metrics = [...(d.metric_lanes || []), ...tl.extra];
  metrics.forEach((m, mi) => {
    const x = m.session ? `<button class="tl-lane-x icon-btn" data-x="${mi - (d.metric_lanes || []).length}" aria-label="${esc(t("tl.remove_lane", { t: m.title }))}">×</button>` : "";
    const err = m.error ? `<span class="werror">${esc(m.error)}</span>` : "";
    lanes.push(`<div class="tl-row metric"><div class="tl-lb">${esc(m.title)}${x}</div><div class="tl-body">${err || metricLaneSvg(m.points, m.unit, d.bucket)}</div></div>`);
  });
  const legend = `<div class="tl-legend" aria-label="${esc(t("tl.legend"))}">${["errors", "log_burst", "alert_firing", "alert_resolved"].map((k) => `<span><i class="tl-dot ${EV_KIND[k].cls}" aria-hidden="true"></i>${esc(EV_KIND[k].name)}</span>`).join("")}</div>`;
  $("#tl-lanes").innerHTML = lanes.join("") + legend + `<div class="tl-cursor" id="tl-cursor" hidden><span class="tl-cursor-t" id="tl-cursor-t"></span></div><div class="tl-select" id="tl-select" hidden></div>`;
  // Positions set through the CSSOM (no style= in the HTML, for the CSP)
  $$("#tl-lanes [data-left]").forEach((el) => { el.style.left = `${el.dataset.left}%`; });
  $("#tl-mobile").hidden = true; $("#tl-desk").hidden = false;
}

function renderMobile() {
  const d = tl.data;
  // Mini axis: log lines per bucket (sum of all lanes)
  const sum = new Map();
  for (const l of d.log_lanes || []) for (const [t, n] of l.points) sum.set(t, (sum.get(t) || 0) + n);
  const pts = [...sum.entries()].sort((a, b) => a[0] - b[0]).map(([t, n]) => [t, n, 0]);
  const withDate = d.end - d.start > 86400;
  const lbl = (x) => fmtDate(x * 1000, withDate ? { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" } : { hour: "2-digit", minute: "2-digit" });
  const axis = [d.start, (d.start + d.end) / 2, d.end];
  $("#tl-mini").innerHTML = logLaneSvg(pts, d.bucket) + `<div class="tl-mini-axis">${axis.map((x) => `<span>${esc(lbl(x))}</span>`).join("")}</div>`;
  const list = d.events.length ? d.events.map((e, i) => {
    return `<button class="tl-item${tlDated()}" data-ev="${i}"><time>${esc(tlStamp(e.time))}</time><i class="tl-dot ${evCls(e)}" aria-hidden="true"></i><span>${esc(e.title)}</span></button>`;
  }).join("") : `<p class="dim">${t("tl.noevents")}</p>`;
  $("#tl-list").innerHTML = list;
  $("#tl-mobile").hidden = false; $("#tl-desk").hidden = true;
}

function paintCursorLine() {
  const c = $("#tl-cursor"); if (!c) return;
  if (tl.cursor == null || tlNarrow()) { c.hidden = true; return; }
  c.hidden = false;
  c.style.left = `${laneX(pctOf(tl.cursor) / 100)}px`;
  c.classList.toggle("fixed", tl.fixed);
  $("#tl-cursor-t").textContent = hhmm(tl.cursor);
}

let stripTimer = null;
function paintStrip() {
  const d = tl.data, strip = $("#tl-strip");
  if (!d || tl.cursor == null || tlNarrow()) { strip.hidden = true; return; }
  strip.hidden = false;
  const b = d.bucket, cur = tl.cursor;
  $("#tl-strip-time").textContent = `${hhmm(cur)} · ${fmtDate(cur * 1000, { day: "numeric", month: "short" })}`;
  const near = d.events.map((e, i) => [e, i]).filter(([e]) => Math.abs(e.time - cur) <= b || (e.end && e.time <= cur && e.end >= cur));
  $("#tl-strip-ev").innerHTML = near.length
    ? near.map(([e, i]) => `<button class="tl-item" data-ev="${i}"><time>${esc(hhmm(e.time))}</time><i class="tl-dot ${evCls(e)}" aria-hidden="true"></i><span>${esc(e.title)}</span><small>${esc(e.detail || "")}</small></button>`).join("")
    : `<p class="dim">${t("tl.nothing")}</p>`;
  const vals = [...(d.metric_lanes || []), ...tl.extra].map((m) => {
    const p = m.points.reduce((best, q) => (best == null || Math.abs(q[0] - cur) < Math.abs(best[0] - cur) ? q : best), null);
    return `<div><span class="lb">${esc(m.title)}</span><b>${p ? esc(fmtUnit(p[1], m.unit || "short")) : "—"}</b></div>`;
  });
  for (const l of d.log_lanes || []) {
    const p = l.points.find((q) => q[0] <= cur && cur < q[0] + b);
    if (p) vals.push(`<div><span class="lb">${t("tl.logs")} ${esc(l.service)}</span><b>${fmtInt(p[1])}${p[2] ? ` <span class="werror">(${fmtInt(p[2])} ${t("tl.err")})</span>` : ""}</b></div>`);
  }
  $("#tl-strip-vals").innerHTML = vals.join("") || `<p class="dim">${t("tl.nometrics")}</p>`;
  // The bucket's logs, slightly debounced so there is not one request per pixel
  clearTimeout(stripTimer);
  stripTimer = setTimeout(() => loadCursorLogs(cur - Math.floor(b / 2), cur + Math.ceil(b / 2)), tl.fixed ? 0 : 180);
}

async function loadCursorLogs(start, end) {
  tl.logsCtrl?.abort(); const ctrl = tl.logsCtrl = new AbortController();
  const el = $("#tl-strip-logs"); el.innerHTML = `<p class="dim">${t("loading")}</p>`;
  const qs = new URLSearchParams({ start, end, limit: 8 });
  if (tl.service) qs.set("service", tl.service);
  try {
    const r = await fetch(`/api/v1/logs?${qs}`, { signal: ctrl.signal });
    const j = await r.json();
    if (!r.ok) throw new Error(j.error || r.statusText);
    el.innerHTML = j.logs.length
      ? j.logs.map((l) => `<div class="tl-log"><time>${esc(fmtDate(l.ts, { hour: "2-digit", minute: "2-digit", second: "2-digit" }))}</time><em>${esc(l.service)}</em><span class="lv ${esc((l.level || "").toLowerCase())}">${esc(l.level || "")}</span><span class="msg">${esc(l.body)}</span></div>`).join("")
      : `<p class="dim">${t("tl.nologs")}</p>`;
  } catch (e) {
    if (e.name !== "AbortError") el.innerHTML = `<p class="werror">${esc(e.message)}</p>`;
  }
}

/** The service drawer (from home.js) needs the home page data */
async function openServiceDrawer(job, host) {
  initHome(); // the drawer lives in the home view
  try {
    if (!home.data) home.data = await fetchOverview();
  } catch { /* no home data, no drawer */ return; }
  const key = `${job}@${host}`;
  if ((home.data.services || []).some((s) => svcKey(s) === key)) openDrawerFor(key);
  else {
    // A service that no longer exists (e.g. retired): show at least its logs
    logState.service = job; logState.host = ""; logState.level = ""; $("#log-q").value = "";
    location.hash = "#/logs";
  }
}

// When the width changes (e.g. rotating a phone), re-render in the matching mode
let tlMode = tlNarrow();
window.addEventListener("resize", () => { const n = tlNarrow(); if (n !== tlMode) { tlMode = n; if (current === "timeline" && tl.data) tlRender(); } });
