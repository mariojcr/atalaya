// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Alerts: the rules in alerts.yaml with their state (firing, pending, silenced,
// ok, error), their instances, the silences and the transition history for the
// global time range. Data: /api/v1/alerts (state), /alerts/history,
// /alerts/silences and /alerts/{id}/test (evaluates now without changing state).
"use strict";

const al = { data: null, history: null, open: new Set(), tests: new Map(), ctrl: null, hctrl: null, inited: false, silence: null };
const AL_STATE_ORDER = { error: 0, firing: 1, pending: 2, silenced: 3, ok: 4 };
const AL_SEV_ICON = {
  critical: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3 2 21h20L12 3z"/><path d="M12 10v5M12 18v.5"/></svg>`,
  warning: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 7v6M12 16v.5"/></svg>`,
  info: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 8v.5"/></svg>`,
};
const AL_STATE_ICON = {
  firing: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3 2 21h20L12 3z"/></svg>`,
  pending: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>`,
  silenced: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 9v6h4l5 4V5L9 9H5z"/><path d="m18 9 4 6M22 9l-4 6"/></svg>`,
  ok: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="m8 12 3 3 5-6"/></svg>`,
  error: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="m9 9 6 6M15 9l-6 6"/></svg>`,
};
const alKindOf = (sev) => sev || "warning";

