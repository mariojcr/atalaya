// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Discover-style (Kibana) log explorer: query bar, field panel with each field's
// most frequent values, a histogram that can be broken down by a field, and
// documents that expand into a table or JSON. The state (query, filters, columns
// and breakdown) lives in the hash, #/logs?q=…&f=…&cols=…&bk=…, so a search is a
// link. Data: /api/v1/logs, /logs/fields, /logs/context and /searches (saved searches).
"use strict";

// The "Logs" links on the home page and the timeline set the service here before navigating
const logState = { service: "", host: "", level: "", q: "" };

const LG_PAGE = 100, LG_LIMIT = 500, LG_CUT = 400;
const LG_FIXED = ["service", "host", "level", "stream"];
const lg = {
  q: "", filters: [], columns: [], breakdown: "",
  data: null, fields: null, fieldsError: "", key: "", ctrl: null, fctrl: null,
  shown: 0, open: new Set(), tab: new Map(), fopen: new Set(), fieldQ: "", hlRe: null, inited: false,
};
const ICO = {
  plus: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg>`,
  minus: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12h14"/></svg>`,
  col: `<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="4" y="4" width="16" height="16" rx="2"/><path d="M10 4v16M16 4v16"/></svg>`,
  chart: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 20V12M11 20V5M17 20v-9M3 20h18"/></svg>`,
  x: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18"/></svg>`,
  chev: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>`,
};

// ── Query and filters ──────────────────────────────────────────────────────
/** Splits the bar: `field:value`, `-field~text`, `field>=3` are filters; the rest are message words */
function lgParse(src) {
  const re = /(-?[\w.@/-]+)(>=|<=|[:~<>])(?:"([^"]*)"|(\S+))|"([^"]*)"|(\S+)/g;
  const filters = [], words = [];
  for (const m of String(src || "").matchAll(re)) {
    if (m[1]) filters.push(`${m[1]}${m[2]}${m[3] ?? m[4]}`);
    else { const w = m[5] ?? m[6]; if (w) words.push(w); }
  }
  return { filters, words };
}
/** Effective filters: the pills plus those in the bar (plain words search the message) */
function lgAllFilters() {
  const p = lgParse(lg.q);
  return [...lg.filters, ...p.filters, ...p.words.map((w) => `body~${w}`)];
}
function lgSplit(f) {
  const neg = f.startsWith("-"), s = neg ? f.slice(1) : f;
  const m = /^([^:~<>]+)(>=|<=|[:~<>])([\s\S]*)$/.exec(s);
  return m ? { neg, field: m[1], op: m[2], value: m[3] } : { neg, field: s, op: "", value: "" };
}
/** Adds a field:value pill (removes it if already present; replaces its negation) */
function lgAddFilter(field, value, neg) {
  const f = `${neg ? "-" : ""}${field}:${value}`, opp = `${neg ? "" : "-"}${field}:${value}`;
  const i = lg.filters.indexOf(f), o = lg.filters.indexOf(opp);
  if (i >= 0) lg.filters.splice(i, 1);
  else if (o >= 0) lg.filters[o] = f;
  else lg.filters.push(f);
  lgCommit();
}
/** A field's value in a document (fixed columns, then Attributes, then Resource) */
function lgValue(l, f) {
  if (f === "service" || f === "host" || f === "level" || f === "stream") return l[f];
  if (f === "body" || f === "message") return l.body;
  return l.attributes?.[f] ?? l.resource?.[f];
}
const lgDocKey = (l) => `${l.ts}|${l.service}|${l.body}`;

// ── State in the hash ──────────────────────────────────────────────────────
function lgHash() {
  const p = new URLSearchParams();
  if (lg.q) p.set("q", lg.q);
  lg.filters.forEach((f) => p.append("f", f));
  if (lg.columns.length) p.set("cols", lg.columns.join(","));
  if (lg.breakdown) p.set("bk", lg.breakdown);
  const s = p.toString();
  return "#/logs" + (s ? `?${s}` : "");
}
/** Writes the state to the hash; the router calls showLogs again, which only queries if the request changed */
function lgCommit() {
  const h = lgHash();
  if (location.hash !== h) location.hash = h;
  else showLogs(new URLSearchParams(h.split("?")[1] || ""));
}
const lgKey = () => JSON.stringify([time, lgAllFilters(), lg.breakdown]);

async function showLogs(params) {
  lgInit();
  lg.q = params.get("q") || "";
  lg.filters = params.getAll("f").filter(Boolean);
  lg.columns = (params.get("cols") || "").split(",").filter(Boolean);
  lg.breakdown = params.get("bk") || "";
  // Link from the home page or the timeline: the service becomes a pill
  if (logState.service) {
    lg.filters.push(`service:${logState.service}`); logState.service = "";
    history.replaceState(null, "", lgHash());
  }
  $("#log-q").value = lg.q;
  $("#lg-clear").hidden = !lg.q;
  lgPaintPills();
  lgPaintBreakdown();
  const key = lgKey();
  if (key !== lg.key || !lg.data) await lgQuery(key);
  else lgRender();
}
function logsRefresh() { lg.key = ""; showLogs(new URLSearchParams(location.hash.split("?")[1] || "")); }

// ── Data ───────────────────────────────────────────────────────────────────
async function lgQuery(key) {
  lg.ctrl?.abort(); lg.fctrl?.abort();
  const ctrl = lg.ctrl = new AbortController(), fctrl = lg.fctrl = new AbortController();
  lg.key = key;
  const tr = resolveTime(), filters = lgAllFilters();
  const p = new URLSearchParams({ start: tr.start, end: tr.end, limit: LG_LIMIT });
  filters.forEach((f) => p.append("f", f));
  if (lg.breakdown) p.set("breakdown", lg.breakdown);
  const status = $("#log-status"); status.className = "status"; status.textContent = t("querying");
  // The field panel is fetched separately and in parallel: it scans the keys of every document in the range
  const fp = new URLSearchParams({ start: tr.start, end: tr.end });
  filters.forEach((f) => fp.append("f", f));
  lg.fields = null; lg.fieldsError = ""; lgPaintFields();
  fetch(`/api/v1/logs/fields?${fp}`, { signal: fctrl.signal }).then((r) => r.json()).then((j) => {
    if (fctrl.signal.aborted) return;
    if (j.error) throw new Error(j.error);
    lg.fields = j.fields; lgPaintFields(); lgPaintBreakdown();
  }).catch((e) => { if (e.name !== "AbortError") { lg.fields = []; lg.fieldsError = e.message; lgPaintFields(); } });
  const t0 = performance.now();
  try {
    const r = await fetch(`/api/v1/logs?${p}`, { signal: ctrl.signal });
    const text = await r.text();
    let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 300) || r.statusText); }
    if (!r.ok) throw new Error(j.error || r.statusText);
    if (ctrl.signal.aborted) return;
    j.ms = Math.round(performance.now() - t0);
    j.hits = j.histogram.reduce((a, h) => a + h.n, 0);
    j.more = j.logs.length >= LG_LIMIT;
    lg.data = j; lg.shown = 0; lg.open.clear(); lg.tab.clear();
    status.textContent = "";
    lgRender();
  } catch (e) {
    if (e.name === "AbortError") return;
    status.className = "status error"; status.textContent = e.message;
  }
}
/** Documents older than those already loaded (each query is capped at LG_LIMIT) */
async function lgOlder(btn) {
  const j = lg.data, last = j.logs.at(-1); if (!last) return;
  const p = new URLSearchParams({ start: j.start, end: Math.floor(last.ts / 1000) + 1, limit: LG_LIMIT });
  lgAllFilters().forEach((f) => p.append("f", f));
  btn.disabled = true; btn.textContent = t("loading");
  try {
    const r = await api(`/api/v1/logs?${p}`);
    const seen = new Set(j.logs.slice(-300).map(lgDocKey));
    const add = r.logs.filter((l) => !seen.has(lgDocKey(l)));
    j.logs.push(...add); j.more = r.logs.length >= LG_LIMIT && add.length > 0;
    lgAppendPage();
  } catch (e) { const s = $("#log-status"); s.className = "status error"; s.textContent = e.message; btn.disabled = false; btn.textContent = t("lg.older"); }
}

// ── Rendering ──────────────────────────────────────────────────────────────
function lgRender() {
  const j = lg.data; if (!j) return;
  $("#lg-count").innerHTML = `${tn("lg.hits", j.hits)}<small>${t("lg.in", { ms: fmtInt(j.ms) })}</small>`;
  lgChart(j);
  lgRenderDocs();
  lgPaintFields();
}

/** Histogram: documents per bucket, or stacked by the values of the breakdown field */
function lgChart(j) {
  const host = $("#log-chart"), legend = $("#lg-legend");
  const colors = {};
  let sets;
  if (lg.breakdown && Array.isArray(j.breakdown)) {
    const OTHER = "\u0000", by = new Map();
    j.breakdown.forEach((r) => { const k = r.value === "…" ? OTHER : r.value; if (!by.has(k)) by.set(k, []); by.get(k).push([r.ts, r.n]); });
    const total = (k) => by.get(k).reduce((a, p) => a + p[1], 0);
    // Stable order by volume; "other" goes last, in grey
    const names = [...by.keys()].filter((k) => k !== OTHER).sort((a, b) => total(b) - total(a));
    const label = (k) => (k === OTHER ? t("lg.other") : k === "" ? t("lg.blank") : k);
    const PAL = palette();
    names.forEach((k, i) => { colors[label(k)] = PAL[i % PAL.length]; });
    if (by.has(OTHER)) { names.push(OTHER); colors[label(OTHER)] = css("--sev-debug"); }
    sets = [{ name: "", keys: ["v"], timeSeries: names.map((k) => ({ labels: { v: label(k) }, points: by.get(k) })) }];
  } else {
    const by = new Map();
    j.histogram.forEach((h) => by.set(h.ts, (by.get(h.ts) || 0) + h.n));
    colors[t("lg.count")] = css("--accent");
    sets = [{ name: "", keys: ["v"], timeSeries: [{ labels: { v: t("lg.count") }, points: [...by].sort((a, b) => a[0] - b[0]) }] }];
  }
  const multi = sets[0].timeSeries.length > 1;
  // Integer counts on the axis, tooltip and legend (fmtUnit "short" with no decimals)
  renderChart(host, multi ? legend : null, sets, { type: "bar", height: 150, start: j.start, end: j.end, colors, grafana: true, unit: "short", decimals: 0, onZoom: (a, b) => setTime({ start: a, end: b }), legend: { mode: "list", calcs: ["sum"] } });
  if (!multi) legend.innerHTML = "";
  // Clicking a bar zooms into its bucket (dragging still zooms to a selection)
  const u = host._u; if (!u || COARSE) return;
  let x0 = null;
  u.over.addEventListener("mousedown", (e) => { x0 = e.clientX; });
  u.over.addEventListener("click", (e) => {
    if (x0 == null || Math.abs(e.clientX - x0) > 4 || u.cursor.idx == null) return;
    const ts = u.data[0][u.cursor.idx], bx = u.valToPos(ts, "x");
    const half = Math.max(3, (u.valToPos(ts + j.step, "x") - bx) * 0.45);
    if (Math.abs(u.cursor.left - bx) > half) return;
    setTime({ start: ts, end: ts + j.step });
  });
}

/** Breakdown options: the fixed columns plus the attributes present in the range */
function lgPaintBreakdown() {
  const sel = $("#lg-bk");
  const extra = (lg.fields || []).map((f) => f.field).filter((f) => !LG_FIXED.includes(f));
  const opts = [...LG_FIXED, ...extra];
  if (lg.breakdown && !opts.includes(lg.breakdown)) opts.push(lg.breakdown);
  sel.innerHTML = `<option value="">${t("lg.bk_none")}</option>` + opts.map((f) => `<option value="${esc(f)}">${esc(f)}</option>`).join("");
  sel.value = lg.breakdown;
}

function lgPaintPills() {
  $("#lg-pills").innerHTML = lg.filters.map((f, i) => {
    const s = lgSplit(f);
    const val = s.value === "*" && s.op === ":" ? `<i>${t("lg.exists")}</i>` : esc(s.value);
    return `<span class="lg-pill${s.neg ? " neg" : ""}"><button class="pbody" data-i="${i}" data-act="toggle" data-tip="${t("lg.pill_toggle")}">${s.neg ? `<em>${t("lg.not")}</em>` : ""}<span class="k">${esc(s.field)}${esc(s.op === ":" ? ":" : ` ${s.op}`)}</span> ${val}</button><button class="px" data-i="${i}" data-act="rm" aria-label="${esc(t("lg.pill_rm", { f }))}">${ICO.x}</button></span>`;
  }).join("");
}

// ── Field panel ────────────────────────────────────────────────────────────
/** The field's values in the loaded documents (a sample, as Discover does) */
function lgSampleValues(field) { return (lg.data?.logs || []).map((l) => lgValue(l, field)).filter((v) => v != null); }
function lgFieldType(f, vals) {
  if (f.field === "level" || !vals.length) return "str";
  return vals.every((v) => v !== "" && Number.isFinite(+v)) ? "num" : "str";
}
function lgPaintFields() {
  const body = $("#lg-fields-body");
  if (!lg.fields) { body.innerHTML = `<p class="hint lg-pad">${lg.fieldsError ? esc(lg.fieldsError) : t("lg.fields_loading")}</p>`; return; }
  const q = lg.fieldQ.toLowerCase();
  const match = (name) => !q || name.toLowerCase().includes(q);
  const byName = new Map(lg.fields.map((f) => [f.field, f]));
  const selected = lg.columns.filter(match).map((c) => byName.get(c) || { field: c, docs: null, distinct: null, top: [] });
  const available = lg.fields.filter((f) => !lg.columns.includes(f.field) && match(f.field));
  const group = (title, items) => (items.length ? `<div class="lg-fgroup"><h3>${title} <span class="n">${items.length}</span></h3>${items.map(lgFieldHtml).join("")}</div>` : "");
  body.innerHTML = group(t("lg.selected"), selected) + group(t("lg.available"), available) || `<p class="hint lg-pad">${t("lg.no_fields")}</p>`;
  // Percentage bars are sized through the CSSOM (the CSP forbids style attributes)
  $$(".lg-fval .bar i", body).forEach((b) => { b.style.width = `${b.dataset.w}%`; });
}
function lgFieldHtml(f) {
  const open = lg.fopen.has(f.field), isCol = lg.columns.includes(f.field);
  const vals = lgSampleValues(f.field), type = lgFieldType(f, vals);
  const tip = f.distinct != null ? `${f.field} · ${t("lg.distinct", { n: fmtInt(f.distinct) })}` : f.field;
  return `<div class="lg-field${open ? " open" : ""}" data-f="${esc(f.field)}">
    <div class="lg-field-row">
      <span class="ftype ${type}" aria-hidden="true">${type === "num" ? "#" : "t"}</span>
      <button class="fname" data-act="fopen" aria-expanded="${open}" data-tip="${esc(tip)}">${esc(f.field)}</button>
      ${f.docs != null ? `<span class="fn">${fmtInt(f.docs)}</span>` : ""}
      <span class="fact">
        <button class="icon-btn" data-act="bk" aria-pressed="${lg.breakdown === f.field}" data-tip="${t("lg.visualize")}" aria-label="${t("lg.visualize")}">${ICO.chart}</button>
        <button class="icon-btn" data-act="col" aria-pressed="${isCol}" data-tip="${t(isCol ? "lg.rm_col" : "lg.add_col")}" aria-label="${t(isCol ? "lg.rm_col" : "lg.add_col")}">${isCol ? ICO.x : ICO.col}</button>
      </span>
    </div>
    ${open ? lgFieldValues(f, vals) : ""}
  </div>`;
}
const lgPm = (f, v) => `<span class="pm"><button data-act="inc" data-f="${esc(f)}" data-v="${esc(v)}" aria-label="${t("lg.filter_for")}" data-tip="${t("lg.filter_for")}">${ICO.plus}</button><button data-act="exc" data-f="${esc(f)}" data-v="${esc(v)}" aria-label="${t("lg.filter_out")}" data-tip="${t("lg.filter_out")}">${ICO.minus}</button></span>`;
const lgVal = (v) => (v === "" ? `<i>${t("lg.blank")}</i>` : esc(String(v).length > 60 ? String(v).slice(0, 60) + "…" : v));
function lgFieldValues(f, vals) {
  if (!vals.length) {
    return `<div class="lg-fvals"><p class="hint">${t("lg.top_range")}</p>${(f.top || []).map((v) => `<div class="lg-fval"><span class="v">${lgVal(v)}</span><span class="pct"></span>${lgPm(f.field, v)}</div>`).join("")}</div>`;
  }
  const cnt = new Map(); vals.forEach((v) => cnt.set(v, (cnt.get(v) || 0) + 1));
  const top = [...cnt].sort((a, b) => b[1] - a[1]).slice(0, 5);
  return `<div class="lg-fvals"><p class="hint">${t("lg.top_values", { n: fmtInt(vals.length) })}</p>${top.map(([v, n]) => {
    const pct = (n / vals.length) * 100;
    return `<div class="lg-fval"><span class="v" data-tip="${esc(v)}">${lgVal(v)}</span><span class="pct">${fmtNum(pct, pct < 10 ? 1 : 0)} %</span>${lgPm(f.field, v)}<span class="bar"><i data-w="${pct.toFixed(1)}"></i></span></div>`;
  }).join("")}</div>`;
}

// ── Documents ──────────────────────────────────────────────────────────────
const lgColumns = () => (lg.columns.length ? lg.columns : ["service", "host", "_doc"]);
function lgHlRe() {
  const ws = [...lgParse(lg.q).words, ...lg.filters.filter((f) => f.startsWith("body~")).map((f) => f.slice(5))].filter(Boolean);
  return ws.length ? new RegExp(ws.map((w) => esc(w).replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|"), "gi") : null;
}
function lgHl(s) { const h = esc(s ?? ""); return lg.hlRe ? h.replace(lg.hlRe, "<mark>$&</mark>") : h; }
/** Time with milliseconds; includes day and month unless only the time is requested */
function lgTime(ts, withDate = true) {
  const opts = withDate ? { day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit" } : { hour: "2-digit", minute: "2-digit", second: "2-digit" };
  return `${esc(fmtDate(ts, opts))}<small>.${String(ts % 1000).padStart(3, "0")}</small>`;
}
function lgHead() {
  const custom = lg.columns.length > 0;
  const th = (c) => (c === "_doc" ? `<th class="doc">${t("lg.document")}</th>`
    : `<th class="col" data-c="${esc(c)}"><span class="cn">${esc(c)}</span>${custom ? `<span class="thx"><button class="icon-btn" data-act="colleft" data-tip="${t("lg.col_left")}" aria-label="${t("lg.col_left")}">‹</button><button class="icon-btn" data-act="colright" data-tip="${t("lg.col_right")}" aria-label="${t("lg.col_right")}">›</button><button class="icon-btn" data-act="colrm" data-tip="${t("lg.rm_col")}" aria-label="${t("lg.rm_col")}">${ICO.x}</button></span>` : ""}</th>`);
  const col = (c) => (c === "_doc" ? "<col>" : custom ? `<col class="c-col">` : `<col class="c-def-${esc(c)}">`);
  return `<colgroup><col class="c-exp"><col class="c-time">${lgColumns().map(col).join("")}</colgroup><thead><tr><th class="lg-exp"></th><th class="time">${t("lg.time")}</th>${lgColumns().map(th).join("")}</tr></thead>`;
}
function lgDocSummary(l) {
  const body = l.body.length > LG_CUT ? l.body.slice(0, LG_CUT) + "…" : l.body;
  const attrs = Object.entries(l.attributes || {}).filter(([, v]) => v !== "").slice(0, 12);
  return `<div class="clamp"><span class="lg-body">${lgHl(body)}</span>${attrs.length ? ` <span class="lg-src">${attrs.map(([k, v]) => `<b>${esc(k)}:</b> <span>${esc(String(v).slice(0, 80))}</span>`).join(" ")}</span>` : ""}</div>`;
}
function lgRow(l, i) {
  const open = lg.open.has(i);
  const cell = (c) => (c === "_doc" ? `<td class="doc">${lgDocSummary(l)}</td>` : `<td class="col"><div class="clamp">${lgHl(lgValue(l, c) ?? "")}</div></td>`);
  return `<tr class="lg-row" data-i="${i}" aria-expanded="${open}"><td class="lg-exp"><button class="icon-btn" data-act="exp" aria-label="${t(open ? "lg.collapse" : "lg.expand")}">${ICO.chev}</button></td><td class="tcell"><span class="sev ${esc(l.level)}" aria-label="${esc(SEV_NAME[l.level] ?? l.level)}"></span>${lgTime(l.ts)}</td>${lgColumns().map(cell).join("")}</tr>${open ? lgDetailRow(l, i) : ""}`;
}
function lgDetailRow(l, i) {
  const tab = lg.tab.get(i) || "table";
  const span = lgColumns().length + 2;
  let inner;
  if (tab === "json") {
    const doc = { "@timestamp": new Date(l.ts).toISOString(), service: l.service, host: l.host, level: l.level, stream: l.stream, message: l.body, attributes: l.attributes || {}, resource: l.resource || {} };
    inner = `<pre>${esc(JSON.stringify(doc, null, 2))}</pre>`;
  } else {
    const rows = [["message", l.body, "body"], ["service", l.service], ["host", l.host], ["level", l.level], ["stream", l.stream],
      ...Object.entries(l.attributes || {}), ...Object.entries(l.resource || {}).map(([k, v]) => [k, v, "res"])];
    const acts = (k, v, kind) => {
      const f = kind === "body" ? "body" : k, isCol = lg.columns.includes(f);
      const filt = `<button data-act="inc" data-f="${esc(f)}" data-v="${esc(v)}" data-tip="${t("lg.filter_for")}" aria-label="${t("lg.filter_for")}">${ICO.plus}</button><button data-act="exc" data-f="${esc(f)}" data-v="${esc(v)}" data-tip="${t("lg.filter_out")}" aria-label="${t("lg.filter_out")}">${ICO.minus}</button>`;
      return `<span class="acts">${filt}<button data-act="col" data-f="${esc(f)}" aria-pressed="${isCol}" data-tip="${t(isCol ? "lg.rm_col" : "lg.add_col")}" aria-label="${t(isCol ? "lg.rm_col" : "lg.add_col")}">${isCol ? ICO.x : ICO.col}</button></span>`;
    };
    inner = `<table class="lg-kv">${rows.filter(([, v]) => v !== "" && v != null).map(([k, v, kind]) => `<tr><td class="a">${acts(k, v, kind)}</td><td class="k">${esc(k)}</td><td class="v">${lgHl(String(v))}</td></tr>`).join("")}</table>`;
  }
  return `<tr class="lg-detail" data-i="${i}"><td colspan="${span}">
    <div class="lg-dhead">
      <div class="tabs" role="tablist"><button role="tab" class="tab${tab === "table" ? " active" : ""}" data-act="tab" data-tab="table">${t("lg.tab_table")}</button><button role="tab" class="tab${tab === "json" ? " active" : ""}" data-act="tab" data-tab="json">JSON</button></div>
      <button class="outlined small" data-act="ctx">${t("lg.context")}</button>
    </div>${inner}</td></tr>`;
}
function lgMoreHtml() {
  const j = lg.data, left = j.logs.length - lg.shown;
  return `<div class="lg-more">${left > 0 ? `<button class="outlined" data-act="more">${t("lg.more", { n: fmtInt(Math.min(LG_PAGE, left)), r: fmtInt(left) })}</button>`
    : j.more ? `<button class="outlined" data-act="older">${t("lg.older")}</button>` : `<span class="hint">${t("lg.all_loaded", { n: fmtInt(j.logs.length) })}</span>`}</div>`;
}
function lgRenderDocs() {
  const j = lg.data, host = $("#lg-docs");
  lg.hlRe = lgHlRe();
  if (!j.logs.length) { host.innerHTML = `<div class="empty inline"><p>${t("lg.empty")}</p></div>`; return; }
  lg.shown = Math.max(lg.shown, Math.min(LG_PAGE, j.logs.length));
  host.innerHTML = `<table class="lg-table">${lgHead()}<tbody>${j.logs.slice(0, lg.shown).map(lgRow).join("")}</tbody></table>${lgMoreHtml()}`;
}
function lgAppendPage() {
  const j = lg.data, host = $("#lg-docs");
  const chunk = j.logs.slice(lg.shown, lg.shown + LG_PAGE);
  $(".lg-more", host)?.remove();
  $("tbody", host)?.insertAdjacentHTML("beforeend", chunk.map((l, k) => lgRow(l, lg.shown + k)).join(""));
  lg.shown += chunk.length;
  host.insertAdjacentHTML("beforeend", lgMoreHtml());
}
function lgToggleRow(i) {
  const l = lg.data.logs[i], row = $(`.lg-row[data-i="${i}"]`, $("#lg-docs")); if (!row) return;
  const open = !lg.open.has(i);
  if (open) lg.open.add(i); else lg.open.delete(i);
  $(`.lg-detail[data-i="${i}"]`, $("#lg-docs"))?.remove();
  row.setAttribute("aria-expanded", String(open));
  $("[data-act=exp]", row).setAttribute("aria-label", t(open ? "lg.collapse" : "lg.expand"));
  if (open) row.insertAdjacentHTML("afterend", lgDetailRow(l, i));
}
function lgMoveCol(c, dir) {
  const i = lg.columns.indexOf(c), k = i + dir;
  if (i < 0 || k < 0 || k >= lg.columns.length) return;
  [lg.columns[i], lg.columns[k]] = [lg.columns[k], lg.columns[i]];
  lgCommit();
}
function lgToggleCol(c) {
  const i = lg.columns.indexOf(c);
  if (i >= 0) lg.columns.splice(i, 1); else lg.columns.push(c);
  lgCommit();
}

// ── Surrounding documents ──────────────────────────────────────────────────
async function lgContext(l) {
  const dlg = $("#lg-ctx"); dlg.hidden = false; document.body.classList.add("drawer-open");
  $("#lg-ctx-title").textContent = t("lg.ctx_title", { t: fmtDate(l.ts, { dateStyle: "short", timeStyle: "medium" }) });
  $("#lg-ctx-sub").textContent = [l.service, l.host].filter(Boolean).join(" · ");
  const list = $("#lg-ctx-list"); list.innerHTML = `<p class="hint lg-pad">${t("loading")}</p>`;
  lg.ctxDoc = l;
  const p = new URLSearchParams({ service: l.service, host: l.host, ts: l.ts, n: $("#lg-ctx-n").value || 20 });
  try {
    const j = await api(`/api/v1/logs/context?${p}`);
    if (lg.ctxDoc !== l) return;
    const isAnchor = (d) => d.ts === l.ts && d.body === l.body;
    const row = (d) => `<div class="lg-ctx-row${isAnchor(d) ? " anchor" : ""}"><time>${lgTime(d.ts, false)}</time><span class="sev ${esc(d.level)}" aria-label="${esc(SEV_NAME[d.level] ?? d.level)}"></span><span class="msg">${esc(d.body)}</span></div>`;
    list.innerHTML = [...j.before, ...j.after].map(row).join("") || `<p class="hint lg-pad">${t("lg.ctx_empty")}</p>`;
    $(".anchor", list)?.scrollIntoView({ block: "center" });
    $("#lg-ctx-close").focus();
  } catch (e) { list.innerHTML = `<p class="werror lg-pad">${esc(e.message)}</p>`; }
}
function lgCloseContext() { $("#lg-ctx").hidden = true; document.body.classList.remove("drawer-open"); lg.ctxDoc = null; }

// ── Saved searches ─────────────────────────────────────────────────────────
async function lgSavedMenu() {
  const menu = $("#lg-saved-menu");
  menu.hidden = !menu.hidden; $("#lg-open").setAttribute("aria-expanded", String(!menu.hidden));
  if (menu.hidden) return;
  $("#lg-save-pop").hidden = true;
  menu.innerHTML = `<p class="hint lg-pad">${t("loading")}</p>`;
  try {
    const list = (await api("/api/v1/searches")).searches || [];
    menu.innerHTML = list.length ? list.map((s) => `<div class="lg-sitem"><button class="item" role="menuitem" data-id="${esc(s.id)}"><span>${esc(s.title)}</span><small>${esc(ago(s.updated))}</small></button><button class="icon-btn" data-del="${esc(s.id)}" aria-label="${esc(t("lg.del_search", { t: s.title }))}" data-tip="${t("delete")}">${ICO.x}</button></div>`).join("")
      : `<p class="hint lg-pad">${t("lg.saved_none")}</p>`;
  } catch (e) { menu.innerHTML = `<p class="werror lg-pad">${esc(e.message)}</p>`; }
}
async function lgSave() {
  const name = $("#lg-save-name").value.trim(); if (!name) { $("#lg-save-name").focus(); return; }
  const id = `s-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
  const status = $("#log-status");
  try {
    await fetch(`/api/v1/searches/${id}`, { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ title: name, q: lg.q, filters: lg.filters, columns: lg.columns, breakdown: lg.breakdown }) }).then(async (r) => { if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.statusText); });
    $("#lg-save-pop").hidden = true; $("#lg-save-name").value = "";
    status.className = "status"; status.textContent = t("lg.saved_ok", { t: name });
  } catch (e) { status.className = "status error"; status.textContent = e.message; }
}

