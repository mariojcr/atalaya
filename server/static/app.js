// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Atalaya: a Cloud Monitoring-style UI, no frameworks.
"use strict";

// With sign-in on, an API call without a session gets 401: back through the
// sign-in to the same page
{
  const nativeFetch = window.fetch.bind(window);
  window.fetch = async (...args) => {
    const r = await nativeFetch(...args);
    if (r.status === 401 && new URL(r.url, location.href).pathname.startsWith("/api/")) {
      location.href = "/auth/login?to=" + encodeURIComponent(location.pathname + location.search + location.hash);
    }
    return r;
  };
}

const $ = (s, root = document) => root.querySelector(s);
const $$ = (s, root = document) => [...root.querySelectorAll(s)];
// Series colors: the --s1..--s8 tokens of the active theme (read at paint time)
const PALETTE_VARS = ["--s1", "--s2", "--s3", "--s4", "--s5", "--s6", "--s7", "--s8"];
const palette = () => { const p = PALETTE_VARS.map(css).filter(Boolean); return p.length ? p : ["#4a93d4", "#c96e24", "#3fa36b", "#b06bd0"]; };
const SEV_ORDER = ["FATAL", "ERROR", "WARN", "INFO", "DEBUG", "TRACE", ""];
const SEV_NAME = { FATAL: t("sev.FATAL"), ERROR: t("sev.ERROR"), WARN: t("sev.WARN"), INFO: t("sev.INFO"), DEBUG: t("sev.DEBUG"), TRACE: t("sev.TRACE"), "": t("sev.none") };
const LETTERS = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";

function esc(s) { return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])); }
function css(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }
function store(k, v) { try { if (v === undefined) return JSON.parse(localStorage.getItem(k)); localStorage.setItem(k, JSON.stringify(v)); } catch { return null; } }

async function api(path, body) {
  const opts = body ? { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) } : {};
  const r = await fetch(path, opts);
  const text = await r.text();
  let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 300)); }
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}

// ── Formatting ─────────────────────────────────────────────────────────────
function unitOf(res) {
  const m = res.metric || "", u = res.unit || "", rate = res.aligner === "ALIGN_RATE";
  if (/bytes/.test(m) || u === "By") return rate ? "B/s" : "B";
  if (/seconds/.test(m) || u === "s") return rate ? "s/s" : "s";
  // Physical units of Atalaya's own metrics (°C, W, V, rpm, Hz, Wh); joules per second are watts
  if (u === "J") return rate ? "W" : "J";
  if (u && u !== "1") return rate ? `${u}/s` : u;
  return rate ? "/s" : "";
}
function fmt(v, unit = "") {
  if (v == null || Number.isNaN(v)) return "—";
  if (unit.startsWith("B")) {
    const p = ["", "Ki", "Mi", "Gi", "Ti", "Pi"]; let i = 0; let x = Math.abs(v);
    while (x >= 1024 && i < p.length - 1) { x /= 1024; i++; }
    return `${fmtNum(Math.sign(v) * x, x >= 100 || !i ? 0 : 1)} ${p[i]}${unit}`;
  }
  const a = Math.abs(v);
  const s = a >= 1e9 ? fmtNum(v / 1e9, 2) + " G" : a >= 1e6 ? fmtNum(v / 1e6, 2) + " M" : a >= 1e4 ? fmtNum(v / 1e3, 1) + " k"
    : a >= 100 ? fmtNum(v, 0) : a >= 1 ? fmtNum(v, 2) : a === 0 ? "0" : fmtSig(v, 3);
  return unit ? `${s} ${unit}`.replace("  ", " ") : s;
}
// Duration axes with round ticks (30 min, 1 h, 6 h…) instead of 33.3 min or 1.11 h
const AXIS_DUR_STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800, 604800, 2592000, 31536000];
const AXIS_DUR_INCRS = { s: AXIS_DUR_STEPS, dtdurations: AXIS_DUR_STEPS, ms: [1, 2, 5, 10, 20, 50, 100, 200, 500, ...AXIS_DUR_STEPS.map((x) => x * 1000)], dtdurationms: [1, 2, 5, 10, 20, 50, 100, 200, 500, ...AXIS_DUR_STEPS.map((x) => x * 1000)] };
function fmtTime(sec, withDate) {
  const d = new Date(sec * 1000);
  return withDate ? fmtDate(d, { dateStyle: "short", timeStyle: "medium" }) : fmtDate(d, { timeStyle: "medium" });
}
const shortField = (f) => f.replace(/^(resource|metric)\./, "");

// ── Time range (global to all views, like Cloud Monitoring) ────────────────
const PRESETS = ["15m", "1h", "6h", "12h", "1d", "2d", "7d", "30d", "42d", "365d"].map((k) => [k, t(`p.${k}`)]);
/** Short preset label ("15 minutes"), without the leading "Last" (p.strip, per language) */
const presetShort = (k) => (PRESETS.find((p) => p[0] === k)?.[1] || k).replace(new RegExp(t("p.strip"), "i"), "");
const DUR = { s: 1, m: 60, h: 3600, d: 86400, w: 604800 };
function durSecs(p) { const m = /^(\d+)\s*([smhdw])$/.exec(String(p).trim()); return m ? +m[1] * DUR[m[2]] : NaN; }
/** Server settings, in index.html: seconds relative ranges stay behind "now"
 *  (ATALAYA_NOW_DELAY, the time for data to arrive) and between refreshes of the
 *  pages that are not dashboards (ATALAYA_REFRESH) */
function metaSecs(name, def) {
  const v = Number(document.querySelector(`meta[name="atalaya-${name}"]`)?.content);
  return Number.isFinite(v) && v >= 0 ? v : def;
}
const NOW_DELAY = metaSecs("now-delay", 45), PAGE_REFRESH = metaSecs("refresh", 60) || 60;
let time = store("atalaya.time") || { preset: "1h" };

function resolveTime() {
  if (time.preset) { const end = Math.floor(Date.now() / 1000); return { start: end - durSecs(time.preset), end }; }
  return { start: time.start, end: time.end };
}
function timeLabel() {
  if (time.preset) return PRESETS.find((p) => p[0] === time.preset)?.[1] || time.preset;
  return `${fmtTime(time.start, true)} – ${fmtTime(time.end, true)}`;
}
function setTime(t) {
  time = t; store("atalaya.time", t);
  try { sessionStorage.setItem("atalaya.timeChosen", "1"); } catch { /* no storage */ }
  $("#time-label").textContent = timeLabel(); refresh();
}
function timeChosen() { try { return sessionStorage.getItem("atalaya.timeChosen") === "1"; } catch { return false; } }
const toLocalInput = (sec) => { const d = new Date(sec * 1000); d.setMinutes(d.getMinutes() - d.getTimezoneOffset()); return d.toISOString().slice(0, 16); };

function initTimePicker() {
  const menu = $("#time-menu"), btn = $("#time-button");
  $("#time-presets").innerHTML = PRESETS.map(([k, l]) => `<button data-p="${k}">${l}</button>`).join("");
  const open = (v) => {
    menu.hidden = !v; btn.setAttribute("aria-expanded", String(v));
    if (v) {
      const r = resolveTime(); $("#time-start").value = toLocalInput(r.start); $("#time-end").value = toLocalInput(r.end);
      $$("#time-presets button").forEach((b) => b.setAttribute("aria-current", String(b.dataset.p === time.preset)));
    }
  };
  btn.onclick = (e) => { e.stopPropagation(); open(menu.hidden); };
  menu.onclick = (e) => e.stopPropagation();
  document.addEventListener("click", () => open(false));
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && !menu.hidden) { open(false); btn.focus(); } });
  $("#time-presets").onclick = (e) => { const p = e.target.dataset.p; if (p) { open(false); setTime({ preset: p }); } };
  $("#time-apply").onclick = () => {
    const s = Date.parse($("#time-start").value) / 1000, en = Date.parse($("#time-end").value) / 1000;
    if (s && en && s < en) { open(false); setTime({ start: Math.floor(s), end: Math.floor(en) }); }
  };
  $("#time-label").textContent = timeLabel();
  $("#auto-refresh").onclick = (e) => {
    const on = e.currentTarget.getAttribute("aria-pressed") !== "true";
    e.currentTarget.setAttribute("aria-pressed", String(on));
    setAutoEvery(autoEvery);
  };
}
// Auto refresh: every PAGE_REFRESH or whatever the open dashboard says
let autoEvery = PAGE_REFRESH, autoTimer = null, missedRefresh = false;
function setAutoEvery(secs) {
  autoEvery = secs > 0 ? secs : PAGE_REFRESH;
  const btn = $("#auto-refresh");
  btn.dataset.tip = t("auto.tip", { t: autoEvery < 60 ? `${autoEvery} ${t("sec_short")}` : `${autoEvery / 60} ${t("min_short")}` });
  clearInterval(autoTimer);
  // No queries while the tab is hidden: note it and refresh once when it is visible again
  if (btn.getAttribute("aria-pressed") === "true") autoTimer = setInterval(() => { if (document.hidden) missedRefresh = true; else refresh(); }, autoEvery * 1000);
}
document.addEventListener("visibilitychange", () => { if (!document.hidden && missedRefresh) { missedRefresh = false; refresh(); } });