async function alApi(method, path, body) {
  const r = await fetch(path, { method, headers: body ? { "Content-Type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
  if (r.status === 204) return null;
  const text = await r.text();
  let j; try { j = JSON.parse(text); } catch { throw new Error(text.slice(0, 300) || r.statusText); }
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}

async function showAlerts(params) {
  alInit();
  if (params.get("open")) al.open.add(params.get("open"));
  await alertsRefresh();
}

async function alertsRefresh() {
  al.ctrl?.abort(); al.hctrl?.abort();
  const ctrl = al.ctrl = new AbortController(), hctrl = al.hctrl = new AbortController();
  const status = $("#al-status");
  const tr = resolveTime();
  const hist = fetch(`/api/v1/alerts/history?${new URLSearchParams({ start: tr.start, end: tr.end, lang: LANG })}`, { signal: hctrl.signal })
    .then((r) => r.json()).then((j) => { if (!hctrl.signal.aborted) { al.history = j.history || []; renderHistory(); } }).catch(() => {});
  try {
    const r = await fetch(`/api/v1/alerts?lang=${LANG}`, { signal: ctrl.signal });
    const j = await r.json();
    if (ctrl.signal.aborted) return;
    if (j.error) throw new Error(j.error);
    al.data = j;
    status.className = "status"; status.textContent = "";
    renderAlerts();
  } catch (e) {
    if (e.name === "AbortError") return;
    status.className = "status error"; status.textContent = e.message;
  }
  await hist;
}

/** "2 min ago" / "hace 2 min" from Unix seconds */
function alAgo(sec) {
  if (!sec) return "";
  const d = Math.max(0, Math.floor(Date.now() / 1000) - sec);
  if (d < 45) return t("ago.now");
  if (d < 3600) return t("ago.min", { n: Math.round(d / 60) });
  if (d < 86400) return t("ago.h", { n: Math.round(d / 3600) });
  if (d < 2 * 86400) return t("ago.yesterday");
  return t("ago.d", { n: Math.round(d / 86400) });
}
const alDur = (s) => (s % 86400 === 0 && s >= 86400 ? `${s / 86400} d` : s % 3600 === 0 && s >= 3600 ? `${s / 3600} h` : s % 60 === 0 && s >= 60 ? `${s / 60} min` : `${s} s`);
const alLabels = (labels) => Object.entries(labels || {}).map(([k, v]) => `<span class="al-lbl"><span class="k">${esc(k)}</span>${esc(v)}</span>`).join("");
const alStateName = (s) => t(`al.state_${s}`);
const alSevName = (s) => t(`al.sev_${alKindOf(s)}`);

function renderAlerts() {
  const d = al.data; if (!d) return;
  const c = d.counts || {};
  const chips = ["firing", "pending", "silenced", "error", "ok"].map((k) => {
    const n = c[k] || 0;
    return `<span class="al-count ${k}${n ? "" : " zero"}">${AL_STATE_ICON[k]}<b>${fmtInt(n)}</b> ${esc(t(`al.c_${k}`))}</span>`;
  }).join("");
  $("#al-counts").innerHTML = chips;
  $("#al-meta").textContent = t("al.meta", { n: fmtInt(d.rules.length), every: d.every, t: hhmm(d.time) });
  const cfgErr = $("#al-cfg-error");
  cfgErr.hidden = !d.config_error; cfgErr.textContent = d.config_error ? t("al.config_error", { e: d.config_error }) : "";
  const unconfigured = (d.channels || []).filter((ch) => !ch.configured);
  const chan = $("#al-channels");
  chan.innerHTML = (d.channels || []).map((ch) => `<span class="al-chan${ch.configured ? "" : " off"}" data-tip="${esc(ch.env.length ? ch.env.join(", ") : t("al.chan_noenv"))}">${esc(ch.name)} <small>${esc(ch.type)}</small>${ch.configured ? "" : ` · ${esc(t("al.chan_missing"))}`}</span>`).join("");
  chan.hidden = !(d.channels || []).length;
  void unconfigured;

  const rules = [...d.rules].sort((a, b) => (AL_STATE_ORDER[a.state] - AL_STATE_ORDER[b.state]) || sevRank(b.severity) - sevRank(a.severity) || a.title.localeCompare(b.title));
  $("#al-rules").innerHTML = rules.length ? rules.map(ruleHtml).join("") : `<p class="dim">${t("al.norules")}</p>`;
  renderSilences();
}
const sevRank = (s) => ({ critical: 3, warning: 2, info: 1 })[s] || 0;

function ruleHtml(r) {
  const open = al.open.has(r.id);
  const sev = alKindOf(r.severity);
  const firing = r.instances.filter((i) => i.phase === "firing").length;
  const pending = r.instances.filter((i) => i.phase === "pending").length;
  const meta = [
    r.kind === "logs" ? t("al.kind_logs", { w: alDur(r.window), n: fmtInt(r.threshold) }) : t("al.kind_metric"),
    r.for ? t("al.for", { d: alDur(r.for) }) : t("al.for_now"),
    r.last_change ? t("al.changed", { t: alAgo(r.last_change) }) : "",
    firing ? tn("al.n_firing", firing) : pending ? tn("al.n_pending", pending) : "",
  ].filter(Boolean).join(" · ");
  const test = al.tests.get(r.id);
  const testHtml = test ? `<div class="al-test">${test.error ? `<span class="werror">${esc(test.error)}</span>` : test.matches.length
    ? `<p class="hint">${t("al.test_matches", { n: fmtInt(test.matches.length), ms: test.elapsedMs })}</p>` + test.matches.slice(0, 20).map((m) => `<div class="al-inst"><div class="al-inst-l">${alLabels(m.labels) || `<span class="dim">—</span>`}</div><div class="al-inst-s">${esc(m.summary)}</div><b class="al-val">${esc(fmtNum(m.value, 2))}</b></div>`).join("")
    : `<p class="hint">${t("al.test_none", { ms: test.elapsedMs })}</p>`}</div>` : "";
  const body = !open ? "" : `<div class="al-body">
      ${r.error ? `<p class="werror">${esc(t("al.eval_error", { e: r.error }))}</p>` : ""}
      <pre class="al-expr"><code>${esc(r.kind === "logs" ? r.filters.join("  ") : r.expr)}</code></pre>
      ${r.runbook ? `<p class="al-runbook">${esc(r.runbook)}</p>` : ""}
      ${r.instances.length ? `<div class="al-insts">${r.instances.map((i) => `<div class="al-inst ${i.phase}${i.silenced ? " silenced" : ""}">
          <span class="al-pill ${i.silenced ? "silenced" : i.phase}">${AL_STATE_ICON[i.silenced ? "silenced" : i.phase]}${esc(alStateName(i.silenced ? "silenced" : i.phase))}</span>
          <div class="al-inst-l">${alLabels(i.labels) || `<span class="dim">—</span>`}</div>
          <div class="al-inst-s">${esc(i.summary)}</div>
          <span class="al-since" data-tip="${esc(fmtDate(i.since * 1000))}">${esc(alAgo(i.since))}</span>
          <b class="al-val">${esc(fmtNum(i.value, 2))}</b>
          <button class="outlined small" data-act="silence" data-rule="${esc(r.id)}" data-labels="${esc(JSON.stringify(i.labels))}" ${i.silenced ? "disabled" : ""}>${t("al.silence")}</button>
        </div>`).join("")}</div>` : `<p class="hint">${r.state === "ok" ? t("al.no_instances") : ""}</p>`}
      <div class="al-actions">
        <button class="outlined small" data-act="test" data-rule="${esc(r.id)}">${t("al.test")}</button>
        <button class="outlined small" data-act="silence" data-rule="${esc(r.id)}" data-labels="{}">${t("al.silence_rule")}</button>
        <span class="hint">${t("al.notify", { c: (r.notify || []).join(", ") })}</span>
      </div>
      ${testHtml}
    </div>`;
  return `<article class="al-rule sev-${sev} st-${r.state}" data-rule="${esc(r.id)}">
    <button class="al-row" data-act="toggle" data-rule="${esc(r.id)}" aria-expanded="${open}">
      <span class="al-pill ${r.state}">${AL_STATE_ICON[r.state]}${esc(alStateName(r.state))}</span>
      <span class="al-title"><span class="al-sev ${sev}" data-tip="${esc(alSevName(sev))}" aria-label="${esc(alSevName(sev))}">${AL_SEV_ICON[sev]}</span>${esc(r.title)}</span>
      <span class="al-meta">${esc(meta)}</span>
      <svg class="al-chev" viewBox="0 0 24 24" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg>
    </button>${body}</article>`;
}

function renderSilences() {
  const d = al.data; if (!d) return;
  const list = d.silences || [];
  $("#al-silences").innerHTML = list.length ? list.map((s) => `<div class="al-sil">
      ${AL_STATE_ICON.silenced}
      <span class="al-sil-what">${s.rule ? `<b>${esc(d.rules.find((r) => r.id === s.rule)?.title || s.rule)}</b>` : `<b>${t("al.all_rules")}</b>`} ${alLabels(s.labels)}</span>
      <span class="hint">${esc(t("al.until", { t: fmtDate(s.until * 1000) }))}${s.comment ? ` · ${esc(s.comment)}` : ""}</span>
      <button class="icon-btn" data-act="unsilence" data-id="${esc(s.id)}" aria-label="${esc(t("al.unsilence"))}" data-tip="${esc(t("al.unsilence"))}"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18"/></svg></button>
    </div>`).join("") : `<p class="dim">${t("al.nosilences")}</p>`;
}

function renderHistory() {
  const h = al.history; if (!h) return;
  const tr = resolveTime();
  $("#al-history-range").textContent = t("al.history_range", { n: fmtInt(h.length), r: time.preset ? presetShort(time.preset).trim() : `${fmtTime(tr.start, true)} – ${fmtTime(tr.end, true)}` });
  const withDate = tr.end - tr.start > 86400;
  $("#al-history").innerHTML = h.length ? h.map((x) => `<div class="al-hist ${x.to}">
      <time>${esc(fmtDate(x.ts * 1000, withDate ? { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" } : { hour: "2-digit", minute: "2-digit", second: "2-digit" }))}</time>
      <span class="al-pill ${x.to === "firing" ? "firing" : "ok"}">${AL_STATE_ICON[x.to === "firing" ? "firing" : "ok"]}${esc(x.to === "firing" ? alStateName("firing") : t("al.resolved"))}</span>
      <span class="al-hist-t"><span class="al-sev ${alKindOf(x.severity)}" aria-hidden="true">${AL_SEV_ICON[alKindOf(x.severity)]}</span>${esc(x.title)} ${alLabels(x.labels)}</span>
      <span class="al-hist-s">${esc(x.to === "firing" ? x.summary : "")}</span>
    </div>`).join("") : `<p class="dim">${t("al.nohistory")}</p>`;
}

// ── Actions ────────────────────────────────────────────────────────────────
async function alTest(id) {
  al.tests.set(id, { matches: [], elapsedMs: 0, loading: true });
  al.open.add(id);
  renderAlerts();
  try {
    const j = await alApi("POST", `/api/v1/alerts/${encodeURIComponent(id)}/test?lang=${LANG}`);
    al.tests.set(id, j);
  } catch (e) { al.tests.set(id, { error: e.message, matches: [] }); }
  renderAlerts();
}

function alOpenSilence(rule, labels) {
  al.silence = { rule, labels };
  const r = al.data?.rules.find((x) => x.id === rule);
  $("#al-sil-what").innerHTML = `<b>${esc(r?.title || rule)}</b> ${alLabels(labels) || `<span class="dim">${t("al.sil_all_instances")}</span>`}`;
  $("#al-sil-comment").value = "";
  $$("#al-sil-dur button").forEach((b) => b.setAttribute("aria-checked", String(b.dataset.d === "1h")));
  $("#al-sil-custom").hidden = true;
  $("#al-sil-custom").value = toLocalInput(Math.floor(Date.now() / 1000) + 86400);
  $("#al-silence").hidden = false;
  setTimeout(() => $("#al-sil-comment").focus(), 0);
}
function alCloseSilence() { $("#al-silence").hidden = true; al.silence = null; }

async function alSaveSilence() {
  const s = al.silence; if (!s) return;
  const chosen = $$("#al-sil-dur button").find((b) => b.getAttribute("aria-checked") === "true")?.dataset.d || "1h";
  const until = chosen === "custom" ? new Date($("#al-sil-custom").value).toISOString() : chosen;
  const status = $("#al-status");
  try {
    await alApi("POST", "/api/v1/alerts/silences", { rule: s.rule, labels: s.labels, until, comment: $("#al-sil-comment").value.trim() });
    alCloseSilence();
    await alertsRefresh();
  } catch (e) { status.className = "status error"; status.textContent = e.message; }
}

function alInit() {
  if (al.inited) return;
  al.inited = true;
  $("#al-refresh").onclick = () => alertsRefresh();
  $("#view-alerts").addEventListener("click", async (e) => {
    const b = e.target.closest("[data-act]"); if (!b) return;
    const act = b.dataset.act;
    if (act === "toggle") { const id = b.dataset.rule; if (al.open.has(id)) al.open.delete(id); else al.open.add(id); renderAlerts(); }
    else if (act === "test") alTest(b.dataset.rule);
    else if (act === "silence") alOpenSilence(b.dataset.rule, JSON.parse(b.dataset.labels || "{}"));
    else if (act === "unsilence") {
      try { await alApi("DELETE", `/api/v1/alerts/silences/${encodeURIComponent(b.dataset.id)}`); await alertsRefresh(); }
      catch (err) { const st = $("#al-status"); st.className = "status error"; st.textContent = err.message; }
    }
  });
  $("#al-sil-dur").onclick = (e) => {
    const b = e.target.closest("button"); if (!b) return;
    $$("#al-sil-dur button").forEach((x) => x.setAttribute("aria-checked", String(x === b)));
    $("#al-sil-custom").hidden = b.dataset.d !== "custom";
  };
  $("#al-sil-cancel").onclick = alCloseSilence;
  $("#al-sil-save").onclick = alSaveSilence;
  $("#al-silence").addEventListener("click", (e) => { if (e.target === $("#al-silence")) alCloseSilence(); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && !$("#al-silence").hidden) alCloseSilence(); });
}
