// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Notebooks: an investigation as a document made of cells (text, PromQL, logs, note).
// The server only stores JSON (/api/v1/notebooks); this view defines its shape:
//   { id, title, created, updated, frozen, time: {preset} | {start, end},
//     cells: [{ id, type: text|note|promql|logs, body, query, legend, unit,
//               step: auto|1m|5m|1h, compare: ""|1h|1d|7d, view: chart|table,
//               filter: {service, level, q}, time?: {start, end} }] }
// Query cells store the query, not the result: they are re-run on open with the
// notebook's range (or their own, if pinned).
"use strict";

// rev counts edits: a save only clears "unsaved" if nothing changed while it was in flight
const nb = { list: null, doc: null, id: null, dirty: false, rev: 0, savedAt: null, saveTimer: 0, current: null, runs: new Map(), picker: null };
const NB_STEPS = [["auto", "auto"], ["1m", "1 min"], ["5m", "5 min"], ["1h", "1 h"]];
const NB_COMPARE = [["", t("cmp.no")], ["1h", t("cmp.1h_ago")], ["1d", t("cmp.1d_ago")], ["7d", t("cmp.7d_ago")]];
const NB_TYPES = { text: t("nb.text"), note: t("nb.note"), promql: t("nb.promql"), logs: t("nb.logs") };
// The type ends up in class attributes, so only allow the closed list (the server validates it too)
const cellType = (c) => (Object.hasOwn(NB_TYPES, c.type) ? c.type : "unknown");
const nbId = () => `nb-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
const cellId = () => `c-${Math.random().toString(36).slice(2, 9)}`;
const nbNarrow = () => matchMedia("(max-width: 860px)").matches;

async function nbApi(method, path, body) {
  const r = await fetch(path, { method, headers: body ? { "Content-Type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
  if (r.status === 204) return null;
  const text = await r.text();
  let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 300) || r.statusText); }
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}

/** "2 min ago", "yesterday", … */
function ago(sec) {
  if (!sec) return "";
  const d = Math.max(0, Math.floor(Date.now() / 1000) - sec);
  if (d < 45) return t("ago.now");
  if (d < 3600) return t("ago.min", { n: Math.round(d / 60) });
  if (d < 86400) return t("ago.h", { n: Math.round(d / 3600) });
  if (d < 2 * 86400) return t("ago.yesterday");
  return t("ago.d", { n: Math.round(d / 86400) });
}
const nbFmtDate = (s) => fmtDate(s * 1000, { dateStyle: "short", timeStyle: "short" });

// ── Notebook range ──────────────────────────────────────────────────────────
function nbTime(cell) {
  if (cell?.time?.start) return { start: cell.time.start, end: cell.time.end };
  const t = nb.doc.time || { preset: "1d" };
  if (t.start) return { start: t.start, end: t.end };
  const end = Math.floor(Date.now() / 1000) - 45; // like dashboards: data arrives with some delay
  return { start: end - (durSecs(t.preset) || 86400), end };
}
function nbRangeLabel() {
  const t = nb.doc.time || { preset: "1d" };
  if (t.start) return `${nbFmtDate(t.start)} – ${nbFmtDate(t.end)}`;
  return PRESETS.find((p) => p[0] === t.preset)?.[1] || t.preset;
}

// ── List ────────────────────────────────────────────────────────────────────
async function showNotebooks(parts, params) {
  if (parts[1]) return openNotebook(parts[1], params);
  $("#nb-doc").hidden = true; $("#nb-index").hidden = false;
  await flushPending();
  nb.id = null; nb.doc = null; closeDrawer?.();
  const status = $("#nb-status"); status.className = "status"; status.textContent = "";
  try {
    nb.list = (await nbApi("GET", "/api/v1/notebooks")).notebooks || [];
    if (!nb.list.length) { await createExample(); nb.list = (await nbApi("GET", "/api/v1/notebooks")).notebooks || []; }
  } catch (e) { status.className = "status error"; status.textContent = e.message; return; }
  renderIndex();
}
function renderIndex() {
  const host = $("#nb-list");
  host.innerHTML = nb.list.length ? nb.list.map((n) => `
    <article class="nb-card" data-id="${esc(n.id)}">
      <a class="nb-card-main" href="#/notebooks/${encodeURIComponent(n.id)}">
        <h3>${esc(n.title || t("nb.untitled"))}</h3>
        <p class="hint">${tn("nb.cells", n.cells)} · ${esc(ago(n.updated))}</p>
      </a>
      <div class="nb-card-actions"><button class="text small" data-act="del">${t("delete")}</button></div>
    </article>`).join("") : `<p class="dim">${t("nb.none")}</p>`;
  host.onclick = async (e) => {
    const b = e.target.closest("button[data-act]"); if (!b) return;
    const card = b.closest(".nb-card"), id = card.dataset.id;
    if (b.dataset.act === "del") {
      // Inline confirmation instead of confirm()
      b.closest(".nb-card-actions").innerHTML = `<span class="hint">${t("nb.confirm_del")}</span><button class="text small" data-act="no">${t("no")}</button><button class="outlined small danger" data-act="yes">${t("nb.yes_del")}</button>`;
    } else if (b.dataset.act === "no") renderIndex();
    else if (b.dataset.act === "yes") {
      try { await nbApi("DELETE", `/api/v1/notebooks/${encodeURIComponent(id)}`); nb.list = nb.list.filter((n) => n.id !== id); renderIndex(); }
      catch (err) { const s = $("#nb-status"); s.className = "status error"; s.textContent = err.message; }
    }
  };
}
async function newNotebook(title, cells) {
  const id = nbId();
  const now = Math.floor(Date.now() / 1000);
  const doc = { id, title: title || t("nb.new_title"), created: now, updated: now, frozen: false, time: { preset: time?.preset || "1d" }, cells: cells || [{ id: cellId(), type: "text", body: "" }] };
  await nbApi("PUT", `/api/v1/notebooks/${encodeURIComponent(id)}`, doc);
  return id;
}

// ── Document ────────────────────────────────────────────────────────────────
async function openNotebook(id, params) {
  $("#nb-index").hidden = true; $("#nb-doc").hidden = false;
  const status = $("#nb-status"); status.className = "status"; status.textContent = "";
  if (nb.id !== id) {
    await flushPending();
    nb.runs.forEach((r) => r.ctrl?.abort()); nb.runs.clear();
    try { nb.doc = await nbApi("GET", `/api/v1/notebooks/${encodeURIComponent(id)}`); }
    catch (e) { status.className = "status error"; status.textContent = e.message; $("#nb-cells").innerHTML = ""; return; }
    nb.id = id; nb.dirty = false; nb.savedAt = nb.doc.updated;
    nb.doc.cells ||= []; nb.doc.cells.forEach((c) => { c.id ||= cellId(); });
  }
  renderDoc();
  const focusCell = params?.get("cell");
  if (focusCell) document.getElementById(`cell-${focusCell}`)?.scrollIntoView({ block: "center" });
}
function markDirty() {
  nb.dirty = true; nb.rev++; paintSaved();
  clearTimeout(nb.saveTimer); nb.saveTimer = setTimeout(() => saveNotebook(), 2000); // autosave 2 s after the last edit
}
function paintSaved() {
  const el = $("#nb-saved");
  el.textContent = nb.dirty ? t("nb.unsaved") : nb.savedAt ? t("nb.saved", { t: ago(nb.savedAt) }) : "";
  el.classList.toggle("dirty", nb.dirty);
}
/** Saves the open document. The response applies to that document (not whichever
 *  is open when it arrives) and only clears "unsaved" if there were no further
 *  edits while it was in flight. */
async function saveNotebook() {
  const doc = nb.doc, id = doc?.id || nb.id, rev = nb.rev;
  if (!doc || !id) return;
  clearTimeout(nb.saveTimer);
  const status = $("#nb-status");
  try {
    const saved = await nbApi("PUT", `/api/v1/notebooks/${encodeURIComponent(id)}`, doc);
    doc.updated = saved?.updated || Math.floor(Date.now() / 1000);
    if (nb.doc !== doc) return;
    nb.savedAt = doc.updated;
    if (nb.rev === rev) nb.dirty = false;
    status.className = "status"; status.textContent = "";
  } catch (e) { if (nb.doc === doc) { status.className = "status error"; status.textContent = t("nb.save_fail", { e: e.message }); } }
  if (nb.doc === doc) paintSaved();
}
/** Before switching document or returning to the list: save pending edits */
async function flushPending() {
  if (nb.doc && nb.dirty) await saveNotebook();
}
async function shareNotebook() {
  const url = location.href.split("?")[0];
  const b = $("#nb-share");
  try { await navigator.clipboard.writeText(url); b.textContent = t("nb.copied"); }
  catch {
    // No clipboard access: show the link so it can be copied by hand
    const sel = document.createElement("input"); sel.value = url; sel.readOnly = true; sel.className = "nb-share-fallback";
    b.after(sel); sel.select(); try { document.execCommand("copy"); b.textContent = t("nb.copied"); } catch { b.textContent = t("nb.copy_manual"); }
    setTimeout(() => sel.remove(), 4000);
  }
  setTimeout(() => { b.textContent = t("nb.share"); }, 2500);
}

function renderDoc() {
  const d = nb.doc;
  $("#nb-title").textContent = d.title || t("nb.untitled");
  $("#nb-range-label").textContent = nbRangeLabel();
  $("#nb-freeze").setAttribute("aria-pressed", String(!!d.frozen));
  $("#nb-freeze").textContent = t(d.frozen ? "nb.frozen" : "nb.freeze");
  $("#nb-range-sel").hidden = !!d.frozen;
  paintSaved();
  renderOutline();
  const host = $("#nb-cells");
  host.innerHTML = d.cells.map((c, i) => cellHtml(c, i)).join("") || `<p class="dim">${t("nb.empty")}</p>`;
  d.cells.forEach((c) => mountCell(c));
}
function renderOutline() {
  const d = nb.doc;
  $("#nb-outline").innerHTML = d.cells.map((c, i) => `<button class="nb-ol ${c.id === nb.current ? "cur" : ""}" data-cell="${esc(c.id)}"><span class="nb-tag t-${cellType(c)}">${esc(NB_TYPES[cellType(c)] || c.type)}</span><span>${esc(cellTitle(c, i))}</span></button>`).join("");
}
function cellTitle(c, i) {
  if (c.type === "text" || c.type === "note") { const first = (c.body || "").split("\n").find((l) => l.trim()) || ""; return first.replace(/^#+\s*/, "").replace(/[*_`]+/g, "").slice(0, 60) || NB_TYPES[c.type]; }
  if (c.type === "promql") return c.legend || (c.query || "").slice(0, 60) || "PromQL";
  if (c.type === "logs") return [c.filter?.service && `logs · ${c.filter.service}`, c.filter?.level, c.filter?.q, ...(c.filter?.f || [])].filter(Boolean).join(" · ").slice(0, 60) || "Logs";
  return t("nb.cell_n", { n: i + 1 });
}