// ── Formulas: A / B * 100 ──────────────────────────────────────────────────
// Hand-written parser (no eval): numbers, query letters, + - * / and parentheses.
function parseFormula(src) {
  const toks = src.match(/\d+(?:\.\d+)?(?:e[+-]?\d+)?|[A-Z]|[-+*/()]|\S/gi) || [];
  let i = 0;
  const peek = () => toks[i], next = () => toks[i++];
  function primary() {
    const tok = next();
    if (tok === undefined) throw new Error(t("f.early"));
    if (tok === "(") { const e = expr(); if (next() !== ")") throw new Error(t("f.paren")); return e; }
    if (tok === "-") return { op: "neg", a: primary() };
    if (/^\d/.test(tok)) return { num: parseFloat(tok) };
    if (/^[A-Z]$/i.test(tok)) return { ref: tok.toUpperCase() };
    throw new Error(t("f.symbol", { t: tok }));
  }
  function term() { let a = primary(); while (peek() === "*" || peek() === "/") { const op = next(); a = { op, a, b: primary() }; } return a; }
  function expr() { let a = term(); while (peek() === "+" || peek() === "-") { const op = next(); a = { op, a, b: term() }; } return a; }
  const ast = expr();
  if (i < toks.length) throw new Error(t("f.extra", { t: toks[i] }));
  return ast;
}
function refsOf(ast, out = new Set()) { if (ast.ref) out.add(ast.ref); if (ast.a) refsOf(ast.a, out); if (ast.b) refsOf(ast.b, out); return out; }
function evalAst(ast, env) {
  if (ast.num !== undefined) return ast.num;
  if (ast.ref) return env[ast.ref] ?? null;
  const a = evalAst(ast.a, env); if (a == null) return null;
  if (ast.op === "neg") return -a;
  const b = evalAst(ast.b, env); if (b == null) return null;
  switch (ast.op) { case "+": return a + b; case "-": return a - b; case "*": return a * b; case "/": return b === 0 ? null : a / b; }
  return null;
}
const labelKey = (labels) => JSON.stringify(Object.entries(labels).sort());
/** Evaluates a formula over the query results, matching series by their labels;
 *  a query with a single series applies to all of them. */
function evalFormula(expr, byLetter) {
  const ast = parseFormula(expr);
  const refs = [...refsOf(ast)];
  if (!refs.length) throw new Error(t("f.noref"));
  const idx = {};
  for (const L of refs) {
    const r = byLetter[L];
    if (!r) throw new Error(t("f.missing", { L }));
    idx[L] = { single: r.timeSeries.length === 1 ? r.timeSeries[0] : null, byKey: new Map(r.timeSeries.map((s) => [labelKey(s.labels), s])) };
  }
  const keys = new Set();
  refs.forEach((L) => { if (!idx[L].single) idx[L].byKey.forEach((_, k) => keys.add(k)); });
  if (!keys.size) keys.add(labelKey({}));
  const out = [];
  for (const k of keys) {
    const ser = {};
    if (!refs.every((L) => (ser[L] = idx[L].single || idx[L].byKey.get(k)))) continue;
    const maps = {}; const ts = new Set();
    refs.forEach((L) => { maps[L] = new Map(ser[L].points); ser[L].points.forEach(([t]) => ts.add(t)); });
    const points = [...ts].sort((a, b) => a - b).map((t) => {
      const env = {}; refs.forEach((L) => { env[L] = maps[L].get(t) ?? null; });
      return [t, evalAst(ast, env)];
    });
    const labels = idx[refs.find((L) => !idx[L].single)]?.byKey.get(k)?.labels || {};
    out.push({ labels, points });
  }
  return out;
}

// ── Chart and legend ───────────────────────────────────────────────────────
function seriesName(labels, keys) {
  const parts = keys.map((k) => labels[k]).filter((v) => v !== undefined && v !== "");
  return parts.length ? parts.join(", ") : t("total");
}
// Labels that tell apart the series of a set (common ones are not shown)
function varyingKeys(series) {
  if (series.length < 2) return [];
  const keys = new Set(series.flatMap((s) => Object.keys(s.labels)));
  return [...keys].filter((k) => new Set(series.map((s) => s.labels[k] ?? "")).size > 1);
}

// Legend calculations (Grafana names) over a series' values
const CALCS = {
  value: { label: t("calc.value") },
  lastNotNull: { label: t("calc.last"), f: (vs) => vs.at(-1) },
  last: { label: t("calc.last"), f: (vs) => vs.at(-1) },
  firstNotNull: { label: t("calc.first"), f: (vs) => vs[0] },
  first: { label: t("calc.first"), f: (vs) => vs[0] },
  mean: { label: t("calc.mean"), f: (vs) => vs.reduce((a, b) => a + b, 0) / vs.length },
  max: { label: t("calc.max"), f: (vs) => Math.max(...vs) },
  min: { label: t("calc.min"), f: (vs) => Math.min(...vs) },
  sum: { label: t("calc.sum"), f: (vs) => vs.reduce((a, b) => a + b, 0) },
  count: { label: t("calc.count"), f: (vs) => vs.length, raw: true },
  range: { label: t("calc.range"), f: (vs) => Math.max(...vs) - Math.min(...vs) },
  delta: { label: t("calc.delta"), f: (vs) => vs.reduce((a, v, i) => a + (i && v > vs[i - 1] ? v - vs[i - 1] : 0), 0) },
  diff: { label: t("calc.diff"), f: (vs) => vs.at(-1) - vs[0] },
};
function calc(name, values) {
  const vs = values.filter((v) => v != null && Number.isFinite(v));
  const c = CALCS[name];
  return vs.length && c?.f ? c.f(vs) : null;
}
const withAlpha = (c, a) => {
  if (/^#[0-9a-f]{6}$/i.test(c)) return c + Math.round(a * 255).toString(16).padStart(2, "0");
  const m = /^rgba?\(([^)]+)\)$/.exec(c);
  if (m) { const p = m[1].split(",").map((x) => x.trim()); return `rgba(${p[0]}, ${p[1]}, ${p[2]}, ${a * (p[3] != null ? +p[3] : 1)})`; }
  return c;
};

/** Annotations: a dashed vertical line with a small flag on top per mark ({t, color}) */
function drawMarks(u, marks) {
  const ctx = u.ctx, { left, width, top, height } = u.bbox, dpr = devicePixelRatio;
  ctx.save();
  ctx.lineWidth = Math.max(1, dpr);
  for (const m of marks) {
    const x = Math.round(u.valToPos(m.t, "x", true)) + 0.5;
    if (x < left || x > left + width) continue;
    ctx.strokeStyle = ctx.fillStyle = m.color;
    ctx.setLineDash([3 * dpr, 3 * dpr]);
    ctx.beginPath(); ctx.moveTo(x, top); ctx.lineTo(x, top + height); ctx.stroke();
    ctx.setLineDash([]);
    ctx.beginPath(); ctx.moveTo(x - 4 * dpr, top); ctx.lineTo(x + 4 * dpr, top); ctx.lineTo(x, top + 5 * dpr); ctx.closePath(); ctx.fill();
  }
  ctx.restore();
}

/**
 * Draws several sets of series in one chart, with tooltip and legend.
 * set: { timeSeries: [{ labels, points, name?, style? }], name (prefix: "A", "F1"…), dashed, shift (s), keys, base }
 * style (per series): { color, fill (0-100), lineWidth, dash, negative, draw, stack, hidden: true|"legend", unit, decimals }
 * opts: { type: line|area|bar, log, logBase, height (px or "fit"), unit, decimals, start, end, colors, onZoom,
 *         stack: normal|percent, fill, lineWidth, draw: line|bars|points, interpolation, showPoints, spanNulls,
 *         min, max, softMin, softMax, thresholds: [{value, color}], thresholdsStyle: line|dashed, axis: "hidden",
 *         marks: [{t, title, detail, color}] (annotations: vertical lines with a tooltip),
 *         legend: { show, mode: list|table, calcs } }
 */
