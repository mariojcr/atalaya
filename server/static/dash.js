// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Dashboards: collapsible sections, variables and widgets (PromQL, log and builder ones).
// The YAML schema is documented in the README (dashboard format).
"use strict";

const ROW_PX = 30, GAP_PX = 8; // Grafana's grid: 24 columns, 30 px rows
const NAMED = { green: "#73BF69", red: "#F2495C", orange: "#FF9830", yellow: "#FADE2A", blue: "#5794F2", purple: "#B877D9",
  "dark-green": "#37872D", "dark-red": "#C4162A", "dark-orange": "#FA6400", "dark-yellow": "#E0B400", "dark-blue": "#1F60C4", "dark-purple": "#8F3BB8" };
const colorName = (c) => (c === "text" ? css("--ink") : NAMED[c] || c);
const ICON_OPEN = `<svg viewBox="0 0 24 24"><path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/></svg>`;
const ICON_INFO = `<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 8v.01"/></svg>`;
const ICON_CHEVRON = `<svg viewBox="0 0 24 24"><path d="m9 6 6 6-6 6"/></svg>`;

let dashList = null;
let board = null; // { name, def, vars, varDefs, widgets, observer }

// ── Help popover: replaces title=, which cannot be read on touch screens ─────
// data-tip="text" on any element: shown on hover or focus (mouse and keyboard)
// or on tap (touch); aria-describedby points at the popover.
const tipEl = (() => { const d = document.createElement("div"); d.id = "tip"; d.className = "tip"; d.hidden = true; d.setAttribute("role", "tooltip"); document.body.appendChild(d); return d; })();
let tipFor = null;
function showTip(el) {
  const text = el.dataset.tip; if (!text) return;
  tipEl.textContent = text; tipEl.hidden = false; tipFor = el;
  el.setAttribute("aria-describedby", "tip");
  const r = el.getBoundingClientRect(), w = tipEl.offsetWidth, h = tipEl.offsetHeight;
  let x = r.left + r.width / 2 - w / 2, y = r.bottom + 6;
  if (y + h > innerHeight - 8) y = r.top - h - 6;
  x = Math.max(8, Math.min(x, innerWidth - w - 8));
  tipEl.style.left = x + "px"; tipEl.style.top = Math.max(8, y) + "px";
}
function hideTip() { if (tipFor) tipFor.removeAttribute("aria-describedby"); tipFor = null; tipEl.hidden = true; }
document.addEventListener("mouseover", (e) => { const el = e.target.closest?.("[data-tip]"); if (el && !COARSE) showTip(el); else if (!COARSE && tipFor && !e.target.closest("#tip")) hideTip(); });
document.addEventListener("focusin", (e) => { const el = e.target.closest?.("[data-tip]"); if (el) showTip(el); });
document.addEventListener("focusout", () => hideTip());
document.addEventListener("click", (e) => {
  const el = e.target.closest?.("[data-tip]");
  if (!el) { if (tipFor) hideTip(); return; }
  // Only purely informative elements (no action of their own) consume the tap
  if (el.dataset.tipOnly !== undefined) { e.preventDefault(); e.stopPropagation(); }
  if (tipFor === el) hideTip(); else showTip(el);
}, true);
window.addEventListener("scroll", () => { if (tipFor) hideTip(); }, { passive: true, capture: true });

// ── Values: reduction, mappings, thresholds and color ──────────────────────
function reduce(points, how = "lastNotNull") {
  const vs = points.map((p) => p[1]);
  if (how === "last") return vs.at(-1) ?? null;
  if (how === "first") return vs[0] ?? null;
  if (how === "count") return vs.filter((v) => v != null && !Number.isNaN(v)).length;
  return calc(how === "lastNotNull" || !CALCS[how] ? "lastNotNull" : how, vs);
}
function mapValue(mappings, v) {
  for (const m of mappings || []) {
    if (m.match !== undefined) {
      const isNull = v == null || (typeof v === "number" && Number.isNaN(v));
      if ((m.match === "null" && isNull) || (m.match === "nan" && Number.isNaN(v)) || (m.match === "true" && v === true) ||
        (m.match === "false" && v === false) || (m.match === "empty" && v === "")) return m;
    } else if (m.value !== undefined) {
      if (v != null && (String(v) === String(m.value) || (typeof v === "number" && Number(m.value) === v))) return m;
    } else if (m.regex !== undefined) {
      if (v != null && new RegExp(m.regex).test(String(v))) return m;
    } else if (typeof v === "number" && !Number.isNaN(v) && (m.from == null || v >= m.from) && (m.to == null || v <= m.to)) return m;
  }
  return null;
}
function thresholdColor(th, v, min, max) {
  const steps = th?.steps; if (!steps?.length || typeof v !== "number" || Number.isNaN(v)) return null;
  let x = v;
  if (th.mode === "percentage") { const lo = min ?? 0, hi = max ?? 100; x = hi === lo ? 0 : ((v - lo) / (hi - lo)) * 100; }
  let c = steps[0][1];
  for (const [at, col] of steps) if (at == null || x >= at) c = col;
  return colorName(c);
}
/** Color of a value: mapping > thresholds > fixed > palette (or null: normal ink) */
function valueColor(w, v, st = {}, i = 0) {
  const m = mapValue(st.mappings || w.mappings, v);
  if (m?.color) return colorName(m.color);
  if (st.color) return colorName(st.color);
  const mode = w.color || (w.thresholds ? "thresholds" : null);
  if (mode === "thresholds") return thresholdColor(st.thresholds || w.thresholds, v, st.min ?? w.min, st.max ?? w.max);
  if (mode?.startsWith("fixed:")) return colorName(mode.slice(6));
  if (mode === "palette") return palette()[i % palette().length];
  return null;
}
function valueText(w, v, st = {}) {
  const m = mapValue(st.mappings || w.mappings, v);
  if (m?.text != null) return m.text;
  if (v == null || (typeof v === "number" && Number.isNaN(v))) return w.noValue ?? t("nodata");
  return fmtUnit(v, st.unit ?? w.unit, st.decimals ?? w.decimals);
}

// ── Widget series: names, renaming and overrides ───────────────────────────
function matchesOverride(o, name, ref) {
  const m = o.match;
  if (Array.isArray(m)) return m.includes(name);
  if (m && typeof m === "object") return m.ref === ref;
  const rx = /^\/(.*)\/([a-z]*)$/.exec(m);
  if (rx) { try { return new RegExp(rx[1], rx[2]).test(name); } catch { return false; } }
  return m === name;
}
function buildSeries(w, results) {
  const out = [];
  results.forEach(({ q, series }) => series.forEach((s) => out.push({ ...s, q })));
  const keys = varyingKeys(out);
  for (const s of out) {
    let name = s.q.legend ? legendName(s.q.legend, s.labels).trim()
      : keys.length ? seriesName(s.labels, keys.filter((k) => k !== "__name__").length ? keys.filter((k) => k !== "__name__") : keys)
      : s.labels.__name__ || (Object.keys(s.labels).length ? selectorName(s.labels) : s.q.ref || "");
    for (const [rx, rep] of w.rename || []) { try { name = name.replace(new RegExp(rx), rep); } catch { /* Grafana regex that is not valid in JS */ } }
    s.name = name || s.q.ref || t("q.value");
    // An earlier period (offset) is dashed, like the explorer's comparison
    const st = s.q.offset ? { dash: true } : {};
    for (const o of w.series || []) if (matchesOverride(o, s.name, s.q.ref)) Object.assign(st, o);
    if (st.name) s.name = st.name;
    delete st.match; delete st.name;
    s.style = st;
  }
  return out;
}