// ── Query bar autocomplete ─────────────────────────────────────────────────
// While typing a term without an operator, the panel's fields are suggested
// (with their counts); after `field:` or `field~`, the field's most frequent
// values in the range under the other filters, fetched from /api/v1/logs/values
// with the typed text as prefix. ↑↓ move, Enter/Tab accept, Esc closes; with the
// dropdown closed, Enter searches as usual.
const lgAc = { items: [], sel: -1, timer: 0, ctrl: null, token: null, open: false };
const LG_AC_OPS = [[":", "lg.ac_op_eq"], ["~", "lg.ac_op_has"], [">", "lg.ac_op_gt"], ["<", "lg.ac_op_lt"], [":*", "lg.ac_op_exists"]];

/** Term under the caret: [start, end) in the text, `-` prefix, field, operator and typed value */
function lgAcToken() {
  const q = $("#log-q"), v = q.value, caret = q.selectionStart ?? v.length;
  let a = caret; while (a > 0 && !/\s/.test(v[a - 1])) a--;
  // Inside an open quote the term extends to the caret even across spaces
  const quoteAt = v.lastIndexOf('"', caret - 1);
  if (quoteAt >= 0 && (v.slice(0, quoteAt).match(/"/g) || []).length % 2 === 0) { let b = quoteAt; while (b > 0 && !/\s/.test(v[b - 1]) && !/[:~<>]/.test(v[b - 1])) b--; a = Math.min(a, b); }
  let b = caret; while (b < v.length && !/\s/.test(v[b])) b++;
  const text = v.slice(a, caret);
  const neg = text.startsWith("-"), body = neg ? text.slice(1) : text;
  const m = /^([\w.@/-]*)(>=|<=|[:~<>])?("?)(.*)$/s.exec(body);
  return { a, b, caret, text, neg, field: m[1], op: m[2] || "", quoted: m[3] === '"', value: m[4] };
}
function lgAcUpdate() {
  const q = $("#log-q");
  if (document.activeElement !== q) return;
  const tk = lgAcToken();
  lgAc.token = tk;
  clearTimeout(lgAc.timer); lgAc.ctrl?.abort();
  if (!tk.op) {
    // Fields: panel fields starting with the typed text first, then those containing it
    const typed = tk.field.toLowerCase();
    const fields = (lg.fields || []).map((f) => f.field);
    if (!fields.includes("body")) fields.push("body");
    const starts = fields.filter((f) => f.toLowerCase().startsWith(typed)), has = typed ? fields.filter((f) => !f.toLowerCase().startsWith(typed) && f.toLowerCase().includes(typed)) : [];
    const docs = Object.fromEntries((lg.fields || []).map((f) => [f.field, f.docs]));
    const list = [...starts, ...has].slice(0, 12);
    if (!list.length || (tk.text === "" && !lg.fields)) return lgAcClose();
    const items = list.map((f) => ({ kind: "field", label: f, hint: docs[f] != null ? fmtInt(docs[f]) : "", insert: `${tk.neg ? "-" : ""}${f}:`, keep: true }));
    return lgAcRender(items, t("lg.ac_fields"), typed);
  }
  if (tk.op === ":" && tk.value === "" && !tk.quoted) {
    // Operator just typed: after the values, offer the other operators for the field (> and < only if numeric)
    const info = (lg.fields || []).find((f) => f.field === tk.field);
    const num = info ? lgFieldType(info, lgSampleValues(tk.field)) === "num" : false;
    const ops = LG_AC_OPS.filter(([o]) => o !== ":" && (num || (o !== ">" && o !== "<")));
    lgAcFetch(tk, ops.map(([o, k]) => ({ kind: "op", label: `${tk.neg ? "-" : ""}${tk.field}${o}`, hint: t(k), insert: `${tk.neg ? "-" : ""}${tk.field}${o}`, keep: o !== ":*" })));
    return;
  }
  if (tk.op === ":" || tk.op === "~") return lgAcFetch(tk, []);
  lgAcClose();
}
/** Fetches the field's values (debounced so not every keystroke queries) and renders them before `extra` */
function lgAcFetch(tk, extra) {
  const paint = (vals, loading) => {
    const items = [...vals.map((v) => ({ kind: "value", label: v.value, hint: fmtInt(v.n), insert: `${tk.neg ? "-" : ""}${tk.field}${tk.op}${lgAcQuote(v.value)} `, keep: false })), ...extra];
    lgAcRender(items, t("lg.ac_values", { f: tk.field }), tk.value, loading ? t("lg.ac_loading") : (vals.length ? "" : t("lg.ac_none")));
  };
  paint([], true);
  lgAc.timer = setTimeout(async () => {
    const ctrl = lgAc.ctrl = new AbortController();
    const tr = resolveTime();
    const p = new URLSearchParams({ start: tr.start, end: tr.end, field: tk.field, prefix: tk.op === ":" ? tk.value : "" });
    // The other filters narrow the suggestions; the term being typed does not
    const q = $("#log-q"), rest = q.value.slice(0, tk.a) + q.value.slice(tk.b);
    const pr = lgParse(rest);
    [...lg.filters, ...pr.filters, ...pr.words.map((w) => `body~${w}`)].forEach((f) => p.append("f", f));
    try {
      const r = await fetch(`/api/v1/logs/values?${p}`, { signal: ctrl.signal });
      const j = await r.json();
      if (ctrl.signal.aborted || lgAc.token !== tk) return;
      if (j.error) throw new Error(j.error);
      // With ~ the server does not filter by prefix, so filter by substring here
      const vals = tk.op === "~" && tk.value ? j.values.filter((v) => v.value.toLowerCase().includes(tk.value.toLowerCase())) : j.values;
      paint(vals, false);
    } catch (e) { if (e.name !== "AbortError" && lgAc.token === tk) paint([], false); }
  }, 180);
}
const lgAcQuote = (v) => (/[\s"]/.test(v) ? `"${v.replace(/"/g, "")}"` : v);
function lgAcRender(items, head, typed, note) {
  const box = $("#lg-ac");
  lgAc.items = items; lgAc.sel = -1; lgAc.open = true;
  const hl = (label) => {
    if (!typed) return esc(label);
    const i = label.toLowerCase().indexOf(typed.toLowerCase());
    return i < 0 ? esc(label) : `${esc(label.slice(0, i))}<mark>${esc(label.slice(i, i + typed.length))}</mark>${esc(label.slice(i + typed.length))}`;
  };
  let html = "", lastKind = "";
  if (note) html += `<div class="lg-ac-h">${esc(head)}</div><div class="lg-ac-empty">${esc(note)}</div>`;
  items.forEach((it, i) => {
    const kind = it.kind === "op" ? t("lg.ac_ops") : head;
    if (kind !== lastKind) { html += `<div class="lg-ac-h">${esc(kind)}</div>`; lastKind = kind; }
    html += `<button type="button" role="option" id="lg-ac-${i}" data-i="${i}" aria-selected="${i === lgAc.sel}"><span>${hl(it.label)}</span>${it.hint ? `<small>${esc(it.hint)}</small>` : ""}</button>`;
  });
  box.innerHTML = html; box.hidden = false;
  const q = $("#log-q"); q.setAttribute("aria-expanded", "true"); q.setAttribute("aria-activedescendant", lgAc.sel >= 0 ? `lg-ac-${lgAc.sel}` : "");
}
function lgAcClose() {
  clearTimeout(lgAc.timer); lgAc.ctrl?.abort();
  lgAc.open = false; lgAc.items = []; lgAc.sel = -1; lgAc.token = null;
  const box = $("#lg-ac"); box.hidden = true; box.innerHTML = "";
  const q = $("#log-q"); q.setAttribute("aria-expanded", "false"); q.removeAttribute("aria-activedescendant");
}
function lgAcMove(d) {
  if (!lgAc.items.length) return;
  lgAc.sel = (lgAc.sel + d + lgAc.items.length) % lgAc.items.length;
  $("#lg-ac").querySelectorAll("[role=option]").forEach((b, i) => b.setAttribute("aria-selected", String(i === lgAc.sel)));
  $("#lg-ac").querySelector("[aria-selected=true]")?.scrollIntoView({ block: "nearest" });
  $("#log-q").setAttribute("aria-activedescendant", `lg-ac-${lgAc.sel}`);
}
/** Replaces the term under the caret with the suggestion; fields keep the dropdown open to continue with the value */
function lgAcPick(i) {
  const it = lgAc.items[i], tk = lgAc.token; if (!it || !tk) return;
  const q = $("#log-q");
  const before = q.value.slice(0, tk.a), after = q.value.slice(tk.b);
  const ins = it.insert;
  q.value = (before + ins + after.replace(/^\s+/, it.keep ? "" : " ")).trimEnd() + (it.keep ? "" : " ");
  const pos = before.length + ins.length;
  q.setSelectionRange(pos, pos);
  $("#lg-clear").hidden = !q.value;
  if (it.keep) lgAcUpdate(); else lgAcClose();
}
/** Dropdown key handling; returns true if the key was consumed */
function lgAcKey(e) {
  if (!lgAc.open) {
    if (e.key === "ArrowDown") { lgAcUpdate(); if (lgAc.open) { e.preventDefault(); return true; } }
    return false;
  }
  if (e.key === "ArrowDown") { e.preventDefault(); lgAcMove(1); return true; }
  if (e.key === "ArrowUp") { e.preventDefault(); lgAcMove(-1); return true; }
  if (e.key === "Escape") { e.preventDefault(); lgAcClose(); return true; }
  if (e.key === "Enter" && lgAc.sel >= 0) { e.preventDefault(); lgAcPick(lgAc.sel); return true; }
  if (e.key === "Tab" && !e.shiftKey && lgAc.items.length) { e.preventDefault(); lgAcPick(Math.max(lgAc.sel, 0)); return true; }
  if (e.key === "Enter") { lgAcClose(); return false; }
  return false;
}

// ── Setup ──────────────────────────────────────────────────────────────────
function lgInit() {
  if (lg.inited) return;
  lg.inited = true;
  const q = $("#log-q");
  const run = () => { lgAcClose(); lg.q = q.value.trim(); lgCommit(); };
  $("#lg-run").onclick = run;
  q.onkeydown = (e) => {
    if (lgAcKey(e)) return;
    if (e.key === "Enter") { e.preventDefault(); run(); }
  };
  q.oninput = () => { $("#lg-clear").hidden = !q.value; lgAcUpdate(); };
  q.onfocus = () => lgAcUpdate();
  q.onclick = () => lgAcUpdate();
  q.onblur = () => setTimeout(() => { if (document.activeElement !== q) lgAcClose(); }, 120);
  // Mouse: mousedown must not steal focus from the input; click picks
  $("#lg-ac").onmousedown = (e) => e.preventDefault();
  $("#lg-ac").onclick = (e) => { const b = e.target.closest("[role=option]"); if (b) lgAcPick(+b.dataset.i); };
  $("#lg-clear").onclick = () => { q.value = ""; lg.q = ""; lgAcClose(); lgCommit(); q.focus(); };
  $("#lg-bk").onchange = (e) => { lg.breakdown = e.target.value; lgCommit(); };
  // Pills: clicking toggles include/exclude, × removes
  $("#lg-pills").onclick = (e) => {
    const b = e.target.closest("button[data-act]"); if (!b) return;
    const i = +b.dataset.i, f = lg.filters[i]; if (f == null) return;
    if (b.dataset.act === "rm") lg.filters.splice(i, 1);
    else lg.filters[i] = f.startsWith("-") ? f.slice(1) : `-${f}`;
    lgCommit();
  };
  // Field panel
  $("#lg-field-q").oninput = (e) => { lg.fieldQ = e.target.value.trim(); lgPaintFields(); };
  $("#lg-fields-body").onclick = (e) => {
    const b = e.target.closest("button[data-act]"); if (!b) return;
    const field = b.closest(".lg-field")?.dataset.f, act = b.dataset.act;
    if (act === "fopen") { if (lg.fopen.has(field)) lg.fopen.delete(field); else lg.fopen.add(field); lgPaintFields(); }
    else if (act === "inc" || act === "exc") lgAddFilter(b.dataset.f, b.dataset.v, act === "exc");
    else if (act === "col") lgToggleCol(field);
    else if (act === "bk") { lg.breakdown = lg.breakdown === field ? "" : field; lgCommit(); }
  };
  // On mobile the panel is a bottom sheet
  const sheet = (open) => { $("#lg-fields").classList.toggle("open", open); $("#lg-fields-scrim").hidden = !open; $("#lg-fields-btn").setAttribute("aria-expanded", String(open)); };
  $("#lg-fields-btn").onclick = () => sheet(true);
  $("#lg-fields-close").onclick = () => sheet(false);
  $("#lg-fields-scrim").onclick = () => sheet(false);
  // Documents: expand, detail actions, columns and paging
  const docs = $("#lg-docs");
  docs.onclick = (e) => {
    const b = e.target.closest("button[data-act]");
    if (!b) { const row = e.target.closest(".lg-row"); if (row && !e.target.closest("a, mark")) lgToggleRow(+row.dataset.i); return; }
    const act = b.dataset.act, row = b.closest("tr"), i = row ? +row.dataset.i : -1;
    if (act === "exp") lgToggleRow(i);
    else if (act === "more") lgAppendPage();
    else if (act === "older") lgOlder(b);
    else if (act === "inc" || act === "exc") lgAddFilter(b.dataset.f, b.dataset.v, act === "exc");
    else if (act === "col") lgToggleCol(b.dataset.f);
    else if (act === "tab") { lg.tab.set(i, b.dataset.tab); row.outerHTML = lgDetailRow(lg.data.logs[i], i); }
    else if (act === "ctx") lgContext(lg.data.logs[i]);
    else if (act === "colrm") lgToggleCol(b.closest("th").dataset.c);
    else if (act === "colleft") lgMoveCol(b.closest("th").dataset.c, -1);
    else if (act === "colright") lgMoveCol(b.closest("th").dataset.c, 1);
  };
  docs.onkeydown = (e) => {
    if (e.key !== "Enter" && e.key !== " ") return;
    const row = e.target.closest(".lg-row"); if (row && e.target === row) { e.preventDefault(); lgToggleRow(+row.dataset.i); }
  };
  // Surrounding documents
  $("#lg-ctx-close").onclick = lgCloseContext;
  $("#lg-ctx").addEventListener("click", (e) => { if (e.target === e.currentTarget) lgCloseContext(); });
  $("#lg-ctx-n").onchange = () => { if (lg.ctxDoc) lgContext(lg.ctxDoc); };
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") { if (!$("#lg-ctx").hidden) lgCloseContext(); $("#lg-saved-menu").hidden = true; $("#lg-save-pop").hidden = true; } });
  // Saved searches and notebook
  $("#lg-open").onclick = (e) => { e.stopPropagation(); lgSavedMenu(); };
  $("#lg-saved-menu").onclick = async (e) => {
    e.stopPropagation();
    const del = e.target.closest("[data-del]"), item = e.target.closest("[data-id]");
    try {
      if (del) { await fetch(`/api/v1/searches/${encodeURIComponent(del.dataset.del)}`, { method: "DELETE" }); $("#lg-saved-menu").hidden = true; lgSavedMenu(); }
      else if (item) {
        const s = await api(`/api/v1/searches/${encodeURIComponent(item.dataset.id)}`);
        $("#lg-saved-menu").hidden = true;
        lg.q = s.q || ""; lg.filters = s.filters || []; lg.columns = s.columns || []; lg.breakdown = s.breakdown || "";
        lgCommit();
      }
    } catch (err) { const st = $("#log-status"); st.className = "status error"; st.textContent = err.message; }
  };
  $("#lg-save").onclick = (e) => { e.stopPropagation(); const pop = $("#lg-save-pop"); pop.hidden = !pop.hidden; $("#lg-saved-menu").hidden = true; if (!pop.hidden) $("#lg-save-name").focus(); };
  $("#lg-save-pop").onclick = (e) => e.stopPropagation();
  $("#lg-save-ok").onclick = lgSave;
  $("#lg-save-name").onkeydown = (e) => { if (e.key === "Enter") lgSave(); };
  document.addEventListener("click", () => { $("#lg-saved-menu").hidden = true; $("#lg-save-pop").hidden = true; });
  $("#lg-to-nb").onclick = (e) => pickNotebook(e.currentTarget, () => ({ type: "logs", filter: { service: "", level: "", q: "", f: lgAllFilters() }, time: time.preset ? undefined : { start: time.start, end: time.end } }));
}
function initLogs() { /* the view is set up on entry (showLogs) */ }