// No hover pointer (touch): the tooltip is pinned on tap and shown in a bar, not under the finger
const COARSE = matchMedia("(hover: none)").matches;
// Time axis ticks in 24 h and day/month (uPlot defaults to am/pm and month/day). Columns: step, tick
// format, then what is added below on a change of year, month, day, hour, minute and second; the last is the mode.
const UPLOT_TIME = [
  [31536000, "{YYYY}", null, null, null, null, null, null, 1],
  [2419200, "{MMM}", "\n{YYYY}", null, null, null, null, null, 1],
  [86400, "{D}/{M}", "\n{YYYY}", null, null, null, null, null, 1],
  [3600, "{HH}:{mm}", "\n{D}/{M}/{YY}", null, "\n{D}/{M}", null, null, null, 1],
  [60, "{HH}:{mm}", "\n{D}/{M}/{YY}", null, "\n{D}/{M}", null, null, null, 1],
  [1, ":{ss}", "\n{D}/{M}/{YY} {HH}:{mm}", null, "\n{D}/{M} {HH}:{mm}", null, "\n{HH}:{mm}", null, 1],
  [0.001, ":{ss}.{fff}", "\n{D}/{M}/{YY} {HH}:{mm}", null, "\n{D}/{M} {HH}:{mm}", null, "\n{HH}:{mm}", null, 1],
];
/** Frees a container's chart: uPlot (which listens for dppx changes on window) and its ResizeObserver are not released just by removing the node */
function destroyChart(host) {
  if (!host) return;
  host._u?.destroy(); host._ro?.disconnect(); host._u = host._ro = null;
}
function renderChart(host, legendHost, sets, opts = {}) {
  destroyChart(host);
  host.innerHTML = ""; if (legendHost) legendHost.innerHTML = "";
  const all = [];
  sets.forEach((set, si) => {
    const keys = set.keys?.length ? set.keys : varyingKeys(set.timeSeries);
    set.timeSeries.forEach((s, k) => {
      const st = s.style || {};
      if (st.hidden === true) return;
      all.push({ s, set, si, k, st, name: s.name ?? [set.name, seriesName(s.labels, keys)].filter(Boolean).join(": ").replace(/: total$/, "") });
    });
  });
  if (!all.length) { host.innerHTML = `<div class="empty"><p>${t("chart.nodata")}</p></div>`; return null; }
  const fmtOf = (i) => {
    const unit = all[i].st.unit ?? opts.unit ?? "", dec = all[i].st.decimals ?? opts.decimals;
    return opts.grafana ? (v) => fmtUnit(v, unit, dec) : (v) => fmt(v, unit);
  };
  const fmts = all.map((_, i) => fmtOf(i));
  const span = (opts.end ?? 0) - (opts.start ?? 0);
  const xs = [...new Set(all.flatMap(({ s, set }) => s.points.map((pt) => pt[0] + (set.shift || 0))))].sort((a, b) => a - b);
  const idx = new Map(xs.map((x, i) => [x, i]));
  const raw = all.map(({ s, set }) => {
    const ys = new Array(xs.length).fill(null);
    s.points.forEach(([t, v]) => { ys[idx.get(t + (set.shift || 0))] = opts.log && v != null && v <= 0 ? null : v; });
    return ys;
  });
  // Per-series color: from the style, opts.colors or the palette (comparison series reuse their original's)
  const PALETTE = palette();
  const colorOf = (i) => {
    const e = all[i];
    if (e.st.color) return e.st.color;
    const lbl = e.s.labels[Object.keys(e.s.labels)[0]];
    if (opts.colors && opts.colors[lbl]) return opts.colors[lbl];
    const target = e.set.base ?? e.set;
    const pos = all.filter((x) => !x.set.dashed).findIndex((x) => x.set === target && x.k === e.k);
    return PALETTE[(pos >= 0 ? pos : i) % PALETTE.length];
  };
  // Effective style of each series
  const defaultStack = opts.stack ?? (opts.type === "area" || opts.type === "bar" ? "normal" : null);
  const looks = all.map((e) => {
    const draw = e.st.draw ?? opts.draw ?? (opts.type === "bar" ? "bars" : "line");
    let stack = e.set.dashed ? null : (e.st.stack ?? defaultStack);
    if (stack === "none") stack = null;
    const fill = e.st.fill ?? opts.fill ?? (opts.type === "area" ? 35 : opts.type === "bar" ? 100 : 0);
    return { draw, stack, fill, neg: !!e.st.negative, width: e.st.lineWidth ?? opts.lineWidth ?? (e.set.dashed ? 1.2 : 1.6), dash: e.st.dash || e.set.dashed };
  });
  // Stacking per group (positive and negative separately), as a fraction of 1 for "percent"
  const plot = raw.map((ys, i) => ys.map((v) => (v == null ? null : looks[i].neg ? -v : v)));
  const groupKey = (i) => `${looks[i].stack}:${looks[i].neg ? "-" : "+"}`;
  const prevInGroup = new Map();
  const totals = new Map();
  all.forEach((_, i) => {
    if (!looks[i].stack || looks[i].stack !== "percent") return;
    const t = totals.get(groupKey(i)) || new Array(xs.length).fill(0);
    raw[i].forEach((v, j) => { t[j] += Math.abs(v ?? 0); });
    totals.set(groupKey(i), t);
  });
  const acc = new Map();
  all.forEach((_, i) => {
    const L = looks[i]; if (!L.stack) return;
    const g = groupKey(i);
    const a = acc.get(g) || new Array(xs.length).fill(0);
    const tot = totals.get(g);
    plot[i] = raw[i].map((v, j) => {
      if (v == null) return null;
      const x = tot ? (tot[j] ? Math.abs(v) / tot[j] : 0) : Math.abs(v);
      a[j] += L.neg || v >= 0 || tot ? x : v;
      return L.neg ? -a[j] : a[j];
    });
    L.prev = prevInGroup.get(g);
    prevInGroup.set(g, i);
    acc.set(g, a);
  });
  const hasNeg = looks.some((l) => l.neg);
  const series = [{}];
  const data = [xs];
  const bands = [];
  const bars = uPlot.paths.bars ? uPlot.paths.bars({ size: [0.8, 48] }) : undefined;
  const interp = opts.interpolation === "smooth" && uPlot.paths.spline ? uPlot.paths.spline()
    : opts.interpolation === "stepAfter" && uPlot.paths.stepped ? uPlot.paths.stepped({ align: 1 })
    : opts.interpolation === "stepBefore" && uPlot.paths.stepped ? uPlot.paths.stepped({ align: -1 }) : undefined;
  // Stacked bars are drawn from tallest to shortest (each one covers those below);
  // stacked areas are filled between a series and the previous one using bands
  const stackedBar = (i) => looks[i].stack && looks[i].draw === "bars";
  const order = [...all.keys()].filter(stackedBar).reverse().concat([...all.keys()].filter((i) => !stackedBar(i)));
  const pos = new Map(order.map((i, n) => [i, n + 1]));
  order.forEach((i) => {
    const e = all[i], c = colorOf(i), L = looks[i];
    data.push(plot[i]);
    const fillColor = L.fill > 0 ? (L.draw === "bars" && L.fill >= 100 ? c : withAlpha(c, Math.min(1, L.fill / 100))) : undefined;
    const banded = L.stack && L.prev != null && L.draw !== "bars";
    if (banded && fillColor) bands.push({ series: L.neg ? [pos.get(L.prev), pos.get(i)] : [pos.get(i), pos.get(L.prev)], fill: fillColor });
    series.push({
      label: e.name, stroke: c, width: L.draw === "points" ? 0 : L.width, dash: L.dash ? [5, 4] : undefined,
      fill: banded ? undefined : fillColor, fillTo: opts.log ? undefined : 0,
      paths: L.draw === "bars" ? bars : interp,
      points: { show: L.draw === "points" || opts.showPoints === "always", size: 5, fill: c },
      spanGaps: !!opts.spanNulls, _i: i,
    });
  });
  const ink = css("--muted"), grid = css("--grid");
  const fitHeight = () => Math.max(60, host.clientHeight || 200);
  const height = opts.height === "fit" ? fitHeight() : opts.height || 320;
  const percent = looks.some((l) => l.stack === "percent");
  // Series drawn downwards (negative-Y) keep the sign on the axis, as in Grafana
  const axisFmt = opts.grafana ? (v) => fmtUnit(v, opts.unit ?? "", opts.decimals) : (v) => fmt(v, opts.unit ?? "");
  const tooltip = document.createElement("div"); tooltip.className = COARSE ? "tooltip bar" : "tooltip"; tooltip.hidden = true;
  let pinned = false; // on touch, one tap pins the tooltip and another releases it
  // Y axis width from its labels (long units do not fit a fixed width)
  const axisSize = (u, values) => {
    if (!values) return 48;
    u.ctx.save(); u.ctx.font = u.axes[1].font[0];
    const w = Math.max(...values.map((v) => u.ctx.measureText(v).width)) / devicePixelRatio;
    u.ctx.restore();
    return Math.ceil(w) + 14;
  };
  const yScale = opts.log ? { distr: 3, log: opts.logBase || 10 } : {
    range: (u, mn, mx) => {
      let lo = opts.min ?? Math.min(0, mn * (mn < 0 ? 1.05 : 1), opts.softMin ?? 0);
      let hi = opts.max ?? Math.max(percent ? 1 : mx > 0 ? mx * 1.05 : mx < 0 ? 0 : 1, opts.softMax ?? -Infinity);
      if (percent && hasNeg && opts.min == null) lo = Math.min(lo, -1);
      if (hi <= lo) hi = lo + 1;
      return [lo, hi];
    },
  };
  const steps = (opts.thresholds || []).filter((s) => s.value != null && Number.isFinite(+s.value));
  const u = new uPlot({
    width: Math.max(200, host.clientWidth), height, series, bands,
    // Axis month and day names in the UI language (uPlot only ships English)
    ...(UPLOT_NAMES ? { fmtDate: (tpl) => uPlot.fmtDate(tpl, UPLOT_NAMES) } : {}),
    // The X axis spans the whole requested range, not just the part with data
    scales: { x: { time: true, range: (u, mn, mx) => (opts.start != null && opts.end != null && opts.end > opts.start ? [opts.start, opts.end] : [mn, mx]) }, y: yScale },
    axes: [
      { stroke: ink, grid: { stroke: grid, width: 1 }, ticks: { stroke: grid }, values: UPLOT_TIME },
      { show: opts.axis !== "hidden", stroke: ink, grid: { stroke: grid, width: 1 }, ticks: { show: false }, size: axisSize, values: (u, t) => t.map(axisFmt), ...(AXIS_DUR_INCRS[opts.unit] ? { incrs: AXIS_DUR_INCRS[opts.unit] } : {}) },
    ],
    legend: { show: false },
    cursor: { drag: { x: true, y: false }, focus: { prox: 24 }, points: { size: 6 } },
    focus: { alpha: 0.35 },
    hooks: {
      setCursor: [(u) => { updateLegend(u.cursor.idx); updateTooltip(u); }],
      setSelect: [(u) => {
        if (u.select.width > 8 && opts.onZoom) {
          const a = u.posToVal(u.select.left, "x"), b = u.posToVal(u.select.left + u.select.width, "x");
          opts.onZoom(Math.floor(a), Math.ceil(b));
        }
        u.setSelect({ width: 0, height: 0 }, false);
      }],
      // Thresholds as reference lines, and annotations
      draw: (steps.length && opts.thresholdsStyle ? [(u) => {
        const ctx = u.ctx, { left, width, top, height: h } = u.bbox;
        ctx.save();
        steps.forEach((s) => {
          const y = Math.round(u.valToPos(+s.value, "y", true)) + 0.5;
          if (y < top || y > top + h) return;
          ctx.strokeStyle = s.color; ctx.lineWidth = Math.max(1, devicePixelRatio);
          ctx.setLineDash(opts.thresholdsStyle === "dashed" ? [6 * devicePixelRatio, 4 * devicePixelRatio] : []);
          ctx.beginPath(); ctx.moveTo(left, y); ctx.lineTo(left + width, y); ctx.stroke();
        });
        ctx.restore();
      }] : []).concat(opts.marks?.length ? [(u) => drawMarks(u, opts.marks)] : []),
    },
  }, data, host);
  u.over.appendChild(tooltip);
  host._u = u;
  host._ro = new ResizeObserver(() => u.setSize({ width: Math.max(200, host.clientWidth), height: opts.height === "fit" ? fitHeight() : height }));
  host._ro.observe(host);
  if (COARSE) {
    u.over.addEventListener("click", () => { pinned = !pinned; if (!pinned) tooltip.hidden = true; else updateTooltip(u, true); });
  }

  const valueAt = (i, j) => (j == null ? null : raw[i][j]);
  function lastValue(i) { for (let j = raw[i].length - 1; j >= 0; j--) if (raw[i][j] != null) return raw[i][j]; return null; }
  // Colors set through the CSSOM: the Content-Security-Policy forbids style attributes
  function paintSwatches(host) {
    $$(".swatch[data-i]", host).forEach((e) => {
      const i = +e.dataset.i, c = colorOf(i);
      e.style.borderColor = c;
      e.style.background = all[i].set.dashed ? "transparent" : c;
    });
  }
  function updateTooltip(u, force) {
    const j = u.cursor.idx;
    if (COARSE && !pinned && !force) { tooltip.hidden = true; return; }
    if (j == null || u.cursor.left < 0) { if (!pinned) tooltip.hidden = true; return; }
    const rows = all.map((_, i) => [i, valueAt(i, j)]).filter(([i, v]) => v != null && u.series[series.findIndex((s) => s._i === i)]?.show !== false).sort((a, b) => b[1] - a[1]).slice(0, 12);
    // Annotations under the cursor
    const near = (opts.marks || []).filter((m) => Math.abs(u.valToPos(m.t, "x") - u.cursor.left) <= 6);
    if (!rows.length && !near.length) { tooltip.hidden = true; return; }
    tooltip.innerHTML = `<div class="t">${fmtTime(xs[j], span > 86400)}</div>` + near.map((m, k) =>
      `<div class="mk" data-k="${k}"><b>${esc(m.title)}</b>${m.detail ? `<span>${esc(m.detail)}</span>` : ""}</div>`).join("") + rows.map(([i, v]) =>
      `<div class="r"><span><i class="swatch" data-i="${i}"></i>${esc(all[i].name)}</span><b>${esc(fmts[i](v))}</b></div>`).join("");
    paintSwatches(tooltip);
    $$(".mk", tooltip).forEach((e) => { e.style.borderLeftColor = near[+e.dataset.k].color; });
    tooltip.hidden = false;
    if (COARSE) return; // the bar is fixed at the top (CSS)
    const left = u.cursor.left + 16, w = tooltip.offsetWidth;
    tooltip.style.left = (left + w > u.over.clientWidth ? u.cursor.left - w - 16 : left) + "px";
    tooltip.style.top = Math.max(0, u.cursor.top - 20) + "px";
  }

  // Legend: as a table (name and calculations, sortable) or as a list
  const lg = opts.legend || {};
  const cols = lg.calcs?.length ? lg.calcs : lg.mode === "table" || !opts.legend ? ["value", "min", "max", "mean"] : [];
  const listMode = lg.mode === "list" || (opts.legend && lg.mode !== "table");
  // The explorer sorts by value; dashboards keep query order until a column is clicked
  let sortCol = opts.legend ? null : "value", sortDir = -1;
  const stats = all.map((_, i) => Object.fromEntries(cols.filter((c) => c !== "value").map((c) => [c, calc(c, raw[i])])));
  const fmtCalc = (i, c, v) => (CALCS[c]?.raw ? String(v ?? "—") : fmts[i](v));
  function updateLegend(j) {
    if (!legendHost) return;
    legendHost.querySelectorAll("[data-v]").forEach((td) => { const i = +td.dataset.v; td.textContent = fmts[i](j == null ? lastValue(i) : valueAt(i, j)); });
  }
  function drawLegend() {
    if (!legendHost || lg.show === false) return;
    const rows = all.map((_, i) => i).filter((i) => all[i].st.hidden !== "legend");
    const key = (i) => sortCol === "name" ? all[i].name : sortCol === "value" ? (lastValue(i) ?? -Infinity) : (stats[i][sortCol] ?? -Infinity);
    if (!listMode && sortCol) rows.sort((a, b) => (key(a) > key(b) ? 1 : key(a) < key(b) ? -1 : 0) * sortDir);
    const sIdx = (i) => series.findIndex((s) => s._i === i);
    const off = (i) => series[sIdx(i)]?.show === false;
    if (listMode) {
      legendHost.innerHTML = `<div class="legend-list">` + rows.map((i) =>
        `<button class="li${off(i) ? " off" : ""}" data-s="${sIdx(i)}"><i class="swatch" data-i="${i}"></i><span class="n">${esc(all[i].name)}</span>${cols.map((c) =>
          `<span class="c">${esc(CALCS[c]?.label || c)} <b ${c === "value" ? `data-v="${i}"` : ""}>${esc(c === "value" ? fmts[i](lastValue(i)) : fmtCalc(i, c, stats[i][c]))}</b></span>`).join("")}</button>`).join("") + `</div>`;
    } else {
      const th = (col, label, num) => `<th data-col="${col}" tabindex="0" class="${num ? "num" : ""}" aria-sort="${sortCol === col ? (sortDir > 0 ? "ascending" : "descending") : "none"}">${label}</th>`;
      legendHost.innerHTML = `<table class="legend"><thead><tr>${th("name", t("th.serie"))}${cols.map((c) => th(c, CALCS[c]?.label || c, 1)).join("")}</tr></thead><tbody>` +
        rows.map((i) => `<tr data-s="${sIdx(i)}" tabindex="0" role="button" aria-pressed="${!off(i)}" class="${off(i) ? "off" : ""}"><td><i class="swatch" data-i="${i}"></i>${esc(all[i].name)}</td>${cols.map((c) =>
          c === "value" ? `<td class="num" data-v="${i}">${esc(fmts[i](lastValue(i)))}</td>` : `<td class="num">${esc(fmtCalc(i, c, stats[i][c]))}</td>`).join("")}</tr>`).join("") + "</tbody></table>";
      $$("th", legendHost).forEach((t) => {
        const sortBy = () => { const c = t.dataset.col; sortDir = sortCol === c ? -sortDir : (c === "name" ? 1 : -1); sortCol = c; drawLegend(); };
        t.onclick = sortBy;
        t.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); sortBy(); } };
      });
    }
    paintSwatches(legendHost);
    $$("[data-s]", legendHost).forEach((el) => {
      const si = +el.dataset.s;
      const toggle = () => { u.setSeries(si, { show: !(series[si].show ?? true) }); series[si].show = u.series[si].show; drawLegend(); };
      el.onclick = toggle;
      el.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); toggle(); } };
      el.onmouseenter = () => u.setSeries(si, { focus: true });
      el.onmouseleave = () => u.setSeries(null, { focus: false });
    });
  }
  drawLegend();
  return u;
}