// ── Queries ─────────────────────────────────────────────────────────────────
function varContext() {
  const ctx = {};
  for (const v of board?.varDefs || []) {
    const s = board.vars[v.name];
    if (s) ctx[v.name] = { values: s.values, all: s.all, multi: v.multi, includeAll: v.includeAll, allValue: v.allValue };
  }
  return ctx;
}
/** $__range, $__range_s and $__range_ms of an instant query, from the dashboard's
 *  range: the server only gets the instant and would read them as 0 (Grafana
 *  also resolves them in the browser) */
function expandRange(expr, secs) {
  const v = (unit) => (unit === "_s" ? String(secs) : unit === "_ms" ? String(secs * 1000) : `${secs}s`);
  return expr.replace(/\$\{__range(_s|_ms)?\}|\$__range(_s|_ms)?\b/g, (m, a, b) => v(a || b));
}
/** Filters of a log query (the explorer's syntax, with the variables
 *  substituted) and the range, as /api/v1/logs parameters */
function logParams(src, ctx, t) {
  const { filters, words } = lgParse(interpolate(src || "", ctx));
  const p = new URLSearchParams({ start: t.start, end: t.end });
  [...filters, ...words.map((w) => `body~${w}`)].forEach((f) => p.append("f", f));
  return p;
}
/** Line counts of a log query as series (one per group of its `by` fields):
 *  one count over the range (instant) or one per step */
async function logCounts(q, ctx, t, step, signal) {
  const p = logParams(q.logs, ctx, t);
  [].concat(q.by || []).forEach((b) => p.append("by", b));
  if (q.top) p.set("top", q.top);
  if (step) p.set("step", step);
  const r = await fetch(`/api/v1/logs/counts?${p}`, { signal });
  const j = await r.json().catch(() => ({ error: r.statusText }));
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j.series;
}
async function runQueries(w, t, width, signal) {
  const ctx = varContext();
  const qs = (w.queries || []).filter((q) => !q.hide && (q.expr || q.logs != null));
  const points = w.type === "timeseries" || w.type === "timeline" ? width / 2 : Math.min(200, width / 3);
  return Promise.all(qs.map(async (q, i) => {
    const qq = { ...q, ref: q.ref || LETTERS[i] };
    const minStep = q.minStep ? durSecs(q.minStep) || 15 : 15;
    // A heatmap folds hourly buckets into its cells
    const step = w.type === "heatmap" ? 3600 : niceStep(t.end - t.start, points, minStep);
    // offset: the query reads an earlier window and its points are drawn at the current times
    const off = q.offset ? durSecs(q.offset) || 0 : 0;
    const qt = off ? { start: t.start - off, end: t.end - off } : t;
    let r;
    if (q.logs != null) r = { series: await logCounts(q, ctx, qt, q.instant ? 0 : step, signal), meta: {} };
    else {
      const expr = q.instant ? expandRange(interpolate(q.expr, ctx), t.end - t.start) : interpolate(q.expr, ctx);
      // Range queries are batched with those of other widgets querying at the same time
      r = q.instant ? await promInstant(expr, qt.end, signal) : await promRangeBatched(expr, qt.start, qt.end, step, signal);
    }
    const series = off ? r.series.map((s) => ({ ...s, points: s.points.map(([x, v]) => [x + off, v]) })) : r.series;
    return { q: qq, series, meta: r.meta };
  }));
}

/** Logs explorer link for one group of a log query with `by`: the query plus
 *  that group's field values (null for other series) */
function drillLink(w, s) {
  const by = [].concat(s.q?.by || []);
  if (s.q?.logs == null || !by.length) return null;
  const terms = by.filter((f) => (s.labels[f] ?? "") !== "").map((f) => `${f}:${lgAcQuote(s.labels[f])}`);
  // A `field:*` of a grouped field is redundant once its value is fixed
  const re = (f) => new RegExp(`(^|\\s)${f.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}:\\*(?=\\s|$)`, "g");
  const base = by.reduce((src, f) => src.replace(re(f), "$1"), interpolate(s.q.logs, varContext()));
  const p = new URLSearchParams({ q: [base.replace(/\s+/g, " ").trim(), ...terms].filter(Boolean).join(" ") });
  if (w.logs?.columns?.length) p.set("cols", w.logs.columns.join(","));
  return `#/logs?${p}`;
}

// ── Renderers ───────────────────────────────────────────────────────────────
function renderTimeseries(w, body, series, t) {
  const right = w.legend?.placement === "right";
  body.innerHTML = `<div class="ts${right ? " right" : ""}"><div class="chart-host"></div><div class="legend-host"></div></div>`;
  const st = w.style || {};
  const thresholds = (w.thresholds?.steps || []).map(([value, color]) => ({ value, color: colorName(color) }));
  renderChart($(".chart-host", body), $(".legend-host", body), [{ name: "", timeSeries: series }], {
    grafana: true, height: "fit", unit: w.unit, decimals: w.decimals, min: w.min, max: w.max, start: t.start, end: t.end,
    stack: st.stack, fill: st.fill ?? 0, lineWidth: st.lineWidth, draw: st.draw, interpolation: st.interpolation, showPoints: st.showPoints,
    spanNulls: st.spanNulls, softMin: st.softMin, softMax: st.softMax, log: !!st.logBase, logBase: st.logBase, axis: st.axis,
    thresholds, thresholdsStyle: w.thresholdsStyle, legend: { mode: "list", ...(w.legend || {}) },
    onZoom: (a, b) => setTime({ start: a, end: b }),
  });
}

function sparkline(points, color) {
  const vs = points.filter((p) => p[1] != null && Number.isFinite(p[1]));
  if (vs.length < 2) return "";
  const x0 = vs[0][0], x1 = vs.at(-1)[0], ys = vs.map((p) => p[1]);
  const lo = Math.min(...ys), hi = Math.max(...ys);
  const X = (x) => ((x - x0) / (x1 - x0 || 1)) * 100, Y = (y) => 30 - ((y - lo) / (hi - lo || 1)) * 26 - 2;
  const line = vs.map((p, i) => `${i ? "L" : "M"}${X(p[0]).toFixed(2)},${Y(p[1]).toFixed(2)}`).join("");
  return `<svg class="spark" viewBox="0 0 100 30" preserveAspectRatio="none" aria-hidden="true"><path class="a" d="${line}L100,30L0,30Z"/><path class="l" d="${line}"/></svg>`;
}
/** Fits the font size of big values to their available space */
function fitText(el, h, wpx, maxPx = 56, minPx = 12) {
  const len = Math.max(1, el.textContent.length);
  el.style.fontSize = Math.max(minPx, Math.min(maxPx, h * 0.7, (wpx * 0.92) / (len * 0.58))) + "px";
}