// ── Cells ───────────────────────────────────────────────────────────────────
function cellHtml(c, i) {
  const tools = `<div class="nb-tools">
      <button class="icon-btn sm" data-act="up" aria-label="${t("up")}" data-tip="${t("up")}"><svg viewBox="0 0 24 24"><path d="M12 19V5M6 11l6-6 6 6"/></svg></button>
      <button class="icon-btn sm" data-act="down" aria-label="${t("down")}" data-tip="${t("down")}"><svg viewBox="0 0 24 24"><path d="M12 5v14M6 13l6 6 6-6"/></svg></button>
      <button class="icon-btn sm" data-act="dup" aria-label="${t("dup")}" data-tip="${t("dup")}"><svg viewBox="0 0 24 24"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/></svg></button>
      <button class="icon-btn sm" data-act="del" aria-label="${t("delete")}" data-tip="${t("delete")}"><svg viewBox="0 0 24 24"><path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/></svg></button>
    </div>`;
  const head = `<header class="nb-cell-head"><button class="nb-cell-toggle" aria-expanded="true" aria-controls="cellbody-${esc(c.id)}"><span class="nb-tag t-${cellType(c)}">${esc(NB_TYPES[cellType(c)] || c.type)}</span><span class="nb-cell-title">${esc(cellTitle(c, i))}</span></button>${tools}</header>`;
  let body = "";
  if (c.type === "text" || c.type === "note") {
    body = `<div class="nb-md md" data-k="view" tabindex="0" role="button" aria-label="${t("nb.edit_text")}">${c.body?.trim() ? markdown(c.body) : `<p class="dim">${esc(t("nb.write_here"))}</p>`}</div><textarea class="nb-ta" data-k="edit" hidden spellcheck="false"></textarea>`;
  } else if (c.type === "promql") {
    const sel = (k, opts, v) => `<select data-k="${k}" aria-label="${k}">${opts.map(([val, lb]) => `<option value="${esc(val)}" ${val === (v ?? "") ? "selected" : ""}>${esc(lb)}</option>`).join("")}</select>`;
    body = `<div class="nb-q">
        <textarea class="nb-expr" data-k="query" rows="1" spellcheck="false" placeholder='sum by (mode) (irate(node_cpu_seconds_total{instance="host-1"}[$__rate_interval]))'>${esc(c.query || "")}</textarea>
        <span class="chip nb-range" data-k="range">${esc(c.time?.start ? `${nbFmtDate(c.time.start)} – ${nbFmtDate(c.time.end)}` : nbRangeLabel())}${c.time?.start ? `<button data-act="unpin" aria-label="${t("nb.use_doc_range")}" data-tip="${t("nb.use_doc_range")}">×</button>` : ""}</span>
        <button class="filled small" data-act="run">${t("run")}</button>
      </div>
      <div class="nb-controls">
        <label>${t("nb.step")} ${sel("step", NB_STEPS, c.step || "auto")}</label>
        <label>${t("nb.compare")} ${sel("compare", NB_COMPARE, c.compare || "")}</label>
        <label>${t("nb.legend")} <input data-k="legend" value="${esc(c.legend || "")}" placeholder="{{mode}}" aria-label="${t("nb.legend")}"></label>
        <span class="segmented mode" role="radiogroup" aria-label="${t("nb.show")}"><button role="radio" data-view="chart" aria-checked="${c.view !== "table"}">${t("nb.chart")}</button><button role="radio" data-view="table" aria-checked="${c.view === "table"}">${t("nb.table")}</button></span>
      </div>
      <div class="nb-result"><div class="chart-host nbchart" data-k="chart"></div><div class="legend-host" data-k="legend-host"></div><div class="nb-table" data-k="table" hidden></div></div>
      <div class="status" data-k="status"></div>`;
  } else if (c.type === "logs") {
    const f = c.filter || {};
    body = `<div class="nb-q nb-logq">
        <input data-k="service" value="${esc(f.service || "")}" placeholder="${t("nb.service_ph")}" aria-label="${t("logs.service")}">
        <select data-k="level" aria-label="${t("logs.severity")}">${[["", "lvl.all"], ["WARN,ERROR,FATAL", "lvl.warn"], ["ERROR,FATAL", "lvl.error"], ["INFO,WARN,ERROR,FATAL", "lvl.info"]].map(([v, l]) => `<option value="${v}" ${v === (f.level || "") ? "selected" : ""}>${t(l)}</option>`).join("")}</select>
        <input data-k="q" value="${esc(f.q || "")}" placeholder="${t("nb.text_ph")}" aria-label="${t("logs.text")}">
        <span class="chip nb-range" data-k="range">${esc(c.time?.start ? `${nbFmtDate(c.time.start)} – ${nbFmtDate(c.time.end)}` : nbRangeLabel())}${c.time?.start ? `<button data-act="unpin" aria-label="${t("nb.use_doc_range")}" data-tip="${t("nb.use_doc_range")}">×</button>` : ""}</span>
        <button class="filled small" data-act="run">${t("run")}</button>
      </div>
      <p class="hint nb-filter-text" data-k="ftext"></p>
      <div class="log-list compact nb-logs" data-k="logs"></div>
      <div class="status" data-k="status"></div>`;
  }
  return `<article class="nb-cell t-${cellType(c)}" id="cell-${esc(c.id)}" data-cell="${esc(c.id)}">${head}<div class="nb-cell-body" id="cellbody-${esc(c.id)}">${body}</div></article>`;
}