// ── Metric catalog ─────────────────────────────────────────────────────────
// The catalog is fetched without descriptions (brief); the selected metric's is fetched separately
let descriptors = null;
async function loadDescriptors() {
  if (!descriptors) descriptors = (await api("/api/v1/metricDescriptors?brief=1")).metricDescriptors;
  return descriptors;
}
const descriptorOf = (name) => descriptors?.find((d) => d.name === name);
const descCache = new Map();
async function descriptionOf(name) {
  if (!name) return "";
  if (!descCache.has(name)) descCache.set(name, api(`/api/v1/metricDescriptors?q=${encodeURIComponent(name)}`).then((j) => j.metricDescriptors?.find((d) => d.name === name)?.description || "").catch(() => ""));
  return descCache.get(name);
}
const KIND = { gauge: t("kind.gauge"), cumulative: t("kind.cumulative"), delta: t("kind.delta"), distribution: t("kind.distribution") };
const labelCache = new Map();
async function labelsOf(metric) {
  if (!metric) return [];
  if (!labelCache.has(metric)) labelCache.set(metric, api(`/api/v1/labels?metric=${encodeURIComponent(metric)}`).then((j) => j.labels).catch(() => []));
  return labelCache.get(metric);
}
function categoryOf(name) { const t = name.split("_"); return t.length >= 3 ? `${t[0]} / ${t[1]}` : t[0]; }