function renderStat(w, body, series) {
  const o = w.stat || {};
  let items;
  if (o.field) {
    // The value is a label (e.g. connection_status): the one of the series with the latest sample
    let best = null;
    for (const s of series) { const last = s.points.filter((p) => p[1] != null && !Number.isNaN(p[1])).at(-1); if (last && (!best || last[0] >= best.t)) best = { t: last[0], s }; }
    const v = best ? best.s.labels[o.field] ?? null : null;
    items = [{ name: w.title, v, s: best?.s || { points: [], style: {} } }];
  } else {
    items = series.map((s) => ({ name: s.name, v: reduce(s.points, o.reduce), s }));
  }
  if (!items.length) items = [{ name: "", v: null, s: { points: [], style: {} } }];
  const mode = o.colorMode || "value";
  const textMode = o.textMode || (items.length > 1 ? "value_and_name" : "value");
  body.innerHTML = `<div class="stats${o.orientation === "horizontal" ? " horizontal" : ""}">` + items.map((it, i) => {
    const showName = textMode === "name" || textMode === "value_and_name";
    const main = textMode === "name" ? it.name : valueText(w, it.v, it.s.style);
    return `<div class="stat ${mode === "background" ? "bg" : ""}" data-i="${i}">${showName && textMode !== "name" ? `<div class="stat-name" data-tip="${esc(it.name)}">${esc(it.name)}</div>` : ""}<div class="stat-box"><div class="stat-value">${esc(main)}</div></div>${o.graph === "area" && !o.field ? sparkline(it.s.points, "") : ""}</div>`;
  }).join("") + `</div>`;
  $$(".stat", body).forEach((el) => {
    const it = items[+el.dataset.i];
    const c = valueColor(w, it.v, it.s.style, +el.dataset.i);
    if (c && mode === "background") { const ink = inkOn(c); el.style.background = c; el.style.color = ink; el.style.setProperty("--spark", ink === "#fff" ? "rgba(255,255,255,.85)" : "rgba(32,33,36,.7)"); }
    else if (c && mode !== "none") { $(".stat-value", el).style.color = c; el.style.setProperty("--spark", c); }
    else el.style.setProperty("--spark", c || css("--accent"));
  });
  scheduleFit(body);
}
// Font fitting for every stat of the dashboard in a single frame: measure everything
// first, then write, instead of alternating reads and writes per widget (layout thrashing)
const fitQueue = new Set();
let fitRaf = 0;
function scheduleFit(body) {
  fitQueue.add(body);
  if (fitRaf) return;
  fitRaf = requestAnimationFrame(() => {
    fitRaf = 0;
    const jobs = [];
    for (const b of fitQueue) if (b.isConnected) $$(".stat-value", b).forEach((el) => { const box = el.parentElement; jobs.push([el, box.clientHeight, box.clientWidth]); });
    fitQueue.clear();
    jobs.forEach(([el, h, wpx]) => fitText(el, h, wpx));
  });
}

function arc(cx, cy, r, a0, a1) {
  const p = (a) => [cx + r * Math.cos(a), cy + r * Math.sin(a)];
  const [x0, y0] = p(a0), [x1, y1] = p(a1);
  return `M${x0.toFixed(2)},${y0.toFixed(2)}A${r},${r} 0 ${a1 - a0 > Math.PI ? 1 : 0} 1 ${x1.toFixed(2)},${y1.toFixed(2)}`;
}
function renderGauge(w, body, series) {
  const o = w.gauge || {};
  const items = series.length ? series.map((s) => ({ name: s.name, v: reduce(s.points, o.reduce), s })) : [{ name: "", v: null, s: { style: {} } }];
  const A0 = Math.PI * 0.75, A1 = Math.PI * 2.25; // 270° arc
  body.innerHTML = `<div class="gauges">` + items.map((it, i) => {
    const st = it.s.style || {};
    const min = st.min ?? w.min ?? 0, max = st.max ?? w.max ?? 100;
    const frac = it.v == null || Number.isNaN(it.v) ? 0 : Math.max(0, Math.min(1, (it.v - min) / (max - min || 1)));
    const steps = (st.thresholds || w.thresholds)?.steps || [];
    const pct = (st.thresholds || w.thresholds)?.mode === "percentage";
    const marks = o.markers === false ? "" : steps.map(([at, col], k) => {
      const from = at == null ? 0 : pct ? at / 100 : (at - min) / (max - min || 1);
      const nx = steps[k + 1]?.[0];
      const to = nx == null ? 1 : pct ? nx / 100 : (nx - min) / (max - min || 1);
      const a = Math.max(0, Math.min(1, from)), b = Math.max(0, Math.min(1, to));
      return b > a ? `<path class="mark" d="${arc(100, 100, 92, A0 + (A1 - A0) * a, A0 + (A1 - A0) * b)}" stroke="${esc(colorName(col))}"/>` : "";
    }).join("");
    return `<figure class="gauge"><svg viewBox="0 0 200 172" role="img" aria-label="${esc(it.name)} ${esc(valueText(w, it.v, st))}">${marks}
      <path class="track" d="${arc(100, 100, 78, A0, A1)}"/>${frac > 0 ? `<path class="val" data-c="${i}" d="${arc(100, 100, 78, A0, A0 + (A1 - A0) * Math.max(frac, 0.002))}"/>` : ""}
      <text x="100" y="110" class="gv" data-c="${i}">${esc(valueText(w, it.v, st))}</text></svg>${items.length > 1 ? `<figcaption data-tip="${esc(it.name)}">${esc(it.name)}</figcaption>` : ""}</figure>`;
  }).join("") + `</div>`;
  $$("[data-c]", body).forEach((el) => {
    const i = +el.dataset.c, c = valueColor(w, items[i].v, items[i].s.style, i) || css("--accent");
    if (el.tagName === "path") el.style.stroke = c; else el.style.fill = c;
  });
}

function renderBargauge(w, body, series) {
  const o = w.bargauge || {};
  const items = series.map((s) => ({ name: s.name, v: reduce(s.points, o.reduce), s }));
  if (!items.length) { body.innerHTML = `<div class="empty"><p>${t("nodata")}</p></div>`; return; }
  body.innerHTML = `<div class="bargauges">` + items.map((it, i) => {
    const href = drillLink(w, it.s), tag = href ? "a" : "div";
    return `<${tag} class="bgrow" data-i="${i}"${href ? ` href="${esc(href)}"` : ""}><span class="bgname" data-tip="${esc(it.name)}">${esc(it.name)}</span><span class="bgtrack"><i></i></span><span class="bgval">${esc(valueText(w, it.v, it.s.style))}</span></${tag}>`;
  }).join("") + `</div>`;
  $$(".bgrow", body).forEach((row) => {
    const i = +row.dataset.i, it = items[i], st = it.s.style || {};
    const min = st.min ?? w.min ?? 0, max = st.max ?? w.max ?? Math.max(...items.map((x) => x.v ?? 0), 1);
    const frac = it.v == null || Number.isNaN(it.v) ? 0 : Math.max(0, Math.min(1, (it.v - min) / (max - min || 1)));
    const c = valueColor(w, it.v, st, i) || palette()[i % palette().length];
    const bar = $(".bgtrack i", row);
    bar.style.width = (frac * 100).toFixed(1) + "%";
    bar.style.background = o.mode === "gradient" ? `linear-gradient(90deg, ${withAlpha(c, 0.35)}, ${c})` : c;
    $(".bgval", row).style.color = c;
  });
}