function mountCell(c) {
  const el = document.getElementById(`cell-${c.id}`); if (!el) return;
  const act = (k) => $(`[data-k="${k}"]`, el);
  if (c.type === "text" || c.type === "note") {
    const view = act("view"), ta = act("edit");
    const edit = () => { ta.value = c.body || ""; view.hidden = true; ta.hidden = false; autosize(ta); ta.focus(); };
    view.onclick = edit; view.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); edit(); } };
    ta.oninput = () => autosize(ta);
    ta.onblur = () => { if (ta.value !== (c.body || "")) { c.body = ta.value; markDirty(); } view.innerHTML = c.body?.trim() ? markdown(c.body) : `<p class="dim">${esc(t("nb.write_short"))}</p>`; ta.hidden = true; view.hidden = false; $(".nb-cell-title", el).textContent = cellTitle(c, nb.doc.cells.indexOf(c)); renderOutline(); };
    ta.onkeydown = (e) => { if (e.key === "Escape") ta.blur(); };
  }
  if (c.type === "promql") {
    const ta = act("query"); autosize(ta); document.fonts?.ready.then(() => autosize(ta));
    ta.oninput = () => autosize(ta);
    ta.onchange = () => { c.query = ta.value.trim(); markDirty(); renderOutline(); };
    ta.onkeydown = (e) => { if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); c.query = ta.value.trim(); markDirty(); runCell(c); } };
    act("step").onchange = (e) => { c.step = e.target.value; markDirty(); runCell(c); };
    act("compare").onchange = (e) => { c.compare = e.target.value; markDirty(); runCell(c); };
    act("legend").onchange = (e) => { c.legend = e.target.value.trim(); markDirty(); runCell(c); };
    $$('[role="radio"]', el).forEach((b) => b.onclick = () => { c.view = b.dataset.view; $$('[role="radio"]', el).forEach((x) => x.setAttribute("aria-checked", String(x === b))); markDirty(); paintView(c, el); });
    runCell(c);
  }
  if (c.type === "logs") {
    ["service", "q"].forEach((k) => { act(k).onchange = (e) => { c.filter = { ...(c.filter || {}), [k]: e.target.value.trim() }; markDirty(); renderOutline(); runCell(c); }; act(k).onkeydown = (e) => { if (e.key === "Enter") { e.target.blur(); } }; });
    act("level").onchange = (e) => { c.filter = { ...(c.filter || {}), level: e.target.value }; markDirty(); runCell(c); };
    runCell(c);
  }
  el.addEventListener("focusin", () => setCurrent(c.id));
  el.addEventListener("click", (e) => {
    setCurrent(c.id);
    const b = e.target.closest("button[data-act]"); if (!b) return;
    const a = b.dataset.act, i = nb.doc.cells.indexOf(c);
    if (a === "run") { if (c.type === "promql") c.query = act("query").value.trim(); runCell(c); }
    else if (a === "up" && i > 0) { nb.doc.cells.splice(i - 1, 2, c, nb.doc.cells[i - 1]); markDirty(); renderDoc(); }
    else if (a === "down" && i < nb.doc.cells.length - 1) { nb.doc.cells.splice(i, 2, nb.doc.cells[i + 1], c); markDirty(); renderDoc(); }
    else if (a === "dup") { nb.doc.cells.splice(i + 1, 0, { ...JSON.parse(JSON.stringify(c)), id: cellId() }); markDirty(); renderDoc(); }
    else if (a === "del") {
      if (b.dataset.sure !== "1") { b.dataset.sure = "1"; b.classList.add("danger"); b.setAttribute("data-tip", t("nb.press_again")); setTimeout(() => { b.dataset.sure = ""; b.classList.remove("danger"); b.setAttribute("data-tip", t("delete")); }, 3000); return; }
      nb.runs.get(c.id)?.ctrl?.abort(); nb.runs.delete(c.id);
      nb.doc.cells.splice(i, 1); markDirty(); renderDoc();
    }
    else if (a === "unpin") { delete c.time; markDirty(); renderDoc(); }
  });
  // On mobile, cells collapse to their header
  const tog = $(".nb-cell-toggle", el);
  tog.onclick = () => { const open = tog.getAttribute("aria-expanded") === "true"; tog.setAttribute("aria-expanded", String(!open)); el.classList.toggle("folded", open); };
  if (nbNarrow() && c.type !== "text") { tog.setAttribute("aria-expanded", "false"); el.classList.add("folded"); }
}
function autosize(ta) { ta.style.height = "auto"; ta.style.height = `${Math.max(36, ta.scrollHeight + 2)}px`; }
function setCurrent(id) { if (nb.current === id) return; nb.current = id; $$(".nb-ol", $("#nb-outline")).forEach((b) => b.classList.toggle("cur", b.dataset.cell === id)); }
function paintView(c, el) {
  const table = c.view === "table";
  $('[data-k="chart"]', el).hidden = table; $('[data-k="legend-host"]', el).hidden = table; $('[data-k="table"]', el).hidden = !table;
}

