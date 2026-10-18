// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// PromQL: the Prometheus-compatible API, steps, legends and dashboard variables.
"use strict";

// "Round" steps (like Grafana's intervals) keep queries repeatable and cacheable
const NICE_STEPS = [15, 30, 60, 120, 300, 600, 900, 1200, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800, 604800];
/** Step for `range` seconds with at most `points` points and at least `min` seconds. */
function niceStep(range, points, min = 15) {
  const raw = Math.max(min, range / Math.max(1, points));
  return NICE_STEPS.find((s) => s >= raw) || Math.ceil(raw / 86400) * 86400;
}

// Cap concurrent queries: a large dashboard fires dozens of them.
// The server answers each in ~40 ms; with only 6 slots large dashboards sat in the queue.
const promSlots = { busy: 0, max: 12, queue: [] };
async function promSlot(fn) {
  if (promSlots.busy >= promSlots.max) await new Promise((r) => promSlots.queue.push(r));
  promSlots.busy++;
  try { return await fn(); } finally { promSlots.busy--; promSlots.queue.shift()?.(); }
}

async function promGet(path, params, signal) {
  const qs = new URLSearchParams();
  Object.entries(params).forEach(([k, v]) => [].concat(v).forEach((x) => x != null && qs.append(k, x)));
  return promSlot(async () => {
    if (signal?.aborted) throw new DOMException("cancelled", "AbortError");
    const r = await fetch(`${path}?${qs}`, { signal });
    const text = await r.text();
    let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 300) || r.statusText); }
    if (!r.ok || j.status === "error") throw new Error(j.error || r.statusText);
    return j;
  });
}
const promValue = (s) => (s === "NaN" ? NaN : s === "+Inf" ? Infinity : s === "-Inf" ? -Infinity : parseFloat(s));

const parseRange = (j, step) => {
  const d = j.data;
  const result = d.resultType === "matrix" ? d.result : [];
  return { series: result.map((r) => ({ labels: r.metric || {}, points: r.values.map(([t, v]) => [t, promValue(v)]) })), meta: { step, ...(j.atalaya || {}) } };
};
// Like Grafana, align the bounds to the step: the last evaluated point then falls
// exactly on `end` and the aggregation bucket in progress (its sample sits at the end
// of the window) is part of the last evaluation; otherwise a brand-new series with a
// single bucket would not show up until the next one.
function alignRange(start, end, step) {
  const s = Math.max(1, Math.round(step));
  return { start: Math.floor(start / s) * s, end: Math.floor(end / s) * s };
}
/** query_range → { series: [{ labels, points: [[t, v]] }], meta } */
async function promRange(expr, start, end, step, signal) {
  const a = alignRange(start, end, step);
  return parseRange(await promGet("/api/v1/query_range", { query: expr, start: a.start, end: a.end, step }, signal), step);
}

// ── Batches: all widget queries of a dashboard in a single request ───────────
// promRangeBatched calls made in the same tick are grouped and sent to
// /api/v1/query_range:batch (at most 32 per request). Each one keeps its own abort
// signal; if every query of an in-flight batch is aborted, the request is aborted.
const BATCH_MAX = 32;
let batchPending = [];
let batchTimer = 0;
function promRangeBatched(expr, start, end, step, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(new DOMException("cancelled", "AbortError"));
    const a = alignRange(start, end, step);
    const item = { q: { query: expr, start: a.start, end: a.end, step }, step, resolve, reject, signal, done: false };
    signal?.addEventListener("abort", () => { if (!item.done) { item.done = true; reject(new DOMException("cancelled", "AbortError")); } }, { once: true });
    batchPending.push(item);
    // Wait a few ms so widgets revealed by scrolling in the same frame go together
    if (!batchTimer) batchTimer = setTimeout(flushBatches, 30);
  });
}
function flushBatches() {
  batchTimer = 0;
  const items = batchPending.filter((i) => !i.done); batchPending = [];
  for (let i = 0; i < items.length; i += BATCH_MAX) sendBatch(items.slice(i, i + BATCH_MAX));
}
async function sendBatch(items) {
  const ctrl = new AbortController();
  const check = () => { if (items.every((i) => i.done)) ctrl.abort(); };
  items.forEach((i) => i.signal?.addEventListener("abort", check, { once: true }));
  try {
    const j = await promSlot(async () => {
      const r = await fetch("/api/v1/query_range:batch", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ queries: items.map((i) => i.q) }), signal: ctrl.signal });
      const text = await r.text();
      let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 300) || r.statusText); }
      if (!r.ok || j.status === "error") throw new Error(j.error || r.statusText);
      return j;
    });
    items.forEach((it, k) => {
      if (it.done) return; it.done = true;
      const res = j.results?.[k];
      if (!res || res.status === "error") it.reject(new Error(res?.error || "no result"));
      else it.resolve(parseRange(res, it.step));
    });
  } catch (e) {
    items.forEach((it) => { if (!it.done) { it.done = true; it.reject(e); } });
  }
}
/** query (instant) → the same shape, with one point per series */
async function promInstant(expr, time, signal) {
  const j = await promGet("/api/v1/query", { query: expr, time }, signal);
  const d = j.data;
  let series = [];
  if (d.resultType === "vector") series = d.result.map((r) => ({ labels: r.metric || {}, points: [[r.value[0], promValue(r.value[1])]] }));
  else if (d.resultType === "scalar") series = [{ labels: {}, points: [[d.result[0], promValue(d.result[1])]] }];
  else if (d.resultType === "matrix") series = d.result.map((r) => ({ labels: r.metric || {}, points: r.values.map(([t, v]) => [t, promValue(v)]) }));
  return { series, meta: j.atalaya || {} };
}
async function promLabelValues(label, match, start, end) {
  const j = await promGet(`/api/v1/label/${encodeURIComponent(label)}/values`, { "match[]": match || undefined, start, end });
  return j.data || [];
}