// Metric picker: resources → categories → metrics
const PICKER_PAGE = 60;
function openPicker(current, onPick) {
  const scrim = $("#picker"); scrim.hidden = false;
  let res = "", cat = "", met = current || "", limit = PICKER_PAGE;
  const q = $("#picker-q"); q.value = ""; if (!COARSE) setTimeout(() => q.focus(), 0);
  function render() {
    const term = q.value.trim().toLowerCase();
    const match = (d) => !term || d.name.toLowerCase().includes(term) || (d.services || []).some((s) => s.toLowerCase().includes(term));
    const ds = descriptors.filter(match);
    const resCount = new Map();
    ds.forEach((d) => (d.services || []).forEach((s) => resCount.set(s, (resCount.get(s) || 0) + 1)));
    if (res && !resCount.has(res)) res = "";
    $("#picker-resources").innerHTML = `<li data-r="" tabindex="0" aria-selected="${!res}"><span>${t("all")}</span><span class="meta">${ds.length}</span></li>` +
      [...resCount].sort((a, b) => a[0].localeCompare(b[0])).map(([r, n]) => `<li data-r="${esc(r)}" tabindex="0" aria-selected="${r === res}"><span class="name">${esc(r)}</span><span class="meta">${n}</span></li>`).join("");
    const inRes = ds.filter((d) => !res || (d.services || []).includes(res));
    const cats = new Map(); inRes.forEach((d) => cats.set(categoryOf(d.name), (cats.get(categoryOf(d.name)) || 0) + 1));
    if (cat && !cats.has(cat)) cat = "";
    $("#picker-categories").innerHTML = `<li data-c="" tabindex="0" aria-selected="${!cat}"><span>${t("all_f")}</span><span class="meta">${inRes.length}</span></li>` +
      [...cats].sort().map(([c, n]) => `<li data-c="${esc(c)}" tabindex="0" aria-selected="${c === cat}"><span class="name">${esc(c)}</span><span class="meta">${n}</span></li>`).join("");
    const allMets = inRes.filter((d) => !cat || categoryOf(d.name) === cat);
    // Keep the selected metric visible even if it falls outside the rendered slice
    const mets = allMets.slice(0, limit);
    if (met && !mets.some((d) => d.name === met)) { const d = allMets.find((x) => x.name === met); if (d) mets.unshift(d); }
    $("#picker-metrics").innerHTML = mets.map((d) =>
      `<li data-m="${esc(d.name)}" tabindex="0" role="option" aria-selected="${d.name === met}"><span class="name">${esc(d.name)}</span><span class="meta"><span class="kind">${KIND[d.kind] || d.kind}</span> ${t("n.series", { n: fmtInt(d.series) })}</span></li>`).join("") +
      (allMets.length > limit ? `<li class="more"><button class="text" data-act="more">${t("picker.more", { n: Math.min(PICKER_PAGE, allMets.length - limit), r: allMets.length - limit })}</button></li>` : "") ||
      `<li><span class="meta">${t("picker.none")}</span></li>`;
    $("#picker-count").textContent = t("picker.count", { n: fmtInt(allMets.length) });
    const d = descriptorOf(met);
    $("#picker-desc").textContent = d ? d.name : "";
    if (d) descriptionOf(met).then((txt) => { if (met === d.name && txt) $("#picker-desc").textContent = txt; });
    $("#picker-apply").disabled = !d;
  }
  $("#picker-resources").onclick = (e) => { const li = e.target.closest("li"); if (li) { res = li.dataset.r; cat = ""; limit = PICKER_PAGE; render(); } };
  $("#picker-categories").onclick = (e) => { const li = e.target.closest("li"); if (li) { cat = li.dataset.c; limit = PICKER_PAGE; render(); } };
  $("#picker-metrics").onclick = (e) => {
    if (e.target.closest("[data-act=more]")) { limit += PICKER_PAGE; render(); return; }
    const li = e.target.closest("li[data-m]"); if (li) { met = li.dataset.m; render(); }
  };
  $("#picker-metrics").ondblclick = (e) => { const li = e.target.closest("li[data-m]"); if (li) { met = li.dataset.m; close(true); } };
  // Keyboard: Enter on a metric selects it; arrows move through the list
  $("#picker-metrics").onkeydown = (e) => {
    const li = e.target.closest("li[data-m]"); if (!li) return;
    if (e.key === "Enter" || e.key === " ") { e.preventDefault(); met = li.dataset.m; render(); $(`li[data-m="${CSS.escape(met)}"]`, $("#picker-metrics"))?.focus(); }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); (e.key === "ArrowDown" ? li.nextElementSibling : li.previousElementSibling)?.focus?.(); }
  };
  $$("#picker-resources, #picker-categories").forEach((ul) => ul.onkeydown = (e) => {
    const li = e.target.closest("li"); if (!li) return;
    if (e.key === "Enter" || e.key === " ") { e.preventDefault(); li.click(); }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); (e.key === "ArrowDown" ? li.nextElementSibling : li.previousElementSibling)?.focus?.(); }
  });
  q.oninput = () => { limit = PICKER_PAGE; render(); };
  const close = (apply) => { scrim.hidden = true; document.removeEventListener("keydown", onKey); if (apply && met) onPick(met); };
  const onKey = (e) => { if (e.key === "Escape") close(false); if (e.key === "Enter" && met && !e.target.closest("li")) close(true); };
  document.addEventListener("keydown", onKey);
  $("#picker-cancel").onclick = () => close(false);
  $("#picker-apply").onclick = () => close(true);
  scrim.onclick = (e) => { if (e.target === scrim) close(false); };
  render();
}

// ── Queries (shared by the explorer and dashboards) ────────────────────────
const OP_PREFIX = { "=": "", "!=": "!", "=~": "~", "!=~": "!~" };
const newQuery = (id) => ({ id, mode: "builder", metric: "", expr: "", legend: "", filters: [], reducer: "REDUCE_NONE", groupBy: [], aligner: "", period: "auto", hidden: false });
const isProm = (q) => q.mode === "promql";
const hasQuery = (q) => (isProm(q) ? !!q.expr?.trim() : !!q.metric);

function toApi(q, t) {
  // Keys from a link or pasted code are labels, never properties of the object (as __proto__ would be)
  const filter = Object.fromEntries(q.filters.map((f) => [f.key, OP_PREFIX[f.op] + f.value]));
  return {
    metric: q.metric, filter, ...(t ? { interval: { start: t.start, end: t.end } } : {}),
    aggregation: { alignmentPeriod: q.period, ...(q.aligner ? { perSeriesAligner: q.aligner } : {}), crossSeriesReducer: q.reducer, groupByFields: q.groupBy },
  };
}
function fromApi(a, id) {
  const q = newQuery(id);
  if (a.promql !== undefined) return Object.assign(q, { mode: "promql", expr: a.promql, legend: a.legend || "" });
  q.metric = a.metric || "";
  q.filters = Object.entries(a.filter || {}).map(([key, v]) => {
    const op = v.startsWith("!~") ? "!=~" : v.startsWith("~") ? "=~" : v.startsWith("!") ? "!=" : "=";
    return { key, op, value: v.slice(OP_PREFIX[op].length) };
  });
  const g = a.aggregation || {};
  q.reducer = g.crossSeriesReducer || "REDUCE_NONE"; q.groupBy = g.groupByFields || []; q.aligner = g.perSeriesAligner || ""; q.period = g.alignmentPeriod || "auto";
  return q;
}

/** Comparison offset in seconds (0 = no comparison) */
function compareOffset(c, t) {
  if (!c) return 0;
  if (c === "prev") return t.end - t.start;
  if (c.startsWith("custom:")) return durSecs(c.slice(7)) || 0;
  return durSecs(c) || 0;
}

/** A PromQL query shaped like a timeSeries:query response */
async function runPromQuery(q, tt, width) {
  const step = niceStep(tt.end - tt.start, Math.max(100, (width || 800) / 2));
  const r = await promRange(q.expr, tt.start, tt.end, step);
  const keys = varyingKeys(r.series);
  return {
    timeSeries: r.series.map((s) => ({ ...s, name: q.legend ? legendName(q.legend, s.labels) : undefined })), keys,
    sql: q.expr, tier: r.meta.tier, elapsedMs: r.meta.elapsedMs ?? 0, alignmentPeriodSeconds: step, promql: true,
  };
}
/** Runs queries and formulas (and their comparison) and returns the chart sets
 *  along with each query's results. */