/** Runs a query cell and renders its result */
async function runCell(c) {
  const el = document.getElementById(`cell-${c.id}`); if (!el) return;
  const status = $('[data-k="status"]', el);
  nb.runs.get(c.id)?.ctrl?.abort();
  const ctrl = new AbortController(); nb.runs.set(c.id, { ctrl });
  const tr = nbTime(c);
  if (c.type === "promql") {
    if (!c.query?.trim()) { status.className = "status"; status.textContent = t("nb.write_query"); return; }
    status.className = "status"; status.textContent = t("querying");
    const host = $('[data-k="chart"]', el);
    const width = host.clientWidth || 800;
    const step = c.step && c.step !== "auto" ? durSecs(c.step) : niceStep(tr.end - tr.start, Math.max(100, width / 2));
    const offset = compareOffset(c.compare || "", tr);
    try {
      const t0 = performance.now();
      const run = (tt) => promRange(c.query, tt.start, tt.end, step, ctrl.signal);
      const [now, before] = await Promise.all([run(tr), offset ? run({ start: tr.start - offset, end: tr.end - offset }) : null]);
      if (ctrl.signal.aborted) return;
      const named = (r) => r.series.map((s) => ({ ...s, name: c.legend ? legendName(c.legend, s.labels) : undefined }));
      const keys = varyingKeys(now.series);
      const sets = [{ name: "", timeSeries: named(now), keys }];
      if (before) sets.push({ name: t("ago", { o: humanOffset(offset) }), timeSeries: named(before), keys, dashed: true, shift: offset, base: sets[0] });
      status.innerHTML = `<span>${t("st.series")} <b>${now.series.length}</b></span><span>${t("st.resolution")} <b>${tierName(now.meta.tier)}</b></span><span>${t("st.step")} <b>${step} s</b></span><span>${t("st.query")} <b>${Math.round(performance.now() - t0)} ms</b></span>`;
      const legendHost = $('[data-k="legend-host"]', el);
      renderChart(host, legendHost, sets, { type: "line", height: nbNarrow() ? 220 : 280, unit: c.unit || "", start: tr.start, end: tr.end, legend: { mode: "list", calcs: ["value"] } });
      $('[data-k="table"]', el).innerHTML = seriesTable(sets, c.unit || "");
      paintView(c, el);
    } catch (e) {
      if (e.name === "AbortError") return;
      status.className = "status error"; status.textContent = e.message;
      destroyChart(host); host.innerHTML = "";
    }
  } else if (c.type === "logs") {
    const f = c.filter || {};
    const p = new URLSearchParams({ start: tr.start, end: tr.end, limit: 50 });
    if (f.service) p.set("service", f.service); if (f.level) p.set("level", f.level); if (f.q) p.set("q", f.q);
    (f.f || []).forEach((x) => p.append("f", x)); // structured filters from the log explorer (field:value…)
    const ftext = [f.service && t("nb.f_service", { v: f.service }), f.level && t("nb.f_level", { v: f.level.split(",").join(", ") }), f.q && t("nb.f_msg", { v: f.q }), f.f?.length && t("nb.f_filters", { v: f.f.join("  ") })].filter(Boolean).join(" · ") || t("nb.nofilter");
    $('[data-k="ftext"]', el).textContent = t("nb.filter", { f: ftext, a: nbFmtDate(tr.start), b: nbFmtDate(tr.end) });
    status.className = "status"; status.textContent = t("querying");
    try {
      const r = await fetch(`/api/v1/logs?${p}`, { signal: ctrl.signal });
      const j = await r.json(); if (!r.ok) throw new Error(j.error || r.statusText);
      if (ctrl.signal.aborted) return;
      status.innerHTML = `<span>${t("st.entries")} <b>${j.logs.length}${j.logs.length >= 50 ? "+" : ""}</b></span>`;
      $('[data-k="logs"]', el).innerHTML = j.logs.length ? j.logs.map((l) => `<div class="log-row"><span class="sev ${esc(l.level)}" aria-label="${esc(SEV_NAME[l.level] ?? l.level)}"></span><time data-tip="${esc(fmtDate(l.ts, { dateStyle: "short", timeStyle: "medium" }))}">${esc(fmtDate(l.ts, { timeStyle: "medium" }))}</time><span class="svc">${esc(l.service)}</span><span class="msg" data-tip="${esc(l.body.length > 200 ? l.body : "")}">${esc(l.body.length > 200 ? l.body.slice(0, 200) + "…" : l.body)}</span></div>`).join("")
        : `<p class="dim">${t("nb.nologs")}</p>`;
    } catch (e) {
      if (e.name === "AbortError") return;
      status.className = "status error"; status.textContent = e.message;
    }
  }
}
/** Series table: name, last, min, max and mean */
function seriesTable(sets, unit) {
  const rows = [];
  for (const set of sets) {
    const keys = set.keys?.length ? set.keys : varyingKeys(set.timeSeries);
    for (const s of set.timeSeries) {
      const vs = s.points.map((p) => p[1]).filter((v) => v != null && !Number.isNaN(v));
      const name = [set.name, s.name ?? seriesName(s.labels, keys)].filter(Boolean).join(" · ");
      rows.push({ name, last: vs.at(-1), min: vs.length ? Math.min(...vs) : null, max: vs.length ? Math.max(...vs) : null, mean: vs.length ? vs.reduce((a, b) => a + b, 0) / vs.length : null });
    }
  }
  // The unit is the cell's, as typed, and fmtUnit returns unknown ones verbatim
  const f = (v) => (v == null ? "—" : esc(fmtUnit(v, unit || "short", 2)));
  return `<table class="legend"><thead><tr><th>${t("th.serie")}</th><th class="num">${t("th.last")}</th><th class="num">${t("th.min")}</th><th class="num">${t("th.max")}</th><th class="num">${t("th.mean")}</th></tr></thead><tbody>${rows.map((r) => `<tr><td>${esc(r.name)}</td><td class="num">${f(r.last)}</td><td class="num">${f(r.min)}</td><td class="num">${f(r.max)}</td><td class="num">${f(r.mean)}</td></tr>`).join("")}</tbody></table>`;
}
function notebookRefresh() { if (nb.doc && !nb.doc.frozen) nb.doc.cells.forEach((c) => { if (c.type !== "text" && c.type !== "note") runCell(c); }); }