function renderPie(w, body, series) {
  const o = w.pie || {};
  const items = series.map((s, i) => ({ name: s.name, v: reduce(s.points, o.reduce) ?? 0, c: s.style?.color ? colorName(s.style.color) : palette()[i % palette().length], s })).filter((it) => it.v > 0);
  const total = items.reduce((a, it) => a + it.v, 0);
  if (!total) { body.innerHTML = `<div class="empty"><p>${t("nodata")}</p></div>`; return; }
  let a = -Math.PI / 2;
  const R = 46, r = o.donut ? 28 : 0;
  const segs = items.map((it, i) => {
    const da = (it.v / total) * Math.PI * 2, a1 = a + da;
    const p = (ang, rad) => `${(50 + rad * Math.cos(ang)).toFixed(3)},${(50 + rad * Math.sin(ang)).toFixed(3)}`;
    const large = da > Math.PI ? 1 : 0;
    const d = items.length === 1 ? (r ? `M50,4A46,46 0 1 1 49.99,4ZM50,22A28,28 0 1 0 50.01,22Z` : `M50,4A46,46 0 1 1 49.99,4Z`)
      : r ? `M${p(a, R)}A${R},${R} 0 ${large} 1 ${p(a1, R)}L${p(a1, r)}A${r},${r} 0 ${large} 0 ${p(a, r)}Z` : `M50,50L${p(a, R)}A${R},${R} 0 ${large} 1 ${p(a1, R)}Z`;
    a = a1;
    return `<path data-i="${i}" d="${d}" fill-rule="evenodd"><title>${esc(it.name)}: ${esc(valueText(w, it.v, it.s.style))} (${((it.v / total) * 100).toFixed(1)}%)</title></path>`;
  }).join("");
  const legend = w.legend?.show === false ? "" : `<ul class="pie-legend">` + items.map((it, i) => {
    const href = drillLink(w, it.s), tag = href ? "a" : "span";
    return `<li><${tag} class="pl"${href ? ` href="${esc(href)}"` : ""}><i class="swatch" data-i="${i}"></i><span>${esc(it.name)}</span><b>${esc(valueText(w, it.v, it.s.style))}</b></${tag}></li>`;
  }).join("") + `</ul>`;
  body.innerHTML = `<div class="pie"><svg viewBox="0 0 100 100" role="img" aria-label="${esc(w.title)}">${segs}</svg>${legend}</div>`;
  $$("path[data-i]", body).forEach((p) => {
    const it = items[+p.dataset.i], href = drillLink(w, it.s);
    p.style.fill = it.c;
    if (href) { p.classList.add("link"); p.onclick = () => { location.hash = href.slice(1); }; }
  });
  $$(".swatch[data-i]", body).forEach((e) => { const c = items[+e.dataset.i].c; e.style.background = c; e.style.borderColor = c; });
}

function renderBar(w, body, series) {
  const items = series.map((s) => ({ name: s.name, v: reduce(s.points, "lastNotNull") ?? 0, s }));
  if (!items.length) { body.innerHTML = `<div class="empty"><p>${t("nodata")}</p></div>`; return; }
  const max = Math.max(w.max ?? 0, ...items.map((it) => it.v), 1e-9);
  body.innerHTML = `<div class="bars">` + items.map((it, i) => {
    const href = drillLink(w, it.s), tag = href ? "a" : "div";
    return `<${tag} class="bar" data-i="${i}"${href ? ` href="${esc(href)}"` : ""}><b>${esc(valueText(w, it.v, it.s.style))}</b><span class="col"><i></i></span><span class="lbl" data-tip="${esc(it.name)}">${esc(it.name)}</span></${tag}>`;
  }).join("") + `</div>`;
  $$(".bar", body).forEach((el) => {
    const it = items[+el.dataset.i];
    const bar = $(".col i", el);
    bar.style.height = ((it.v / max) * 100).toFixed(1) + "%";
    bar.style.background = it.s.style?.color ? colorName(it.s.style.color) : valueColor(w, it.v, it.s.style, 0) || css("--accent");
  });
}

/** Table: one row per series or, with join (or several queries), one row per key with a column per query. */
function renderTable(w, body, results) {
  const tc = w.table || {};
  const multi = results.length > 1;
  const rows = new Map();
  const labelOrder = [];
  for (const { q, series } of results) {
    for (const s of series) {
      const { __name__, ...rest } = s.labels;
      const key = tc.join ? s.labels[tc.join] ?? "" : multi ? labelKey(rest) : `${labelKey(s.labels)}#${rows.size}`;
      const row = rows.get(key) || { labels: {}, values: {}, href: drillLink(w, s) };
      Object.entries(s.labels).forEach(([k, v]) => { if (!(k in row.labels)) row.labels[k] = v; if (!labelOrder.includes(k)) labelOrder.push(k); });
      const last = s.points.filter((p) => p[1] != null).at(-1);
      row.values[q.ref] = last ? last[1] : null;
      if (!multi) row.values.Value = row.values[q.ref];
      rows.set(key, row);
    }
  }
  const refs = results.map((r) => r.q.ref);
  const hide = new Set(tc.hide || ["__name__"]);
  let cols = tc.columns?.length ? tc.columns.map((c) => ({ ...c }))
    : [...labelOrder.filter((k) => !hide.has(k)).map((k) => ({ field: k })), ...refs.filter((r) => !hide.has(r) && !(hide.has("Value") && !multi)).map((r) => ({ field: r, name: multi ? r : t("q.value") }))];
  for (const rn of tc.rename || []) cols.filter((c) => c.field === rn.field).forEach((c) => { c.name ||= rn.name; });
  cols = cols.map((c) => ({ ...c, name: c.name || c.field, isValue: refs.includes(c.field) || c.field === "Value" }));
  const cellValue = (row, c) => {
    if (!c.isValue) return row.labels[c.field] ?? "";
    const v = row.values[c.field];
    return v == null ? null : v * (c.scale ?? 1);
  };
  const data = [...rows.values()];
  let sort = tc.sort ? { name: tc.sort.field, desc: !!tc.sort.desc } : null;
  const draw = () => {
    if (sort) {
      const c = cols.find((x) => x.name === sort.name || x.field === sort.name);
      if (c) data.sort((a, b) => { const x = cellValue(a, c), y = cellValue(b, c); const r = typeof x === "number" && typeof y === "number" ? x - y : String(x ?? "").localeCompare(String(y ?? ""), undefined, { numeric: true }); return sort.desc ? -r : r; });
    }
    const unitOfCol = (c) => ({ unit: c.unit ?? (c.isValue ? w.unit : "string"), decimals: c.decimals ?? w.decimals, mappings: c.mappings, thresholds: c.thresholds, min: c.min, max: c.max });
    const head = tc.header === false ? "" : `<thead><tr>${cols.map((c, i) => `<th data-c="${i}" class="${c.isValue ? "num" : ""}" aria-sort="${sort && (sort.name === c.name || sort.name === c.field) ? (sort.desc ? "descending" : "ascending") : "none"}">${esc(c.name)}</th>`).join("")}</tr></thead>`;
    const bodyRows = data.map((row) => `<tr>${cols.map((c, i) => {
      const v = cellValue(row, c);
      const txt = c.isValue ? valueText(w, v, unitOfCol(c)) : String(v);
      if (c.cell === "bar" && c.isValue) return `<td class="num"><span class="cell-bar" data-r="${data.indexOf(row)}" data-c="${i}"><i></i><span>${esc(txt)}</span></span></td>`;
      const cell = !c.isValue && row.href ? `<a href="${esc(row.href)}">${esc(txt)}</a>` : esc(txt);
      return `<td class="${c.isValue ? "num" : ""}" data-r="${data.indexOf(row)}" data-c="${i}">${cell}</td>`;
    }).join("")}</tr>`).join("");
    let foot = "";
    if (tc.footer) {
      foot = `<tfoot><tr>${cols.map((c, i) => {
        if (!c.isValue) return i === 0 ? `<td>${esc(CALCS[tc.footer]?.label || tc.footer)}</td>` : "<td></td>";
        const v = calc(tc.footer, data.map((r) => cellValue(r, c)));
        return `<td class="num">${esc(valueText(w, v, unitOfCol(c)))}</td>`;
      }).join("")}</tr></tfoot>`;
    }
    body.innerHTML = data.length ? `<div class="table-wrap"><table class="grid-table">${head}<tbody>${bodyRows}</tbody>${foot}</table></div>` : `<div class="empty"><p>${t("nodata")}</p></div>`;
    cols.forEach((c, i) => { if (c.width) { const th = $(`th[data-c="${i}"]`, body); if (th) th.style.width = c.width + "px"; } });
    $$("[data-r]", body).forEach((el) => {
      const c = cols[+el.dataset.c]; if (!c.isValue || !c.cell) return;
      const v = cellValue(data[+el.dataset.r], c);
      const col = valueColor({ ...w, ...unitOfCol(c), color: c.cell === "bar" ? (c.thresholds || w.thresholds ? "thresholds" : `fixed:${css("--accent")}`) : w.color }, v, unitOfCol(c), 0);
      if (c.cell === "bar") {
        const frac = v == null ? 0 : Math.max(0, Math.min(1, (v - (c.min ?? 0)) / ((c.max ?? 100) - (c.min ?? 0) || 1)));
        const bar = $("i", el); bar.style.width = (frac * 100).toFixed(1) + "%"; bar.style.background = withAlpha(col || css("--accent"), 0.35);
      } else if (c.cell === "color-background" && col) { el.style.background = col; el.style.color = inkOn(col); }
      else if (c.cell === "color-text" && col) el.style.color = col;
    });
    $$("th[data-c]", body).forEach((th) => {
      th.tabIndex = 0;
      const sortBy = () => {
        const c = cols[+th.dataset.c];
        sort = sort && (sort.name === c.name) ? { name: c.name, desc: !sort.desc } : { name: c.name, desc: c.isValue };
        draw();
      };
      th.onclick = sortBy;
      th.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); sortBy(); } };
    });
  };
  draw();
}