async function runAll(queries, formulas, tr, compare, width) {
  const visible = queries.filter(hasQuery);
  const offset = compareOffset(compare, tr);
  const one = (q, tt) => (isProm(q) ? runPromQuery(q, tt, width) : api("/api/v1/timeSeries:query", toApi(q, tt)));
  const fetchAt = (tt) => Promise.all(visible.map((q) => one(q, tt).then((r) => [q.id, r])));
  const [now, before] = await Promise.all([fetchAt(tr), offset ? fetchAt({ start: tr.start - offset, end: tr.end - offset }) : Promise.resolve(null)]);
  const byNow = Object.fromEntries(now), byBefore = before ? Object.fromEntries(before) : null;
  const named = queries.length + formulas.length > 1;
  const sets = [];
  const add = (id, timeSeries, keys, hidden) => {
    if (hidden) return;
    const base = { name: named ? id : "", timeSeries, keys };
    sets.push(base);
    return base;
  };
  const errors = [];
  for (const q of visible) {
    const r = byNow[q.id];
    const keys = isProm(q) ? r.keys : q.reducer === "REDUCE_NONE" ? undefined : q.groupBy;
    const base = add(q.id, r.timeSeries, keys, q.hidden);
    if (base && byBefore) sets.push({ ...base, name: `${base.name ? base.name + " " : ""}${t("ago", { o: humanOffset(offset) })}`.trim(), timeSeries: byBefore[q.id].timeSeries, dashed: true, shift: offset, base });
  }
  formulas.forEach((f, i) => {
    if (!f.expr.trim()) return;
    const id = f.label?.trim() || `F${i + 1}`;
    try {
      const base = add(id, evalFormula(f.expr, byNow), undefined, f.hidden);
      if (base && byBefore) sets.push({ ...base, name: `${id} ${t("ago", { o: humanOffset(offset) })}`, timeSeries: evalFormula(f.expr, byBefore), dashed: true, shift: offset, base });
    } catch (e) { errors.push(`${id}: ${e.message}`); }
  });
  // Common unit: the first query's, if they all match
  const units = new Set(visible.filter((q) => !q.hidden).map((q) => (isProm(q) ? "" : unitOf(byNow[q.id]))));
  const unit = units.size === 1 && !formulas.some((f) => f.expr.trim() && !f.hidden) ? [...units][0] : "";
  return { sets, results: byNow, unit, errors };
}
function humanOffset(s) {
  if (s % 604800 === 0) return `${s / 604800} ${t("wk")}`;
  if (s % 86400 === 0) return `${s / 86400} d`;
  if (s % 3600 === 0) return `${s / 3600} h`;
  return `${Math.round(s / 60)} min`;
}

// ── Metrics explorer ───────────────────────────────────────────────────────
const defaultEx = () => ({ queries: [newQuery("A")], formulas: [], type: "line", log: false, compare: "" });
let ex = defaultEx();
/** Queries from a link or pasted code: their ids go into the markup and name them in formulas,
 *  so anything that is not a letter of its own becomes the next free one */
function letterIds(qs) {
  const used = new Set();
  const out = qs.filter((q) => q && typeof q === "object").slice(0, LETTERS.length);
  for (const q of out) {
    if (typeof q.id !== "string" || !/^[A-Z]$/.test(q.id) || used.has(q.id)) q.id = [...LETTERS].find((l) => !used.has(l));
    used.add(q.id);
  }
  return out;
}

function filterChip(f, i) {
  const ns = f.key.startsWith("resource.") ? t("ns.resource") : f.key.startsWith("metric.") ? t("ns.metric") : "";
  return `<span class="chip"><span class="ns">${ns}</span>${esc(shortField(f.key))} ${esc(f.op)} ${esc(f.value)}<button data-act="rm-filter" data-i="${i}" aria-label="${t("q.rm_filter")}">×</button></span>`;
}

const ICON_EYE = `<svg viewBox="0 0 24 24"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"/><circle cx="12" cy="12" r="3"/></svg>`;
const ICON_EYE_OFF = `<svg viewBox="0 0 24 24"><path d="M3 3l18 18M10.6 5.1A10 10 0 0 1 12 5c6.5 0 10 7 10 7a17 17 0 0 1-3.2 4.1M6.6 6.6C3.9 8.4 2 12 2 12s3.5 7 10 7c1.7 0 3.2-.4 4.5-1"/></svg>`;
const ICON_DEL = `<svg viewBox="0 0 24 24"><path d="M5 7h14M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/></svg>`;

function modeSwitch(q) {
  return `<div class="segmented mode" role="radiogroup" aria-label="${t("q.mode")}">
    <button role="radio" data-act="mode" data-mode="builder" aria-checked="${!isProm(q)}">${t("q.builder")}</button>
    <button role="radio" data-act="mode" data-mode="promql" aria-checked="${isProm(q)}">${t("q.promql")}</button></div>`;
}
function cardButtons(q) {
  return `<button class="icon-btn" data-act="toggle" data-tip="${t(q.hidden ? "q.show_chart" : "q.hide_chart")}" aria-label="${t(q.hidden ? "show" : "hide")}">${q.hidden ? ICON_EYE_OFF : ICON_EYE}</button>
      ${ex.queries.length > 1 ? `<button class="icon-btn" data-act="delete" data-tip="${t("q.delete")}" aria-label="${t("q.delete")}">${ICON_DEL}</button>` : ""}`;
}
// Card in PromQL mode: the expression and the legend template
function renderPromCard(q, el) {
  el.innerHTML = `
    <header><span class="badge">${esc(q.id)}</span>${modeSwitch(q)}<span class="grow"></span>${cardButtons(q)}</header>
    <textarea data-role="expr" rows="4" spellcheck="false" placeholder="sum by (device) (rate(node_network_receive_bytes_total[$__rate_interval]))" aria-label="${t("q.promql_aria")}">${esc(q.expr)}</textarea>
    <input data-role="legend" value="${esc(q.legend)}" placeholder="${esc(t("q.legend_ph"))}" aria-label="${t("q.legend_aria")}">
    <p class="hint">${t("q.promql_hint")}</p>`;
  const commit = () => { q.expr = $('[data-role="expr"]', el).value; q.legend = $('[data-role="legend"]', el).value; updateExplorer(true); };
  el.addEventListener("click", (e) => {
    const b = e.target.closest("[data-act]"); if (!b) return;
    if (b.dataset.act === "mode") { q.mode = b.dataset.mode; updateExplorer(true); }
    if (b.dataset.act === "toggle") { q.hidden = !q.hidden; updateExplorer(true); }
    if (b.dataset.act === "delete") { ex.queries = ex.queries.filter((x) => x !== q); updateExplorer(true); }
  });
  el.addEventListener("change", (e) => { if (e.target.dataset.role) commit(); });
  el.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); commit(); } });
  return el;
}