// ── Adding cells from other views (explorer, timeline) ─────────────────────
function addCell(type, extra) {
  const c = { id: cellId(), type, ...(extra || {}) };
  if (type === "promql") { c.step ||= "auto"; c.compare ||= ""; c.view ||= "chart"; }
  if (type === "logs") c.filter ||= { service: "", level: "", q: "" };
  if (type === "text" || type === "note") c.body ||= "";
  const i = nb.current ? nb.doc.cells.findIndex((x) => x.id === nb.current) : -1;
  nb.doc.cells.splice(i >= 0 ? i + 1 : nb.doc.cells.length, 0, c);
  markDirty(); renderDoc();
  setTimeout(() => { const el = document.getElementById(`cell-${c.id}`); el?.scrollIntoView({ block: "center", behavior: "smooth" }); $(".nb-md, .nb-expr, input", el)?.focus?.(); }, 50);
}
/** "Which notebook?" picker next to the `anchor` button; `make()` returns the cell to add */
async function pickNotebook(anchor, make) {
  nb.picker?.remove();
  const pop = document.createElement("div"); pop.className = "menu nb-pick"; pop.setAttribute("role", "dialog"); pop.setAttribute("aria-label", t("nb.save_in"));
  pop.innerHTML = `<p class="hint">${t("nb.save_to")}</p><div class="nb-pick-list"><p class="dim">${t("loading")}</p></div><button class="outlined small" data-act="new">${t("nb.new_nb")}</button>`;
  anchor.parentElement.style.position = "relative"; anchor.after(pop); nb.picker = pop;
  const close = () => { pop.remove(); nb.picker = null; document.removeEventListener("click", outside, true); };
  const outside = (e) => { if (!pop.contains(e.target) && e.target !== anchor) close(); };
  setTimeout(() => document.addEventListener("click", outside, true), 0);
  pop.addEventListener("keydown", (e) => { if (e.key === "Escape") { close(); anchor.focus(); } });
  const go = async (id) => {
    const cell = make();
    const doc = await nbApi("GET", `/api/v1/notebooks/${encodeURIComponent(id)}`);
    doc.cells = doc.cells || []; doc.cells.push({ id: cellId(), ...cell });
    await nbApi("PUT", `/api/v1/notebooks/${encodeURIComponent(id)}`, doc);
    close(); nb.id = null; location.hash = `#/notebooks/${encodeURIComponent(id)}?cell=${doc.cells.at(-1).id}`;
  };
  pop.onclick = async (e) => {
    const b = e.target.closest("button"); if (!b) return;
    try {
      if (b.dataset.act === "new") { const id = await newNotebook(t("nb.investigation", { d: fmtDate(new Date(), { day: "numeric", month: "short" }) }), []); await go(id); }
      else if (b.dataset.id) await go(b.dataset.id);
    } catch (err) { $(".nb-pick-list", pop).innerHTML = `<p class="werror">${esc(err.message)}</p>`; }
  };
  try {
    const list = (await nbApi("GET", "/api/v1/notebooks")).notebooks || [];
    $(".nb-pick-list", pop).innerHTML = list.length ? list.slice(0, 8).map((n) => `<button class="nb-pick-item" data-id="${esc(n.id)}"><span>${esc(n.title || t("nb.untitled"))}</span><small>${esc(ago(n.updated))}</small></button>`).join("") : `<p class="dim">${t("nb.none")}</p>`;
    $("button", pop)?.focus();
  } catch (err) { $(".nb-pick-list", pop).innerHTML = `<p class="werror">${esc(err.message)}</p>`; }
}