/** State timeline: bands per value (using the mappings) for each series, or one row with a label's value. */
function renderTimeline(w, body, series, tr) {
  const o = w.timeline || {};
  const xs = [...new Set(series.flatMap((s) => s.points.map((p) => p[0])))].sort((a, b) => a - b);
  const step = xs.length > 1 ? Math.min(...xs.slice(1).map((x, i) => x - xs[i])) : 60;
  let rows;
  if (o.field) {
    const state = new Map();
    for (const s of series) for (const [x, v] of s.points) if (v != null && !Number.isNaN(v) && v !== 0) state.set(x, s.labels[o.field] ?? "");
    rows = [{ name: o.name || o.field, states: xs.map((x) => state.get(x) ?? null), style: {} }];
  } else {
    rows = series.map((s) => { const m = new Map(s.points); return { name: s.name, states: xs.map((x) => m.get(x) ?? null), style: s.style || {} }; });
  }
  if (!rows.length || !xs.length) { body.innerHTML = `<div class="empty"><p>${t("chart.nodata")}</p></div>`; return; }
  const span = tr.end - tr.start || 1;
  const pct = (x) => Math.max(0, Math.min(100, ((x - tr.start) / span) * 100));
  const distinct = [];
  const colorFor = (v, st) => {
    const c = valueColor({ ...w, color: w.color || (w.thresholds ? "thresholds" : null) }, v, st, 0);
    if (c) return c;
    const key = String(v); if (!distinct.includes(key)) distinct.push(key);
    return palette()[distinct.indexOf(key) % palette().length];
  };
  const bands = [];
  const html = rows.map((r, ri) => {
    const segs = [];
    let cur = null;
    r.states.forEach((v, j) => {
      const x = xs[j];
      if (v == null) { cur = null; return; }
      if (cur && cur.v === v && (o.merge !== false) && x - cur.end <= step * 1.5) { cur.end = x + step; return; }
      cur = { v, start: x, end: x + step }; segs.push(cur);
    });
    return `<div class="tl-row"><span class="tl-name" data-tip="${esc(r.name)}">${esc(r.name)}</span><div class="tl-track">${segs.map((s) => {
      const i = bands.push({ ...s, st: r.style }) - 1;
      const text = valueText(w, s.v, r.style);
      return `<span class="tl-band" data-b="${i}" tabindex="0" data-tip="${esc(text)} · ${esc(fmtTime(s.start, span > 86400))} – ${esc(fmtTime(Math.min(s.end, tr.end), span > 86400))}">${o.showValue === "never" ? "" : esc(text)}</span>`;
    }).join("")}</div></div>`;
  }).join("");
  // Ticks on round hours or days, like the chart axes
  const tickStep = [300, 900, 1800, 3600, 3 * 3600, 6 * 3600, 12 * 3600, 86400, 2 * 86400, 7 * 86400, 14 * 86400, 30 * 86400]
    .find((x) => span / x <= 6) || 30 * 86400;
  const tz = new Date().getTimezoneOffset() * 60;
  const ticks = [];
  for (let x = Math.ceil((tr.start - tz) / tickStep) * tickStep + tz; x <= tr.end; x += tickStep) ticks.push(x);
  const tickText = (x) => {
    const d = new Date(x * 1000);
    return tickStep >= 86400 ? fmtDate(d, { day: "numeric", month: "numeric" })
      : fmtDate(d, { hour: "2-digit", minute: "2-digit" });
  };
  const legendItems = (w.mappings || []).filter((m) => m.text);
  body.innerHTML = `<div class="timeline">${html}<div class="tl-axis">${ticks.map((x) => `<span data-x="${x}">${esc(tickText(x))}</span>`).join("")}</div>${
    legendItems.length ? `<div class="legend-list">${legendItems.map((m, i) => `<span class="li"><i class="swatch" data-m="${i}"></i>${esc(m.text)}</span>`).join("")}</div>` : ""}</div>`;
  $$(".tl-axis span", body).forEach((el) => {
    const p = pct(+el.dataset.x);
    el.style.left = p + "%";
    // At the edges, align the label inwards so it is not clipped
    if (p > 94) el.style.transform = "translateX(-100%)";
    else if (p < 6) el.style.transform = "none";
  });
  $$(".tl-band", body).forEach((el) => {
    const b = bands[+el.dataset.b];
    el.style.left = pct(b.start) + "%";
    el.style.width = Math.max(0.2, pct(Math.min(b.end, tr.end)) - pct(b.start)) + "%";
    const c = colorFor(b.v, b.st);
    el.style.background = c; el.style.color = inkOn(c);
    if (o.showValue !== "always" && el.offsetWidth < 48) el.textContent = "";
  });
  $$(".swatch[data-m]", body).forEach((e) => { const c = colorName(legendItems[+e.dataset.m].color || css("--muted")); e.style.background = c; e.style.borderColor = c; });
}