async function renderQueryCard(q) {
  const el = document.createElement("section");
  el.className = `qcard${q.hidden ? " off" : ""}${isProm(q) ? " prom" : ""}`; el.dataset.id = q.id;
  if (isProm(q)) return renderPromCard(q, el);
  const [labels, description] = await Promise.all([labelsOf(q.metric), descriptionOf(q.metric)]);
  const d = descriptorOf(q.metric);
  const fieldOpt = (l) => `<option value="${esc(l.field)}">${esc(shortField(l.field))}${l.field.startsWith("metric.") ? t("q.metric_tag") : ""}</option>`;
  el.innerHTML = `
    <header>
      <span class="badge">${esc(q.id)}</span>
      ${modeSwitch(q)}<span class="grow"></span>${cardButtons(q)}
    </header>
    <button class="select-like" data-act="pick"><span class="ellipsis ${q.metric ? "" : "placeholder"}">${esc(q.metric || t("q.select_metric"))}</span><svg viewBox="0 0 24 24"><path d="m7 10 5 5 5-5"/></svg></button>
    ${d ? `<p class="hint">${esc(KIND[d.kind] || d.kind)}${d.unit ? esc(t("q.unit", { u: d.unit })) : ""}. ${esc(description || "")}</p>` : ""}
    ${q.metric ? `
    <div class="field">
      <span class="field-label">${t("q.filters")}</span>
      <div class="chips">${q.filters.map(filterChip).join("")}</div>
      <div class="filter-editor" hidden>
        <select data-role="f-key" aria-label="${t("q.label")}">${labels.map(fieldOpt).join("")}</select>
        <select data-role="f-op" aria-label="${t("q.operator")}"><option>=</option><option>!=</option><option>=~</option><option>!=~</option></select>
        <input data-role="f-value" list="vals-${esc(q.id)}" placeholder="${t("q.value_ph")}" aria-label="${t("q.value")}"><datalist id="vals-${esc(q.id)}"></datalist>
        <button class="filled small" data-act="f-add">${t("add")}</button><button class="text small" data-act="f-cancel">${t("cancel")}</button>
      </div>
      <button class="text" data-act="f-new">${t("q.add_filter")}</button>
    </div>
    <div class="field">
      <span class="field-label">${t("q.aggregation")}</span>
      <div class="row">
        <select data-role="reducer" aria-label="${t("q.agg_fn")}">
          ${[["REDUCE_NONE", "red.none"], ["REDUCE_MEAN", "red.mean"], ["REDUCE_SUM", "red.sum"], ["REDUCE_MIN", "red.min"], ["REDUCE_MAX", "red.max"], ["REDUCE_COUNT", "red.count"], ["REDUCE_PERCENTILE_50", "red.p50"], ["REDUCE_PERCENTILE_95", "red.p95"], ["REDUCE_PERCENTILE_99", "red.p99"]]
            .map(([v, l]) => `<option value="${v}" ${q.reducer === v ? "selected" : ""}>${t(l)}</option>`).join("")}
        </select>
        <span class="by">${t("q.by")}</span>
        <select data-role="groupby" aria-label="${t("q.group_by")}"><option value="">${t("q.label_ellipsis")}</option>${labels.filter((l) => !q.groupBy.includes(l.field)).map(fieldOpt).join("")}</select>
      </div>
      <div class="chips">${q.groupBy.map((g, i) => `<span class="chip">${esc(shortField(g))}<button data-act="rm-group" data-i="${i}" aria-label="${t("q.rm_group")}">×</button></span>`).join("")}</div>
    </div>
    <details class="advanced" ${q.aligner || q.period !== "auto" ? "open" : ""}>
      <summary>${t("q.advanced")}</summary>
      <div class="field">
        <span class="field-label">${t("q.aligner")}</span>
        <select data-role="aligner">
          ${[["", "al.auto"], ["ALIGN_MEAN", "red.mean"], ["ALIGN_MAX", "red.max"], ["ALIGN_MIN", "red.min"], ["ALIGN_SUM", "red.sum"], ["ALIGN_COUNT", "red.count"], ["ALIGN_RATE", "al.rate"], ["ALIGN_DELTA", "al.delta"], ["ALIGN_PERCENTILE_50", "red.p50"], ["ALIGN_PERCENTILE_95", "red.p95"], ["ALIGN_PERCENTILE_99", "red.p99"]]
            .map(([v, l]) => `<option value="${v}" ${q.aligner === v ? "selected" : ""}>${t(l)}</option>`).join("")}
        </select>
      </div>
      <div class="field">
        <span class="field-label">${t("q.min_period")}</span>
        <select data-role="period">${["auto", "15s", "1m", "5m", "10m", "1h", "6h", "1d"].map((p) => `<option value="${p}" ${q.period === p ? "selected" : ""}>${p === "auto" ? t("automatic") : p}</option>`).join("")}</select>
      </div>
    </details>` : ""}`;

  const editor = $(".filter-editor", el);
  const fillValues = () => { const l = labels.find((x) => x.field === $('[data-role="f-key"]', el).value); $(`#vals-${q.id}`, el).innerHTML = (l?.values || []).map((v) => `<option value="${esc(v)}">`).join(""); };
  el.addEventListener("click", (e) => {
    const b = e.target.closest("[data-act]"); if (!b) return;
    const act = b.dataset.act;
    if (act === "mode") { q.mode = b.dataset.mode; if (isProm(q) && !q.expr && q.metric) q.expr = q.metric; updateExplorer(true); }
    if (act === "pick") loadDescriptors().then(() => openPicker(q.metric, (m) => { if (m !== q.metric) Object.assign(q, newQuery(q.id), { metric: m }); updateExplorer(true); }));
    if (act === "toggle") { q.hidden = !q.hidden; updateExplorer(true); }
    if (act === "delete") { ex.queries = ex.queries.filter((x) => x !== q); updateExplorer(true); }
    if (act === "rm-filter") { q.filters.splice(+b.dataset.i, 1); updateExplorer(true); }
    if (act === "rm-group") { q.groupBy.splice(+b.dataset.i, 1); updateExplorer(true); }
    if (act === "f-new") { editor.hidden = false; b.hidden = true; fillValues(); $('[data-role="f-value"]', el).focus(); }
    if (act === "f-cancel") { editor.hidden = true; $('[data-act="f-new"]', el).hidden = false; }
    if (act === "f-add") {
      const key = $('[data-role="f-key"]', el).value, value = $('[data-role="f-value"]', el).value.trim();
      if (key && value) { q.filters = q.filters.filter((f) => f.key !== key); q.filters.push({ key, op: $('[data-role="f-op"]', el).value, value }); updateExplorer(true); }
    }
  });
  el.addEventListener("change", (e) => {
    const role = e.target.dataset.role; if (!role) return;
    if (role === "f-key") return fillValues();
    if (role === "reducer") { q.reducer = e.target.value; if (q.reducer === "REDUCE_NONE") q.groupBy = []; }
    if (role === "groupby") { if (!e.target.value) return; q.groupBy.push(e.target.value); if (q.reducer === "REDUCE_NONE") q.reducer = "REDUCE_MEAN"; }
    if (role === "aligner") q.aligner = e.target.value;
    if (role === "period") q.period = e.target.value;
    if (role === "f-value" || role === "f-op") return;
    updateExplorer(true);
  });
  el.addEventListener("keydown", (e) => { if (e.target.dataset.role === "f-value" && e.key === "Enter") $('[data-act="f-add"]', el).click(); });
  return el;
}

function renderFormulaCard(f, i) {
  const el = document.createElement("section");
  el.className = `qcard formula${f.hidden ? " off" : ""}`;
  el.innerHTML = `
    <header>
      <span class="badge">${esc(f.label?.trim() || `F${i + 1}`)}</span>
      <input data-role="expr" value="${esc(f.expr)}" placeholder="(A / B) * 100" aria-label="${t("f.aria")}" spellcheck="false">
      <button class="icon-btn" data-act="toggle" data-tip="${t(f.hidden ? "q.show_chart" : "q.hide_chart")}" aria-label="${t(f.hidden ? "show" : "hide")}">${f.hidden ? ICON_EYE_OFF : ICON_EYE}</button>
      <button class="icon-btn" data-act="delete" data-tip="${t("f.delete")}" aria-label="${t("f.delete")}">${ICON_DEL}</button>
    </header>
    <input data-role="label" value="${esc(f.label || "")}" placeholder="${t("f.name_ph")}" aria-label="${t("f.name_aria")}">
    <p class="hint">${t("f.hint")}</p>`;
  el.addEventListener("click", (e) => {
    const b = e.target.closest("[data-act]"); if (!b) return;
    if (b.dataset.act === "toggle") f.hidden = !f.hidden;
    if (b.dataset.act === "delete") ex.formulas.splice(i, 1);
    updateExplorer(true);
  });
  el.addEventListener("change", (e) => {
    if (e.target.dataset.role === "expr") f.expr = e.target.value;
    if (e.target.dataset.role === "label") f.label = e.target.value;
    updateExplorer(true);
  });
  return el;
}

const exportCode = () => JSON.stringify({ queries: ex.queries.map((q) => (isProm(q) ? { id: q.id, promql: q.expr, ...(q.legend ? { legend: q.legend } : {}) } : { id: q.id, ...toApi(q) })), formulas: ex.formulas.map(({ expr, label }) => ({ expr, ...(label ? { label } : {}) })) }, null, 2);

let renderSeq = 0;
async function renderPanel() {
  const seq = ++renderSeq;
  const cards = await Promise.all(ex.queries.map(renderQueryCard));
  if (seq !== renderSeq) return;
  $("#query-list").replaceChildren(...cards);
  $("#formula-list").replaceChildren(...ex.formulas.map(renderFormulaCard));
  $("#code").value = exportCode();
  $("#log-scale").checked = ex.log;
  const custom = ex.compare.startsWith("custom:");
  $("#compare").value = custom ? "custom" : ex.compare;
  $("#compare-custom").hidden = !custom; if (custom) $("#compare-custom").value = ex.compare.slice(7);
  $$(".chart-toolbar .segmented button").forEach((b) => b.setAttribute("aria-checked", String(b.dataset.type === ex.type)));
  const named = ex.queries.filter(hasQuery);
  const what = (q) => (isProm(q) ? q.expr.trim() : q.metric);
  $("#chart-title").textContent = named.length === 1 && !ex.formulas.length ? what(named[0]) : named.length ? named.map((q) => `${q.id}: ${what(q)}`).join("  ") : t("nav.metrics");
}

let exSeq = 0;
async function updateExplorer(rebuild) {
  if (rebuild) await renderPanel();
  history.replaceState(null, "", `#/metrics?s=${encodeState(ex)}`);
  if (!ex.queries.some(hasQuery)) return;
  const seq = ++exSeq;
  const status = $("#explorer-status");
  status.className = "status"; status.textContent = t("querying");
  try {
    const tr = resolveTime();
    const { sets, results, unit, errors } = await runAll(ex.queries, ex.formulas, tr, ex.compare, $("#explorer-chart").clientWidth);
    if (seq !== exSeq) return;
    const rs = Object.entries(results);
    $("#sql").innerHTML = rs.map(([id, r]) => `<div class="sql-block"><h4>${esc(t("ex.query_n", { id }))}${r.promql ? " (PromQL)" : ""}</h4><pre>${esc(r.sql)}</pre></div>`).join("");
    const tiers = [...new Set(rs.map(([, r]) => tierName(r.tier)))].join(", ");
    const ms = rs.reduce((a, [, r]) => Math.max(a, r.elapsedMs), 0);
    status.className = errors.length ? "status error" : "status";
    status.innerHTML = errors.length ? esc(errors.join(" · ")) :
      `<span>${t("st.series")} <b>${sets.reduce((a, s) => a + s.timeSeries.length, 0)}</b></span><span>${t("st.resolution")} <b>${tiers}</b></span><span>${t("st.alignment")} <b>${rs[0]?.[1].alignmentPeriodSeconds ?? "—"} s</b></span><span>${t("st.query")} <b>${ms} ms</b></span>`;
    renderChart($("#explorer-chart"), $("#explorer-legend"), sets, { type: ex.type, log: ex.log, height: matchMedia("(max-width: 860px)").matches ? 260 : 360, unit, start: tr.start, end: tr.end, onZoom: (a, b) => setTime({ start: a, end: b }),
      // On mobile the legend is a list (the 5-column table does not fit); the "Table" button expands it
      ...(matchMedia("(max-width: 860px)").matches && !ex.legendTable ? { legend: { mode: "list", calcs: ["value"] } } : {}) });
    $("#legend-mode").hidden = !matchMedia("(max-width: 860px)").matches;
    $("#legend-mode").setAttribute("aria-pressed", String(!!ex.legendTable));
  } catch (e) {
    if (seq !== exSeq) return;
    status.className = "status error"; status.textContent = e.message;
    destroyChart($("#explorer-chart")); $("#explorer-chart").innerHTML = ""; $("#explorer-legend").innerHTML = "";
  }
}