// ── Example notebook ────────────────────────────────────────────────────────
// Created the first time there are none. Works on any installation with
// node-exporter: CPU per host, the split by mode and the log warnings.
async function createExample() {
  const now = Math.floor(Date.now() / 1000);
  const doc = {
    id: "example-cpu", title: t("nb.ex_title"), created: now, updated: now, frozen: false,
    time: { preset: "1d" },
    cells: [
      { id: cellId(), type: "text", body: t("nb.ex_1") },
      { id: cellId(), type: "promql", query: 'sum by (instance) (rate(node_cpu_seconds_total{mode!="idle"}[$__rate_interval]))', legend: "{{instance}}", step: "auto", compare: "", view: "chart" },
      { id: cellId(), type: "text", body: t("nb.ex_2") },
      { id: cellId(), type: "promql", query: 'sum by (mode) (rate(node_cpu_seconds_total{mode!="idle"}[$__rate_interval]))', legend: "{{mode}}", step: "auto", compare: "", view: "chart" },
      { id: cellId(), type: "logs", filter: { service: "", level: "WARN,ERROR,FATAL", q: "" } },
      { id: cellId(), type: "note", body: t("nb.ex_3") },
    ],
  };
  await nbApi("PUT", `/api/v1/notebooks/${doc.id}`, doc);
}