/** Heatmap: the hourly points of the series (added up) folded into day of week ×
 *  hour of day cells, Monday first, in the browser's time zone. Each cell is the
 *  mean of its hours in the range (or heatmap.reduce: sum, max). */
function renderHeatmap(w, body, series) {
  const how = w.heatmap?.reduce || "mean";
  const slots = Array.from({ length: 7 * 24 }, () => new Map());
  for (const s of series) for (const [x, v] of s.points) {
    if (v == null || !Number.isFinite(v)) continue;
    const d = new Date(x * 1000), m = slots[((d.getDay() + 6) % 7) * 24 + d.getHours()];
    m.set(x, (m.get(x) || 0) + v);
  }
  const vals = slots.map((m) => calc(how, [...m.values()]));
  const nums = vals.filter((v) => v != null);
  if (!nums.length) { body.innerHTML = `<div class="empty"><p>${t("chart.nodata")}</p></div>`; return; }
  const lo = w.min ?? Math.min(...nums), hi = w.max ?? Math.max(...nums);
  // 2026-03-02 is a Monday
  const day = (i, weekday) => fmtDate(new Date(2026, 2, 2 + i), { weekday });
  const hh = (h) => `${String(h % 24).padStart(2, "0")}:00`;
  const peak = vals.indexOf(Math.max(...nums));
  const label = t("hm.peak", { when: `${day(Math.floor(peak / 24), "long")} ${hh(peak % 24)}`, v: valueText(w, vals[peak]) });
  const rows = Array.from({ length: 7 }, (_, di) => `<span class="hm-day">${esc(day(di, "short"))}</span>` + Array.from({ length: 24 }, (_, h) =>
    `<i class="hm-cell" data-i="${di * 24 + h}" data-tip="${esc(`${day(di, "long")} ${hh(h)}–${hh(h + 1)} · ${valueText(w, vals[di * 24 + h])}`)}"></i>`).join("")).join("");
  body.innerHTML = `<div class="heatmap" role="img" aria-label="${esc(label)}"><div class="hm-grid">${rows}</div>` +
    `<div class="hm-axis"><span></span>${Array.from({ length: 24 }, (_, h) => `<span>${h % 3 ? "" : hh(h)}</span>`).join("")}</div>` +
    `<div class="hm-legend"><span>${esc(valueText(w, lo))}</span><i class="hm-ramp"></i><span>${esc(valueText(w, hi))}</span></div></div>`;
  // One hue from light to dark (the accent or color: fixed:<c>); thresholds if the panel has them
  const base = w.color?.startsWith("fixed:") ? colorName(w.color.slice(6)) : css("--accent");
  const tint = (f) => withAlpha(base, 0.12 + 0.88 * Math.max(0, Math.min(1, f)));
  const byThresholds = w.thresholds && (w.color ?? "thresholds") === "thresholds";
  $$(".hm-cell", body).forEach((el) => {
    const v = vals[+el.dataset.i];
    if (v != null) el.style.background = byThresholds ? thresholdColor(w.thresholds, v, w.min, w.max) : tint(hi > lo ? (v - lo) / (hi - lo) : 1);
  });
  $(".hm-ramp", body).style.background = byThresholds ? "none" : `linear-gradient(90deg, ${tint(0)}, ${tint(1)})`;
}

/** Log lines: the latest of the widget's log query, newest first, with a
 *  column per field (`logs.columns`) */
async function renderLogs(w, body, tr, signal) {
  const q = (w.queries || []).find((x) => !x.hide && x.logs != null);
  const o = w.logs || {};
  const p = logParams(q?.logs, varContext(), tr);
  p.set("limit", Math.min(o.limit || 100, 1000));
  p.set("docs_only", "true");
  const r = await fetch(`/api/v1/logs?${p}`, { signal });
  const j = await r.json().catch(() => ({ error: r.statusText }));
  if (!r.ok) throw new Error(j.error || r.statusText);
  if (!j.logs.length) { body.innerHTML = `<div class="empty"><p>${t("nologs")}</p></div>`; return; }
  const cols = o.columns?.length ? o.columns : ["service", "host", "body"];
  const withDate = tr.end - tr.start > 86400;
  const cell = (l, c) => { const v = String(lgValue(l, c) ?? ""); return `<td data-tip="${esc(v)}">${esc(v)}</td>`; };
  body.innerHTML = `<div class="table-wrap"><table class="grid-table wlogs"><thead><tr><th>${t("lg.time")}</th>${cols.map((c) => `<th>${esc(c)}</th>`).join("")}</tr></thead><tbody>${
    j.logs.map((l) => `<tr><td><span class="sev ${esc(l.level)}" aria-label="${esc(SEV_NAME[l.level] ?? l.level)}"></span>${lgTime(l.ts, withDate)}</td>${cols.map((c) => cell(l, c)).join("")}</tr>`).join("")}</tbody></table></div>`;
}

/** Minimal Markdown (headings, lists, bold, italics, code and links) over already-escaped text */
function markdown(src) {
  const inline = (s) => esc(s).replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>").replace(/\*([^*]+)\*/g, "<i>$1</i>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, (m, t, u) => `<a href="${u}" target="_blank" rel="noopener">${t}</a>`);
  const out = []; let list = false;
  for (const line of String(src || "").split("\n")) {
    const h = /^(#{1,4})\s+(.*)/.exec(line), li = /^\s*[-*]\s+(.*)/.exec(line);
    if (!li && list) { out.push("</ul>"); list = false; }
    if (h) out.push(`<h${h[1].length + 2}>${inline(h[2])}</h${h[1].length + 2}>`);
    else if (li) { if (!list) { out.push("<ul>"); list = true; } out.push(`<li>${inline(li[1])}</li>`); }
    else if (line.trim()) out.push(`<p>${inline(line)}</p>`);
  }
  if (list) out.push("</ul>");
  return out.join("");
}

// ── Widgets ─────────────────────────────────────────────────────────────────
function explorerLink(w) {
  const ctx = varContext();
  // Log queries open the logs explorer: the query, the columns and the first by field as the breakdown
  const lq = (w.queries || []).find((q) => !q.hide && q.logs != null);
  if (lq) {
    const p = new URLSearchParams();
    if (lq.logs) p.set("q", interpolate(lq.logs, ctx));
    if (w.logs?.columns?.length) p.set("cols", w.logs.columns.join(","));
    const by = [].concat(lq.by || [])[0];
    if (by) p.set("bk", by);
    return `#/logs${p.toString() ? `?${p}` : ""}`;
  }
  const queries = (w.queries || []).filter((q) => q.expr).map((q, i) => ({ ...newQuery(LETTERS[i]), mode: "promql", expr: interpolate(q.expr, ctx), legend: q.legend || "", hidden: !!q.hide }));
  return `#/metrics?s=${encodeState({ queries, formulas: [], type: w.style?.stack ? (w.style.draw === "bars" ? "bar" : "area") : "line", log: false, compare: "" })}`;
}