/** Name of the resolution tier that served the query */
const tierName = (tier) => ({ raw: t("tier.raw"), "1m": t("tier.1m"), "10m": t("tier.10m") }[tier] || "—");
// The explorer's state for `#/metrics?s=`: base64, escaped for the URL (a '+' left as is reads back
// as a space). Links made before that still open: their spaces were '+'.
const encodeState = (s) => encodeURIComponent(btoa(unescape(encodeURIComponent(JSON.stringify(s)))));
const decodeState = (b) => { try { return JSON.parse(decodeURIComponent(escape(atob(b.replace(/ /g, "+"))))); } catch { return null; } };

function initExplorer() {
  $("#add-query").onclick = () => {
    const used = new Set(ex.queries.map((q) => q.id));
    ex.queries.push(newQuery([...LETTERS].find((l) => !used.has(l))));
    updateExplorer(true);
  };
  $("#add-formula").onclick = () => {
    const ids = ex.queries.filter(hasQuery).map((q) => q.id);
    ex.formulas.push({ expr: ids.length >= 2 ? `${ids[0]} / ${ids[1]}` : "", label: "", hidden: false });
    updateExplorer(true);
  };
  $("#log-scale").onchange = (e) => { ex.log = e.target.checked; updateExplorer(); };
  $("#legend-mode").onclick = () => { ex.legendTable = !ex.legendTable; updateExplorer(); };
  $("#compare").onchange = (e) => {
    const v = e.target.value;
    if (v === "custom") { $("#compare-custom").hidden = false; $("#compare-custom").focus(); ex.compare = `custom:${$("#compare-custom").value || "3h"}`; }
    else { $("#compare-custom").hidden = true; ex.compare = v; }
    updateExplorer();
  };
  $("#compare-custom").onchange = (e) => {
    if (!durSecs(e.target.value)) { const s = $("#explorer-status"); s.className = "status error"; s.textContent = t("ex.bad_offset"); return; }
    ex.compare = `custom:${e.target.value.trim()}`; updateExplorer();
  };
  $$(".chart-toolbar .segmented button").forEach((b) => b.onclick = () => { ex.type = b.dataset.type; $$(".chart-toolbar .segmented button").forEach((x) => x.setAttribute("aria-checked", String(x === b))); updateExplorer(); });
  $$(".tab").forEach((t) => t.onclick = () => {
    $$(".tab").forEach((x) => { x.classList.toggle("active", x === t); x.setAttribute("aria-selected", String(x === t)); x.tabIndex = x === t ? 0 : -1; });
    $$(".tab-body").forEach((x) => { x.hidden = x.dataset.body !== t.dataset.tab; });
  });
  $$(".tab").forEach((t) => { t.setAttribute("aria-selected", String(t.classList.contains("active"))); t.tabIndex = t.classList.contains("active") ? 0 : -1; });
  // Arrow keys in tabs and radio groups (as ARIA requires): move focus and activate
  document.addEventListener("keydown", (e) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    const el = e.target.closest('[role="tab"], [role="radio"]'); if (!el) return;
    const group = el.closest('[role="tablist"], [role="radiogroup"]'); if (!group) return;
    const items = $$('[role="tab"], [role="radio"]', group);
    const next = items[(items.indexOf(el) + (e.key === "ArrowRight" ? 1 : items.length - 1)) % items.length];
    e.preventDefault(); next.focus(); next.click();
  });
  $("#code-run").onclick = () => {
    try {
      const c = JSON.parse($("#code").value);
      const qs = Array.isArray(c.queries) ? c.queries : [c];
      ex.queries = letterIds(qs.filter((a) => a && typeof a === "object").map((a, i) => fromApi(a, a.id || LETTERS[i])));
      ex.formulas = (c.formulas || []).map((f) => ({ expr: f.expr || "", label: f.label || "", hidden: false }));
      updateExplorer(true);
    } catch (e) { const s = $("#explorer-status"); s.className = "status error"; s.textContent = t("ex.bad_query", { e: e.message }); }
  };
}

async function showExplorer(params) {
  await loadDescriptors();
  const s = params.get("s") && decodeState(params.get("s"));
  if (Array.isArray(s?.queries)) ex = Object.assign(defaultEx(), s, { queries: letterIds(s.queries) });
  await updateExplorer(true);
}

// ── Navigation ─────────────────────────────────────────────────────────────
const TITLES = Object.fromEntries(["home", "alerts", "timeline", "notebooks", "metrics", "dashboards", "logs"].map((v) => [v, t(`nav.${v}`)]));
let current = "";
function route() {
  const [path, qs] = location.hash.replace(/^#\/?/, "").split("?");
  const parts = path.split("/").filter(Boolean);
  const view = TITLES[parts[0]] ? parts[0] : "home";
  current = view;
  $$(".view").forEach((v) => v.classList.toggle("active", v.id === `view-${view}`));
  $$(".sidenav a").forEach((a) => a.classList.toggle("active", a.dataset.view === view));
  $("#view-title").textContent = TITLES[view];
  document.body.classList.remove("nav-open");
  const fail = (el) => (e) => { el.className = "status error"; el.textContent = e.message; };
  if (view !== "dashboards") setAutoEvery(PAGE_REFRESH);
  if (view !== "home" && view !== "timeline") closeDrawer();
  if (view === "home") showHome().catch(fail($("#home-status")));
  if (view === "alerts") showAlerts(new URLSearchParams(qs || "")).catch(fail($("#al-status")));
  if (view === "timeline") showTimeline(new URLSearchParams(qs || "")).catch(fail($("#tl-status")));
  if (view === "notebooks") showNotebooks(parts, new URLSearchParams(qs || "")).catch((e) => console.error(e));
  if (view === "metrics") showExplorer(new URLSearchParams(qs || "")).catch(fail($("#explorer-status")));
  if (view === "dashboards") showDashboards(parts[1], new URLSearchParams(qs || "")).catch(fail($("#dash-vars")));
  if (view === "logs") showLogs(new URLSearchParams(qs || "")).catch(fail($("#log-status")));
}
function refresh() {
  if (current === "home") homeRefresh();
  else if (current === "alerts") alertsRefresh();
  else if (current === "timeline") timelineRefresh();
  else if (current === "notebooks") notebookRefresh();
  else if (current === "metrics") updateExplorer();
  else if (current === "dashboards") dashRefresh();
  else if (current === "logs") logsRefresh();
}

$("#nav-toggle").onclick = (e) => {
  const narrow = matchMedia("(max-width: 860px)").matches;
  document.body.classList.toggle(narrow ? "nav-open" : "nav-collapsed");
  e.currentTarget.setAttribute("aria-expanded", String(narrow ? document.body.classList.contains("nav-open") : !document.body.classList.contains("nav-collapsed")));
};
// On mobile the menu drawer closes when tapping outside it
document.addEventListener("click", (e) => {
  if (document.body.classList.contains("nav-open") && !e.target.closest("#sidenav, #nav-toggle")) { document.body.classList.remove("nav-open"); $("#nav-toggle").setAttribute("aria-expanded", "false"); }
});
// ── Language and theme (top bar; on mobile, at the foot of the side menu) ──
const THEME_ORDER = ["system", "light", "dark"];
function paintTheme() {
  const v = themeGet(), btn = $("#theme-btn");
  btn.dataset.t = v;
  const label = t("theme.tip", { t: t(`theme.${v}`) });
  btn.setAttribute("aria-label", `${t("theme.label")}: ${t(`theme.${v}`)}`); btn.dataset.tip = label;
}
function placePrefs() {
  const prefs = $("#prefs"), narrow = matchMedia("(max-width: 860px)").matches;
  const target = narrow ? $("#sidenav-foot") : $(".time-controls");
  if (prefs.parentElement !== target) target.appendChild(prefs);
}
function initPrefs() {
  $$("#lang-pick [data-lang]").forEach((b) => { b.setAttribute("aria-checked", String(b.dataset.lang === LANG)); b.onclick = () => setLang(b.dataset.lang); });
  $("#theme-btn").onclick = () => setTheme(THEME_ORDER[(THEME_ORDER.indexOf(themeGet()) + 1) % THEME_ORDER.length]);
  // A theme change swaps the tokens: repaint the charts (they read them at paint time)
  document.addEventListener("atalaya:theme", () => { paintTheme(); refresh(); });
  paintTheme();
  placePrefs();
  matchMedia("(max-width: 860px)").addEventListener("change", placePrefs);
}

// Last: dash.js and the other scripts are already loaded
document.addEventListener("DOMContentLoaded", () => {
  initPrefs(); initTimePicker(); initExplorer(); initLogs();
  window.addEventListener("hashchange", () => { if (!location.hash.startsWith("#/metrics?s=") || current !== "metrics") route(); });
  route();
});