// ── Setup ───────────────────────────────────────────────────────────────────
function initNotebooks() {
  $("#nb-new").onclick = async () => { try { const id = await newNotebook(); nb.id = null; location.hash = `#/notebooks/${id}`; } catch (e) { const s = $("#nb-status"); s.className = "status error"; s.textContent = e.message; } };
  $("#nb-save").onclick = () => saveNotebook();
  $("#nb-share").onclick = shareNotebook;
  document.addEventListener("keydown", (e) => { if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s" && current === "notebooks" && nb.doc) { e.preventDefault(); saveNotebook(); } });
  // Title becomes editable on click
  const title = $("#nb-title");
  const editTitle = () => {
    const inp = document.createElement("input"); inp.className = "nb-title-input"; inp.value = nb.doc.title || ""; inp.setAttribute("aria-label", t("nb.title_aria"));
    title.replaceWith(inp); inp.focus(); inp.select();
    const done = () => { nb.doc.title = inp.value.trim() || t("nb.untitled"); inp.replaceWith(title); title.textContent = nb.doc.title; markDirty(); };
    inp.onblur = done; inp.onkeydown = (ev) => { if (ev.key === "Enter") inp.blur(); if (ev.key === "Escape") { inp.value = nb.doc.title; inp.blur(); } };
  };
  title.onclick = editTitle; title.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); editTitle(); } };
  // Notebook range and freezing
  const sel = $("#nb-range-sel");
  sel.innerHTML = PRESETS.map(([k, l]) => `<option value="${k}">${l}</option>`).join("");
  sel.onchange = () => { nb.doc.time = { preset: sel.value }; markDirty(); renderDoc(); };
  $("#nb-freeze").onclick = () => {
    if (nb.doc.frozen) { nb.doc.frozen = false; nb.doc.time = { preset: time?.preset || "1d" }; }
    else { const t = nbTime(); nb.doc.frozen = true; nb.doc.time = { start: t.start, end: t.end }; }
    markDirty(); renderDoc();
  };
  // Add-cell menu and outline navigation
  const addPop = $("#nb-add-pop");
  $("#nb-add").onclick = () => { addPop.hidden = !addPop.hidden; if (!addPop.hidden) $("button", addPop).focus(); };
  addPop.onclick = (e) => { const b = e.target.closest("button[data-type]"); if (b) { addPop.hidden = true; addCell(b.dataset.type); } };
  document.addEventListener("click", (e) => { if (!addPop.hidden && !e.target.closest("#nb-add, #nb-add-pop")) addPop.hidden = true; });
  $("#nb-outline").onclick = (e) => { const b = e.target.closest(".nb-ol"); if (!b) return; setCurrent(b.dataset.cell); document.getElementById(`cell-${b.dataset.cell}`)?.scrollIntoView({ block: "start", behavior: "smooth" }); $("#nb-side").classList.remove("open"); };
  $("#nb-cells-btn").onclick = () => $("#nb-side").classList.toggle("open");
  // From the explorer: the current query as a PromQL cell
  $("#ex-to-nb").onclick = (e) => {
    const q = ex.queries.find((x) => isProm(x) && x.expr?.trim());
    if (!q) { const s = $("#explorer-status"); s.className = "status error"; s.textContent = t("nb.only_promql"); return; }
    pickNotebook(e.currentTarget, () => ({ type: "promql", query: q.expr.trim(), legend: q.legend || "", step: "auto", compare: ex.compare && !ex.compare.startsWith("custom") && ex.compare !== "prev" && ex.compare !== "28d" ? ex.compare : "", view: "chart", time: time.preset ? undefined : { start: time.start, end: time.end } }));
  };
  // From the timeline: the logs of the cursor's bucket
  $("#tl-to-nb").onclick = (e) => {
    if (!tl.data || tl.cursor == null) return;
    const b = tl.data.bucket, t = tl.cursor;
    pickNotebook(e.currentTarget, () => ({ type: "logs", filter: { service: tl.service || "", level: "", q: "" }, time: { start: t - Math.floor(b / 2), end: t + Math.ceil(b / 2) } }));
  };
  setInterval(() => { if (current === "notebooks") paintSaved(); }, 30000);
  // When the width changes, re-render so mobile-collapsed cells unfold
  let narrow = nbNarrow();
  window.addEventListener("resize", () => { const n = nbNarrow(); if (n !== narrow) { narrow = n; if (nb.doc && !$("#nb-doc").hidden) renderDoc(); } });
}
document.addEventListener("DOMContentLoaded", initNotebooks);