async function runWidget(wd) {
  wd.dirty = false;
  wd.ctrl?.abort();
  const ctrl = wd.ctrl = new AbortController();
  const w = wd.def, body = wd.body;
  const tr = widgetTime(w);
  // Free the previous chart before replacing the widget's content
  $$(".chart-host", body).forEach(destroyChart);
  if (w.type === "text") { body.innerHTML = `<div class="md">${markdown(w.content)}</div>`; return; }
  if (w.legacy) return runLegacyWidget(wd, tr);
  wd.el.classList.add("loading");
  try {
    if (w.type === "logs") return await renderLogs(w, body, tr, ctrl.signal);
    const results = await runQueries(w, tr, body.clientWidth || 600, ctrl.signal);
    if (ctrl.signal.aborted) return;
    const series = buildSeries(w, results);
    switch (w.type) {
      case "stat": renderStat(w, body, series); break;
      case "gauge": renderGauge(w, body, series); break;
      case "bargauge": renderBargauge(w, body, series); break;
      case "pie": renderPie(w, body, series); break;
      case "bar": renderBar(w, body, series); break;
      case "table": renderTable(w, body, results.map((r) => ({ q: r.q, series: series.filter((s) => s.q.ref === r.q.ref) }))); break;
      case "timeline": renderTimeline(w, body, series, tr); break;
      case "heatmap": renderHeatmap(w, body, series); break;
      default:
        if (!series.length) body.innerHTML = `<div class="empty"><p>${t("chart.nodata")}</p></div>`;
        else renderTimeseries(w, body, series, tr);
    }
  } catch (e) {
    if (e.name === "AbortError") return;
    body.innerHTML = `<div class="werror" role="alert">${esc(e.message)}</div>`;
  } finally {
    if (wd.ctrl === ctrl) wd.el.classList.remove("loading");
  }
}

// Builder widgets (legacy dashboard format: query/queries + formulas)
function runLegacyWidget(wd, t) {
  const w = wd.def, body = wd.body;
  const apis = w.queries || [w.query];
  const queries = apis.map((a, i) => {
    const q = fromApi(a, a.id || LETTERS[i]);
    for (const v of board.varDefs) {
      const s = board.vars[v.name];
      if (v.field && s && !s.all && s.values[0]) { q.filters = q.filters.filter((f) => f.key !== v.field); q.filters.push({ key: v.field, op: "=", value: s.values[0] }); }
    }
    return q;
  });
  const formulas = (w.formulas || []).map((f) => ({ expr: f.expr || "", label: f.label || "", hidden: false }));
  wd.el.querySelector("a.open").href = `#/metrics?s=${encodeState({ queries, formulas, type: w.type || "line", log: false, compare: "" })}`;
  body.innerHTML = `<div class="ts"><div class="chart-host"></div><div class="legend-host"></div></div>`;
  runAll(queries, formulas, t, "").then(({ sets, unit, errors }) => {
    if (errors.length) throw new Error(errors.join(" · "));
    renderChart($(".chart-host", body), $(".legend-host", body), sets, { type: w.type || "line", height: "fit", unit, start: t.start, end: t.end, onZoom: (a, b) => setTime({ start: a, end: b }) });
  }).catch((e) => { body.innerHTML = `<div class="werror" role="alert">${esc(e.message)}</div>`; });
}

function widgetEl(w) {
  const el = document.createElement("article");
  el.className = `widget w-${w.legacy ? "timeseries" : w.type || "timeseries"}`;
  const [x, y, gw, gh] = w.grid || [0, 0, 12, 8];
  el.style.gridColumn = `${x + 1} / span ${gw}`;
  el.style.gridRow = `${y + 1} / span ${gh}`;
  el.style.setProperty("--h", gh);
  const logs = (w.queries || []).some((q) => q.logs != null);
  const link = w.type === "text" ? "" : `<a class="icon-btn open" href="${w.legacy ? "#/metrics" : esc(explorerLink(w))}" data-tip="${t(logs ? "dash.open_logs" : "dash.open")}" aria-label="${t("dash.open_aria")}">${ICON_OPEN}</a>`;
  const own = w.time ? `<span class="wtime" data-tip="${esc(t("dash.time", { d: w.time }))}" data-tip-only>${esc(w.time)}</span>` : "";
  el.innerHTML = `<header><h3 data-tip="${esc(w.title)}">${esc(w.title)}</h3>${own}<span class="tools">${w.description ? `<button class="icon-btn info" data-tip="${esc(w.description)}" data-tip-only aria-label="${esc(t("dash.desc", { d: w.description }))}">${ICON_INFO}</button>` : ""}${link}</span></header><div class="wbody"></div>`;
  return el;
}

function legacyToSections(def) {
  // Legacy format: widgets with a width (out of 24) and no position; laid out in order
  let x = 0, y = 0;
  const widgets = (def.widgets || def.panels || []).map((w) => {
    const gw = Math.min(24, w.width || 12), gh = 9;
    if (x + gw > 24) { x = 0; y += gh; }
    const out = { ...w, legacy: true, grid: [x, y, gw, gh] };
    x += gw;
    return out;
  });
  return [{ widgets }];
}

function buildBoard(def) {
  const host = $("#dash-grid");
  host.innerHTML = "";
  board.observer?.disconnect();
  board.widgets = [];
  board.observer = new IntersectionObserver((entries) => entries.forEach((e) => {
    const wd = e.target._wd; wd.visible = e.isIntersecting;
    if (e.isIntersecting && wd.dirty && !wd.section.collapsed) runWidget(wd);
  }), { rootMargin: "300px 0px" });
  const sections = def.sections || legacyToSections(def);
  sections.forEach((s, si) => {
    const sec = { collapsed: !!s.collapsed, widgets: [] };
    const box = document.createElement("section");
    box.className = "dsection";
    const rows = Math.max(1, ...(s.widgets || []).map((w) => (w.grid ? w.grid[1] + w.grid[3] : 9)));
    box.innerHTML = (s.title !== undefined || sections.length > 1 ? `<button class="section-head" aria-expanded="${!sec.collapsed}">${ICON_CHEVRON}<span>${esc(s.title || t("dash.untitled"))}</span><span class="count">${(s.widgets || []).length}</span></button>` : "") +
      `<div class="sgrid"${sec.collapsed ? " hidden" : ""}></div>`;
    const grid = $(".sgrid", box);
    grid.style.gridTemplateRows = `repeat(${rows}, ${ROW_PX}px)`;
    for (const w of s.widgets || []) {
      const el = widgetEl(w);
      grid.appendChild(el);
      const wd = { def: w, el, body: $(".wbody", el), section: sec, dirty: true, visible: false };
      el._wd = wd;
      sec.widgets.push(wd); board.widgets.push(wd);
      board.observer.observe(el);
    }
    const head = $(".section-head", box);
    if (head) head.onclick = () => {
      sec.collapsed = !sec.collapsed;
      head.setAttribute("aria-expanded", String(!sec.collapsed));
      grid.hidden = sec.collapsed;
      if (!sec.collapsed) requestAnimationFrame(() => sec.widgets.forEach((wd) => { if (wd.dirty && wd.visible) runWidget(wd); }));
    };
    host.appendChild(box);
  });
}

/** Re-queries: visible widgets now, the rest when they scroll into view */
function dashRefresh() {
  if (!board) return;
  for (const wd of board.widgets) {
    wd.dirty = true;
    if (wd.visible && !wd.section.collapsed) runWidget(wd);
  }
}

