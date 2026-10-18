// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Value formatting with Grafana unit names (the ones dashboards use).
// Numbers go through fmtNum (i18n.js): "4.56 GiB" in English, "4,56 GiB" in Spanish.
"use strict";

const SI = [[1e18, "E"], [1e15, "P"], [1e12, "T"], [1e9, "G"], [1e6, "M"], [1e3, "k"]];
const IEC = ["", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei"];
const DEC_PREFIX = ["", "k", "M", "G", "T", "P", "E"];

/** Number with fixed `decimals` or, without them, a sensible precision; the
 *  decimal separator follows the UI language (fmtNum, from i18n.js). */
function num(v, decimals) {
  if (decimals != null) return fmtNum(v, decimals);
  const a = Math.abs(v);
  if (a === 0) return "0";
  if (a >= 100) return fmtNum(v, 0);
  if (a >= 10) return fmtNum(v, undefined, 1);
  if (a >= 1) return fmtNum(v, undefined, 2);
  if (a >= 0.001) return fmtNum(v, undefined, 3);
  return fmtSig(v, 2);
}
/** Scales by powers of `base`, starting at prefix index `start`. */
function scaled(v, base, prefixes, suffix, decimals, start = 0) {
  let i = start, x = v;
  while (Math.abs(x) >= base && i < prefixes.length - 1) { x /= base; i++; }
  while (x !== 0 && Math.abs(x) < 1 && i > 0) { x *= base; i--; }
  return `${num(x, decimals)} ${prefixes[i]}${suffix}`;
}
function siShort(v, decimals, suffix = "") {
  const a = Math.abs(v);
  for (const [f, p] of SI) if (a >= f) return `${num(v / f, decimals)}${suffix ? " " + p + suffix : " " + p}`.trimEnd();
  return `${num(v, decimals)}${suffix ? " " + suffix : ""}`;
}
// Durations: the value comes in unit `from` (expressed in seconds)
const DUR_STEPS = [[86400 * 365, t("u.year"), t("u.years")], [86400 * 7, t("u.wk"), t("u.wk")], [86400, t("u.day"), t("u.days")], [3600, "h", "h"], [60, "min", "min"], [1, "s", "s"], [1e-3, "ms", "ms"], [1e-6, "µs", "µs"], [1e-9, "ns", "ns"]];
function duration(v, from, decimals) {
  const s = v * from, a = Math.abs(s);
  if (a === 0) return "0 s";
  for (const [f, one, many] of DUR_STEPS) if (a >= f) { const x = s / f; return `${num(x, decimals)} ${Math.abs(x) === 1 ? one : many}`; }
  return `${num(s / 1e-9, decimals)} ns`;
}
function dtdurations(v) {
  let s = Math.round(Math.abs(v));
  const parts = [];
  for (const [f, one, many] of [[86400, "u.day", "u.days"], [3600, "u.hour", "u.hours"], [60, "u.minute", "u.minutes"], [1, "u.second", "u.seconds"]]) {
    const n = Math.floor(s / f); if (n) { parts.push(`${fmtInt(n)} ${t(n === 1 ? one : many)}`); s -= n * f; }
    if (parts.length === 2) break;
  }
  return (v < 0 ? "-" : "") + (parts.join(", ") || `0 ${t("u.seconds")}`);
}
function fromNow(ms) {
  const d = (Date.now() - ms) / 1000, a = Math.abs(d);
  const txt = a < 60 ? t("u.few_seconds") : a < 3600 ? `${Math.round(a / 60)} min` : a < 86400 ? `${Math.round(a / 3600)} h` : `${fmtInt(Math.round(a / 86400))} ${t("u.days")}`;
  return t(d >= 0 ? "u.ago" : "u.in", { t: txt });
}

/** Grafana unit → formatter. */
const UNITS = {
  none: (v, d) => num(v, d),
  short: (v, d) => siShort(v, d),
  percent: (v, d) => `${num(v, d)}%`,
  percentunit: (v, d) => `${num(v * 100, d)}%`,
  bytes: (v, d) => scaled(v, 1024, IEC, "B", d),
  decbytes: (v, d) => scaled(v, 1000, DEC_PREFIX, "B", d),
  kbytes: (v, d) => scaled(v, 1024, IEC, "B", d, 1),
  deckbytes: (v, d) => scaled(v, 1000, DEC_PREFIX, "B", d, 1),
  mbytes: (v, d) => scaled(v, 1024, IEC, "B", d, 2),
  decmbytes: (v, d) => scaled(v, 1000, DEC_PREFIX, "B", d, 2),
  gbytes: (v, d) => scaled(v, 1024, IEC, "B", d, 3),
  decgbytes: (v, d) => scaled(v, 1000, DEC_PREFIX, "B", d, 3),
  bits: (v, d) => scaled(v, 1024, IEC, "b", d),
  decbits: (v, d) => scaled(v, 1000, DEC_PREFIX, "b", d),
  bps: (v, d) => scaled(v, 1000, DEC_PREFIX, "b/s", d),
  Bps: (v, d) => scaled(v, 1000, DEC_PREFIX, "B/s", d),
  binBps: (v, d) => scaled(v, 1024, IEC, "B/s", d),
  KBs: (v, d) => scaled(v, 1000, DEC_PREFIX, "B/s", d, 1),
  pps: (v, d) => siShort(v, d, "p/s"),
  ops: (v, d) => siShort(v, d, "ops/s"),
  reqps: (v, d) => siShort(v, d, "req/s"),
  rps: (v, d) => siShort(v, d, "rd/s"),
  wps: (v, d) => siShort(v, d, "wr/s"),
  iops: (v, d) => siShort(v, d, "io/s"),
  eps: (v, d) => siShort(v, d, "evt/s"),
  opm: (v, d) => siShort(v, d, "ops/min"),
  s: (v, d) => duration(v, 1, d),
  ms: (v, d) => duration(v, 1e-3, d),
  "µs": (v, d) => duration(v, 1e-6, d),
  us: (v, d) => duration(v, 1e-6, d),
  ns: (v, d) => duration(v, 1e-9, d),
  m: (v, d) => duration(v, 60, d),
  h: (v, d) => duration(v, 3600, d),
  d: (v, d) => duration(v, 86400, d),
  dtdurations: (v) => dtdurations(v),
  dtdurationms: (v) => dtdurations(v / 1000),
  volt: (v, d) => siShort(v, d, "V"),
  amp: (v, d) => siShort(v, d, "A"),
  watt: (v, d) => siShort(v, d, "W"),
  kwatt: (v, d) => `${num(v, d)} kW`,
  watth: (v, d) => siShort(v, d, "Wh"),
  kwatth: (v, d) => `${num(v, d)} kWh`,
  voltamp: (v, d) => siShort(v, d, "VA"),
  hertz: (v, d) => siShort(v, d, "Hz"),
  rothz: (v, d) => siShort(v, d, "Hz"),
  rotrpm: (v, d) => `${num(v, d)} rpm`,
  celsius: (v, d) => `${num(v, d)} °C`,
  humidity: (v, d) => `${num(v, d)} %H`,
  bool: (v) => t(v ? "u.true" : "u.false"),
  bool_yes_no: (v) => t(v ? "u.yes" : "no"),
  bool_on_off: (v) => t(v ? "u.on" : "u.off"),
  dateTimeAsIso: (v) => { const d = new Date(v); d.setMinutes(d.getMinutes() - d.getTimezoneOffset()); return d.toISOString().slice(0, 19).replace("T", " "); },
  dateTimeAsLocal: (v) => fmtDate(v, { dateStyle: "short", timeStyle: "medium" }),
  dateTimeFromNow: (v) => fromNow(v),
};

/**
 * Formats a value with a Grafana unit. Unknown units are written after the
 * number (like Grafana's "suffix:").
 */
function fmtUnit(v, unit, decimals) {
  if (v == null || (typeof v === "number" && Number.isNaN(v))) return "—";
  if (typeof v === "string") return v;
  if (v === Infinity) return "+∞";
  if (v === -Infinity) return "−∞";
  if (unit === "string") return String(v);
  const f = UNITS[unit || "short"];
  if (f) return f(v, decimals);
  if (unit.startsWith("suffix:")) return `${num(v, decimals)} ${unit.slice(7)}`;
  if (unit.startsWith("prefix:")) return `${unit.slice(7)}${num(v, decimals)}`;
  return `${num(v, decimals)} ${unit}`;
}
/** Axis labels: few digits and the same unit. */
function axisUnit(unit, decimals) { return (v) => fmtUnit(v, unit, decimals); }

/** Readable text color on a background: dark ink on light backgrounds (luminance > .5). */
function inkOn(bg) {
  const m = /^#?([0-9a-f]{6})$/i.exec(String(bg).trim());
  if (!m) return "#fff";
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16) / 255).map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5 ? "#202124" : "#fff";
}