/** Grafana legendFormat: "{{device}} rx" */
function legendName(tpl, labels) { return tpl.replace(/\{\{\s*([^}\s]+?)\s*\}\}/g, (_, k) => labels[k] ?? ""); }
/** Default series name, as in Grafana: metric{labels} */
function selectorName(labels) {
  const { __name__: name = "", ...rest } = labels;
  const inner = Object.entries(rest).map(([k, v]) => `${k}="${v}"`).join(", ");
  return name + (inner ? `{${inner}}` : name ? "" : "{}");
}

// ── Dashboard variables ($var, ${var}, ${var:format}, [[var]]) ───────────────
// $__… macros are resolved by the server (it knows the resolution of each segment).
const regexEscape = (v) => String(v).replace(/[\\^$*+?.()|[\]{}]/g, "\\\\$&");
/** vars: { name: { values: [...], all: bool, multi, includeAll, allValue } } */
function interpolate(expr, vars) {
  return expr.replace(/\$\{(\w+)(?::(\w+))?\}|\[\[(\w+)\]\]|\$(\w+)/g, (m, a, fmt, b, c) => {
    const name = a || b || c;
    if (name.startsWith("__")) return m;
    const v = vars[name];
    if (!v) return m;
    if (v.all) return v.allValue ?? ".*";
    const vals = v.values || [];
    switch (fmt) {
      case "raw": return vals.join(",");
      case "regex": return vals.length > 1 ? `(${vals.map(regexEscape).join("|")})` : regexEscape(vals[0] ?? "");
      case "pipe": return vals.join("|");
      case "csv": return vals.join(",");
      default:
        if (!v.multi && !v.includeAll) return vals[0] ?? "";
        return vals.length > 1 ? `(${vals.map(regexEscape).join("|")})` : regexEscape(vals[0] ?? "");
    }
  });
}

/** Variable values: label_values(selector, label), label_values(label),
 *  metrics(regex) or query_result(expr), with Grafana's regex and sort semantics. */
async function variableOptions(v, vars, t) {
  const q = interpolate(v.query || "", vars).trim();
  let values = [];
  let m;
  if ((m = /^label_values\(\s*(?:([\s\S]*)\s*,\s*)?([A-Za-z_]\w*)\s*\)$/.exec(q))) {
    values = await promLabelValues(m[2], m[1]?.trim(), t.start, t.end);
  } else if ((m = /^metrics\(([\s\S]*)\)$/.exec(q))) {
    const re = new RegExp(m[1].trim() || ".*");
    values = (await promLabelValues("__name__", undefined, t.start, t.end)).filter((x) => re.test(x));
  } else if ((m = /^query_result\(([\s\S]*)\)$/.exec(q))) {
    const r = await promInstant(m[1], t.end);
    values = r.series.map((s) => `${selectorName(s.labels)} ${s.points[0]?.[1]} ${s.points[0]?.[0] * 1000}`);
  } else if (q) {
    values = q.split(",").map((x) => x.trim()).filter(Boolean); // fixed list
  }
  if (v.regex) {
    const rm = /^\/(.*)\/([gimsuy]*)$/.exec(v.regex);
    const re = new RegExp(rm ? rm[1] : v.regex, rm ? rm[2].replace("g", "") : "");
    values = values.flatMap((x) => {
      const mm = re.exec(x);
      if (!mm) return [];
      return [mm.groups?.value ?? mm[1] ?? x];
    });
  }
  values = [...new Set(values)];
  const cmp = { 1: (a, b) => a.localeCompare(b), 2: (a, b) => b.localeCompare(a), 3: (a, b) => parseFloat(a) - parseFloat(b), 4: (a, b) => parseFloat(b) - parseFloat(a),
    5: (a, b) => a.toLowerCase().localeCompare(b.toLowerCase()), 6: (a, b) => b.toLowerCase().localeCompare(a.toLowerCase()) }[v.sort];
  if (cmp) values.sort(cmp);
  return values;
}