// ── Variables ───────────────────────────────────────────────────────────────
function syncVarsToUrl() {
  const qs = new URLSearchParams();
  for (const v of board.varDefs) {
    const s = board.vars[v.name]; if (!s) continue;
    if (s.all) qs.append(`var-${v.name}`, "$__all"); else s.values.forEach((x) => qs.append(`var-${v.name}`, x));
  }
  history.replaceState(null, "", `#/dashboards/${board.name}${qs.toString() ? "?" + qs : ""}`);
}
/** A widget's range: the dashboard's, or its last `time` (Grafana's relative time) */
function widgetTime(w) {
  const tr = dashTime(), secs = w.time ? durSecs(w.time) : NaN;
  return secs > 0 ? { start: tr.end - secs, end: tr.end } : tr;
}
/** Dashboard range. Like Grafana's "now delay": relative ranges are shifted back by
 * the ingestion delay (nowDelay, or the server's NOW_DELAY) so rates at the last point
 * and instant queries never see a half-filled window (a 60 s window with only
 * two samples showed half the CPU). */
function dashTime() {
  const t = resolveTime();
  const delay = durSecs(board?.def?.nowDelay ?? `${NOW_DELAY}s`) || 0;
  return time.preset && delay ? { start: t.start - delay, end: t.end - delay } : t;
}
async function resolveVariable(v, params, prev) {
  const t = dashTime();
  let options = [];
  if (v.query) {
    try { options = await variableOptions(v, varContext(), t); } catch (e) { options = []; board.varErrors[v.name] = e.message; }
  }
  const wanted = params?.getAll(`var-${v.name}`) || [];
  let values, all = false;
  if (wanted.includes("$__all") && v.includeAll) all = true;
  else if (wanted.length) values = wanted;
  else if (prev) { all = prev.all; values = prev.values?.filter((x) => options.includes(x)); }
  if (!all && !values?.length) {
    // No previous choice: "All" if the variable offers it, otherwise the first value (like Grafana)
    if (v.includeAll) all = true;
    else values = options.length ? [options[0]] : [];
  }
  board.vars[v.name] = { options, values: all ? options : values, all };
}
function renderVarControls() {
  const host = $("#dash-vars");
  host.innerHTML = "";
  for (const v of board.varDefs) {
    if (v.hidden) continue;
    const s = board.vars[v.name];
    const wrap = document.createElement("div");
    wrap.className = "dvar";
    const label = esc(v.label || v.name);
    if (v.multi) {
      const summary = s.all ? t("all") : s.values.join(", ") || t("dash.none");
      wrap.innerHTML = `<span class="dvar-label">${label}</span><div class="multi"><button class="select-like" aria-haspopup="true" aria-expanded="false"><span class="ellipsis">${esc(summary)}</span><svg viewBox="0 0 24 24"><path d="m7 10 5 5 5-5"/></svg></button>
        <div class="menu multi-menu" hidden>${v.includeAll ? `<label class="check"><input type="checkbox" data-all ${s.all ? "checked" : ""}> ${t("all")}</label>` : ""}${s.options.map((o) => `<label class="check"><input type="checkbox" value="${esc(o)}" ${!s.all && s.values.includes(o) ? "checked" : ""}> ${esc(o)}</label>`).join("")}</div></div>`;
      const btn = $("button", wrap), menu = $(".menu", wrap);
      btn.onclick = (e) => { e.stopPropagation(); menu.hidden = !menu.hidden; btn.setAttribute("aria-expanded", String(!menu.hidden)); };
      menu.onclick = (e) => e.stopPropagation();
      document.addEventListener("click", () => { menu.hidden = true; btn.setAttribute("aria-expanded", "false"); }, { once: true });
      menu.onchange = (e) => {
        if (e.target.dataset.all !== undefined) { s.all = e.target.checked; if (s.all) s.values = s.options; }
        else { s.all = false; s.values = $$("input[value]", menu).filter((i) => i.checked).map((i) => i.value); }
        if (s.all) $$("input[value]", menu).forEach((i) => { i.checked = false; });
        else { const a = $("input[data-all]", menu); if (a) a.checked = false; }
        $(".ellipsis", btn).textContent = s.all ? t("all") : s.values.join(", ") || t("dash.none");
        onVarChange(v);
      };
    } else {
      wrap.innerHTML = `<label class="dvar-label" for="var-${esc(v.name)}">${label}</label><select id="var-${esc(v.name)}">${v.includeAll ? `<option value="$__all">${t("all")}</option>` : ""}${s.options.map((o) => `<option>${esc(o)}</option>`).join("")}</select>`;
      const sel = $("select", wrap);
      sel.value = s.all ? "$__all" : s.values[0] ?? "";
      sel.onchange = () => { s.all = sel.value === "$__all"; s.values = s.all ? s.options : [sel.value]; onVarChange(v); };
    }
    if (board.varErrors[v.name]) { wrap.dataset.tip = board.varErrors[v.name]; wrap.classList.add("has-error"); }
    host.appendChild(wrap);
  }
}
async function onVarChange(changed) {
  // Recompute the variables that depend on this one (in order)
  const after = board.varDefs.slice(board.varDefs.indexOf(changed) + 1);
  const touched = new Set([changed.name]);
  for (const v of after) {
    if ([...touched].some((n) => new RegExp(`\\$\\{?${n}\\b|\\[\\[${n}\\]\\]`).test(v.query || ""))) {
      await resolveVariable(v, null, board.vars[v.name]);
      touched.add(v.name);
    }
  }
  syncVarsToUrl();
  renderVarControls();
  board.widgets.forEach((wd) => { if (!wd.def.legacy && wd.def.type !== "text") $("a.open", wd.el)?.setAttribute("href", explorerLink(wd.def)); });
  dashRefresh();
}

// ── Dashboards view ─────────────────────────────────────────────────────────
async function showDashboards(name, params) {
  dashList ||= (await api("/api/v1/dashboards")).dashboards;
  if (!name && dashList.length) { location.replace(`#/dashboards/${dashList[0].name}`); return; }
  $("#dash-list").innerHTML = dashList.map((d) => `<a href="#/dashboards/${d.name}" class="${d.name === name ? "active" : ""}">${esc(d.title)}</a>`).join("") ||
    `<p class="hint">${t("dash.empty")}</p>`;
  if (!name) return;
  const def = await api(`/api/v1/dashboards/${encodeURIComponent(name)}`);
  $("#view-title").textContent = def.title || name;
  // The dashboard's default range and refresh, unless others were chosen this session
  if (def.time && !timeChosen()) {
    const secs = durSecs(def.time);
    const preset = PRESETS.find(([k]) => durSecs(k) === secs)?.[0] || def.time;
    if (secs && time.preset !== preset) { time = { preset }; $("#time-label").textContent = timeLabel(); }
  }
  setAutoEvery(def.refresh ? durSecs(def.refresh) : PAGE_REFRESH);
  const prevVars = board?.name === name ? board.vars : {};
  board = { name, def, vars: {}, varErrors: {}, varDefs: def.variables || [], widgets: [] };
  // Legacy variables (field + metric): their values come from /labels
  for (const v of board.varDefs) {
    if (v.field && !v.query) {
      const labels = await labelsOf(v.metric).catch(() => []);
      const l = labels.find((x) => x.field === v.field);
      const options = (l?.values || []).slice().sort();
      const wanted = params?.get(`var-${v.name}`);
      board.vars[v.name] = { options, values: wanted && wanted !== "$__all" ? [wanted] : [], all: !wanted || wanted === "$__all" };
      v.includeAll = true;
    } else {
      await resolveVariable(v, params, prevVars[v.name]);
    }
  }
  renderVarControls();
  buildBoard(def);
}
