// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Alerts: rules from alerts.yaml evaluated by the server every few seconds,
//! with state (pending → firing → resolved), silences and notifications
//! grouped per channel (ntfy, Telegram, webhook or the log).
//!
//! A rule is either an instant PromQL query (every series it returns is an
//! alert, identified by its labels) or a log filter (fires when the count in
//! the window reaches the threshold). Rules are evaluated ATALAYA_NOW_DELAY
//! before now, like the panels: the time for the collector's data to arrive.
//!
//! State lives in ATALAYA_DATA/alerts/: state.json (instances and silences) and
//! history.jsonl (one line per transition). Channel secrets come from
//! environment variables named in the YAML; they are never written here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::app::App;
use crate::overview::Text;
use atalaya::ch::Param;
use atalaya::promql::Output;

/// How far behind "now" evaluation runs (same delay as the panels)
/// Maximum number of history lines returned (the most recent ones)
const MAX_HISTORY: usize = 5000;

pub type Labels = BTreeMap<String, String>;

// ── Configuration ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub channels: BTreeMap<String, Channel>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Deserialize, Clone)]
pub struct Defaults {
    #[serde(default = "d_every")]
    pub every: String,
    #[serde(default = "d_for", rename = "for")]
    pub for_: String,
    #[serde(default = "d_group_wait")]
    pub group_wait: String,
    #[serde(default = "d_notify")]
    pub notify: Vec<String>,
}
fn d_every() -> String {
    "30s".into()
}
fn d_for() -> String {
    "5m".into()
}
fn d_group_wait() -> String {
    "30s".into()
}
fn d_notify() -> Vec<String> {
    vec!["default".into()]
}
impl Default for Defaults {
    fn default() -> Self {
        Self {
            every: d_every(),
            for_: d_for(),
            group_wait: d_group_wait(),
            notify: d_notify(),
        }
    }
}

/// A notification channel. Secrets are only referenced by environment variable name.
#[derive(Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Channel {
    /// Only writes to the server log (testing, or while no real channel exists)
    Log,
    /// ntfy: full topic URL (https://ntfy.sh/my-topic) and optional token
    Ntfy {
        url_env: String,
        #[serde(default)]
        token_env: Option<String>,
    },
    Telegram {
        bot_token_env: String,
        chat_id_env: String,
    },
    /// POSTs a JSON body with the alerts to the URL
    Webhook {
        url_env: String,
    },
}

impl Channel {
    pub fn kind(&self) -> &'static str {
        match self {
            Channel::Log => "log",
            Channel::Ntfy { .. } => "ntfy",
            Channel::Telegram { .. } => "telegram",
            Channel::Webhook { .. } => "webhook",
        }
    }
    /// Environment variables it needs (names are shown, never their values)
    pub fn env_vars(&self) -> Vec<&str> {
        match self {
            Channel::Log => vec![],
            Channel::Ntfy { url_env, token_env } => {
                let mut v = vec![url_env.as_str()];
                if let Some(t) = token_env {
                    v.push(t.as_str());
                }
                v
            }
            Channel::Telegram { bot_token_env, chat_id_env } => vec![bot_token_env.as_str(), chat_id_env.as_str()],
            Channel::Webhook { url_env } => vec![url_env.as_str()],
        }
    }
    /// Whether all of its variables are set in the environment
    pub fn configured(&self) -> bool {
        self.env_vars().iter().all(|v| std::env::var(v).map(|s| !s.is_empty()).unwrap_or(false))
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}
fn d_sev() -> Severity {
    Severity::Warning
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Metric,
    Logs,
}

#[derive(Deserialize, Clone)]
pub struct Rule {
    pub id: String,
    pub title: Text,
    #[serde(default = "d_sev")]
    pub severity: Severity,
    #[serde(default)]
    pub kind: Kind,
    /// PromQL (kind metric): every series in the result is an alert
    #[serde(default)]
    pub expr: String,
    /// Log filters (kind logs), same syntax as `f=` in /api/v1/logs
    #[serde(default)]
    pub filters: Vec<String>,
    /// Window for log filters (default 10m)
    #[serde(default)]
    pub window: Option<String>,
    /// Count at which a log rule fires
    #[serde(default)]
    pub threshold: Option<f64>,
    /// How long the condition must hold before firing; "0s" fires immediately
    #[serde(default, rename = "for")]
    pub for_: Option<String>,
    /// Labels that make up the alert identity (all of them if unset)
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Text with {{label}} and {{value}} placeholders; the value if unset
    #[serde(default = "default_summary")]
    pub summary: Text,
    #[serde(default)]
    pub runbook: Option<Text>,
    #[serde(default)]
    pub notify: Option<Vec<String>>,
}

fn default_summary() -> Text {
    Text::One("{{value}}".into())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Reads and validates alerts.yaml. A missing file yields an empty config.
pub fn load_config(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let cfg: Config = serde_saphyr::from_str(&text).with_context(|| format!("{}", path.display()))?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<()> {
    crate::query::parse_duration(&cfg.defaults.every).context("defaults.every")?;
    crate::query::parse_duration(&cfg.defaults.for_).context("defaults.for")?;
    crate::query::parse_duration(&cfg.defaults.group_wait).context("defaults.group_wait")?;
    for c in &cfg.defaults.notify {
        if !cfg.channels.contains_key(c) {
            bail!("defaults.notify: unknown channel '{c}'");
        }
    }
    let mut seen = std::collections::HashSet::new();
    for r in &cfg.rules {
        if !valid_id(&r.id) {
            bail!("rule with invalid id: '{}'", r.id);
        }
        if !seen.insert(&r.id) {
            bail!("duplicate rule: {}", r.id);
        }
        match r.kind {
            Kind::Metric if r.expr.trim().is_empty() => bail!("rule {}: missing expr", r.id),
            Kind::Logs if r.filters.is_empty() || r.threshold.is_none() => bail!("rule {}: a logs rule needs filters and threshold", r.id),
            _ => {}
        }
        if let Some(f) = &r.for_ {
            crate::query::parse_duration(f).with_context(|| format!("rule {}: for", r.id))?;
        }
        if let Some(w) = &r.window {
            crate::query::parse_duration(w).with_context(|| format!("rule {}: window", r.id))?;
        }
        for c in r.notify.iter().flatten() {
            if !cfg.channels.contains_key(c) {
                bail!("rule {}: unknown channel '{c}'", r.id);
            }
        }
    }
    Ok(())
}

impl Rule {
    pub fn for_secs(&self, d: &Defaults) -> i64 {
        crate::query::parse_duration(self.for_.as_deref().unwrap_or(&d.for_)).unwrap_or(300)
    }
    pub fn window_secs(&self) -> i64 {
        crate::query::parse_duration(self.window.as_deref().unwrap_or("10m")).unwrap_or(600)
    }
    pub fn channels<'a>(&'a self, d: &'a Defaults) -> &'a [String] {
        self.notify.as_deref().unwrap_or(&d.notify)
    }
    /// The summary with labels and value substituted
    pub fn render_summary(&self, lang: &str, labels: &Labels, value: f64) -> String {
        render(self.summary.get(lang), labels, value)
    }
}

/// Replaces {{value}} and {{label}} in a text
pub fn render(text: &str, labels: &Labels, value: f64) -> String {
    let mut out = text.replace("{{value}}", &fmt_value(value));
    for (k, v) in labels {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

pub fn fmt_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else if v.abs() >= 100.0 {
        format!("{v:.0}")
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

// ── State ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Pending,
    Firing,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Instance {
    pub rule: String,
    pub labels: Labels,
    pub phase: Phase,
    /// When it entered the current phase
    pub since: i64,
    pub value: f64,
    pub last_seen: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Silence {
    pub id: String,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub labels: Labels,
    pub until: i64,
    #[serde(default)]
    pub comment: String,
    pub created: i64,
}

impl Silence {
    pub fn matches(&self, rule: &str, labels: &Labels, now: i64) -> bool {
        self.until > now && self.rule.as_deref().is_none_or(|r| r == rule) && self.labels.iter().all(|(k, v)| labels.get(k) == Some(v))
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct RuleStatus {
    pub last_eval: i64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub last_change: Option<i64>,
    #[serde(default)]
    pub matches: usize,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    /// rule|labels key → instance
    #[serde(default)]
    pub instances: BTreeMap<String, Instance>,
    #[serde(default)]
    pub silences: Vec<Silence>,
    #[serde(default)]
    pub rules: BTreeMap<String, RuleStatus>,
}

/// A state transition: what goes to the history and to notifications
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Transition {
    pub ts: i64,
    pub rule: String,
    pub labels: Labels,
    pub from: String,
    pub to: String,
    pub value: f64,
}

/// Human-readable labels: a=1,b=2 (not usable as an identity: {a: "x,b=y"}
/// and {a: "x", b: "y"} render the same)
pub fn labels_text(labels: &Labels) -> String {
    labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}

/// Instance identity: rule plus labels as canonical JSON (sorted and escaped,
/// so it is unambiguous)
fn instance_key(rule: &str, labels: &Labels) -> String {
    format!("{rule}|{}", serde_json::to_string(labels).unwrap_or_default())
}

/// One step of a rule's state machine given its evaluation result. Returns the
/// transitions (pending→firing, firing→resolved).
pub fn step(instances: &mut BTreeMap<String, Instance>, rule: &str, matches: &[(Labels, f64)], now: i64, for_secs: i64) -> Vec<Transition> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (labels, value) in matches {
        let key = instance_key(rule, labels);
        seen.insert(key.clone());
        match instances.get_mut(&key) {
            None => {
                let phase = if for_secs <= 0 { Phase::Firing } else { Phase::Pending };
                instances.insert(
                    key,
                    Instance {
                        rule: rule.to_owned(),
                        labels: labels.clone(),
                        phase,
                        since: now,
                        value: *value,
                        last_seen: now,
                    },
                );
                if phase == Phase::Firing {
                    out.push(Transition {
                        ts: now,
                        rule: rule.to_owned(),
                        labels: labels.clone(),
                        from: "inactive".into(),
                        to: "firing".into(),
                        value: *value,
                    });
                }
            }
            Some(i) => {
                i.value = *value;
                i.last_seen = now;
                if i.phase == Phase::Pending && now - i.since >= for_secs {
                    i.phase = Phase::Firing;
                    i.since = now;
                    out.push(Transition {
                        ts: now,
                        rule: rule.to_owned(),
                        labels: labels.clone(),
                        from: "pending".into(),
                        to: "firing".into(),
                        value: *value,
                    });
                }
            }
        }
    }
    let gone: Vec<String> = instances
        .iter()
        .filter(|(k, i)| i.rule == rule && !seen.contains(*k))
        .map(|(k, _)| k.clone())
        .collect();
    for k in gone {
        if let Some(i) = instances.remove(&k)
            && i.phase == Phase::Firing
        {
            out.push(Transition {
                ts: now,
                rule: rule.to_owned(),
                labels: i.labels,
                from: "firing".into(),
                to: "resolved".into(),
                value: i.value,
            });
        }
    }
    out
}

// ── Service ────────────────────────────────────────────────────────────

pub struct Alerts {
    path: PathBuf,
    dir: PathBuf,
    cfg: Mutex<Arc<Config>>,
    cfg_error: Mutex<Option<String>>,
    state: Mutex<State>,
    /// Outbox (on disk as outbox.json): one pending notification per channel
    /// until the channel accepts it
    queue: Mutex<Vec<Pending>>,
    http: reqwest::Client,
    /// Last completed evaluation (for /readyz)
    last_tick: std::sync::atomic::AtomicI64,
}

/// A notification waiting to be delivered to a channel. At-least-once delivery:
/// if the process dies between sending and removal, it is resent on startup.
#[derive(Serialize, Deserialize, Clone)]
struct Pending {
    id: u64,
    /// When it was queued (counts towards group_wait)
    queued: i64,
    channel: String,
    tr: Transition,
    #[serde(default)]
    attempts: u32,
    /// Not before this time (growing backoff after a failure)
    #[serde(default)]
    next_try: i64,
}

/// Retries for a failing channel: 30 s, 90 s, 4.5 min… up to 1 h between attempts
const MAX_ATTEMPTS: u32 = 24;

/// History rotates to history.1.jsonl past this size (about 40,000 transitions)
const HISTORY_ROTATE_BYTES: u64 = 8 << 20;

fn backoff(attempts: u32) -> i64 {
    (30 * 3_i64.pow(attempts.min(6))).min(3600)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Alerts {
    pub fn new(path: PathBuf, data_dir: &Path) -> Self {
        let dir = data_dir.join("alerts");
        let mut state: State = std::fs::read_to_string(dir.join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).map_err(|e| tracing::warn!("alerts/state.json: {e}")).ok())
            .unwrap_or_default();
        // Recompute keys with the current format (older files used a=1,b=2) so
        // a firing alert keeps its identity across upgrades
        state.instances = std::mem::take(&mut state.instances)
            .into_values()
            .map(|i| (instance_key(&i.rule, &i.labels), i))
            .collect();
        let (cfg, cfg_error) = match load_config(&path) {
            Ok(c) => (c, None),
            Err(e) => {
                tracing::warn!("alerts: {e:#}");
                (Config::default(), Some(format!("{e:#}")))
            }
        };
        let outbox: Vec<Pending> = std::fs::read_to_string(dir.join("outbox.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).map_err(|e| tracing::warn!("alerts/outbox.json: {e}")).ok())
            .unwrap_or_default();
        Self {
            path,
            dir,
            cfg: Mutex::new(Arc::new(cfg)),
            cfg_error: Mutex::new(cfg_error),
            state: Mutex::new(state),
            queue: Mutex::new(outbox),
            http: reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default(),
            last_tick: std::sync::atomic::AtomicI64::new(0),
        }
    }

    pub async fn config(&self) -> Arc<Config> {
        self.cfg.lock().await.clone()
    }

    /// Rereads alerts.yaml; if invalid, keeps the previous config and records the error
    async fn reload(&self) {
        match load_config(&self.path) {
            Ok(c) => {
                *self.cfg.lock().await = Arc::new(c);
                *self.cfg_error.lock().await = None;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let mut cur = self.cfg_error.lock().await;
                if cur.as_deref() != Some(&msg) {
                    tracing::warn!("alerts: {msg}");
                }
                *cur = Some(msg);
            }
        }
    }

    fn persist(&self, state: &State) {
        if let Err(e) = (|| -> Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            let p = self.dir.join("state.json");
            let tmp = self.dir.join("state.json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
            std::fs::rename(&tmp, &p)?;
            Ok(())
        })() {
            tracing::warn!("alerts: saving state: {e:#}");
        }
    }

    fn persist_outbox(&self, q: &[Pending]) {
        if let Err(e) = (|| -> Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            let tmp = self.dir.join("outbox.json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(q)?)?;
            std::fs::rename(&tmp, self.dir.join("outbox.json"))?;
            Ok(())
        })() {
            tracing::warn!("alerts: saving outbox: {e:#}");
        }
    }

    fn append_history(&self, transitions: &[Transition]) {
        if transitions.is_empty() {
            return;
        }
        if let Err(e) = (|| -> Result<()> {
            use std::io::Write;
            std::fs::create_dir_all(&self.dir)?;
            // Rotate so the file does not grow unbounded (one previous file is kept)
            let path = self.dir.join("history.jsonl");
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > HISTORY_ROTATE_BYTES) {
                std::fs::rename(&path, self.dir.join("history.1.jsonl"))?;
            }
            let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&path)?;
            for t in transitions {
                writeln!(f, "{}", serde_json::to_string(t)?)?;
            }
            Ok(())
        })() {
            tracing::warn!("alerts: saving history: {e:#}");
        }
    }

    /// Stored transitions with ts in [start, end], from the current and the
    /// previously rotated history; the most recent MAX_HISTORY if there are more
    pub fn history(&self, start: i64, end: i64) -> Vec<Transition> {
        let mut out: Vec<Transition> = ["history.1.jsonl", "history.jsonl"]
            .iter()
            .filter_map(|f| std::fs::read_to_string(self.dir.join(f)).ok())
            .flat_map(|text| {
                text.lines()
                    .filter_map(|l| serde_json::from_str::<Transition>(l).ok())
                    .filter(|t| t.ts >= start && t.ts <= end)
                    .collect::<Vec<_>>()
            })
            .collect();
        let skip = out.len().saturating_sub(MAX_HISTORY);
        out.drain(..skip);
        out
    }

    /// Evaluates a rule: the alerts that match right now
    pub async fn evaluate(&self, app: &App, rule: &Rule) -> Result<Vec<(Labels, f64)>> {
        let at = now() - crate::app::now_delay();
        match rule.kind {
            Kind::Metric => {
                let r = app.promql.instant(&rule.expr, at).await?;
                let mut out = Vec::new();
                match r.output {
                    Output::Matrix(m) => {
                        for (labels, pts) in m {
                            let Some(&(_, v)) = pts.last() else { continue };
                            if v.is_nan() {
                                continue;
                            }
                            let mut l: Labels = labels.into_iter().filter(|(k, _)| k != "__name__").collect();
                            if let Some(keep) = &rule.labels {
                                l.retain(|k, _| keep.contains(k));
                            }
                            out.push((l, v));
                        }
                    }
                    Output::Scalar(s) => {
                        if let Some(&(_, v)) = s.last()
                            && !v.is_nan()
                        {
                            out.push((Labels::new(), v));
                        }
                    }
                    Output::Str(_) => bail!("the expression returns a string"),
                }
                // Two series with the same identity (after trimming labels): keep the larger
                out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)));
                out.dedup_by(|a, b| a.0 == b.0);
                Ok(out)
            }
            Kind::Logs => {
                let window = rule.window_secs();
                let mut params: Vec<Param> = vec![("start".into(), (at - window).to_string()), ("end".into(), at.to_string())];
                let mut conds =
                    vec!["Time >= fromUnixTimestamp64Milli({start:Int64} * 1000) AND Time < fromUnixTimestamp64Milli({end:Int64} * 1000)".to_owned()];
                conds.extend(crate::logsearch::filters_sql(&rule.filters, &mut params)?);
                let sql = format!("SELECT count() AS n FROM {}.logs WHERE {}", app.db, conds.join(" AND "));
                let rows = app.ch.rows(&sql, &params).await?;
                let n = rows.first().and_then(|r| r["n"].as_f64()).unwrap_or(0.0);
                Ok(if n >= rule.threshold.unwrap_or(f64::INFINITY) {
                    vec![(Labels::new(), n)]
                } else {
                    vec![]
                })
            }
        }
    }

    /// One cycle: reload config, evaluate, update state and queue notifications
    pub async fn tick(&self, app: &App) {
        self.reload().await;
        let cfg = self.config().await;
        let t = now();
        let mut transitions = Vec::new();
        let mut state = self.state.lock().await.clone();
        state.silences.retain(|s| s.until > t);
        let known: std::collections::HashSet<&str> = cfg.rules.iter().map(|r| r.id.as_str()).collect();
        // Drop instances and status of rules that no longer exist
        state.instances.retain(|_, i| known.contains(i.rule.as_str()));
        state.rules.retain(|k, _| known.contains(k.as_str()));
        for rule in &cfg.rules {
            let status = state.rules.entry(rule.id.clone()).or_default();
            status.last_eval = t;
            match self.evaluate(app, rule).await {
                Ok(matches) => {
                    status.error = None;
                    status.matches = matches.len();
                    let tr = step(&mut state.instances, &rule.id, &matches, t, rule.for_secs(&cfg.defaults));
                    if !tr.is_empty() {
                        status.last_change = Some(t);
                    }
                    transitions.extend(tr);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if status.error.as_deref() != Some(&msg) {
                        tracing::warn!("alert {}: {msg}", rule.id);
                    }
                    status.error = Some(msg);
                }
            }
        }
        self.append_history(&transitions);
        self.finish_cycle(&cfg, state, transitions, t).await;
        self.last_tick.store(t, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn last_tick(&self) -> Option<i64> {
        Some(self.last_tick.load(std::sync::atomic::Ordering::Relaxed)).filter(|t| *t > 0)
    }

    /// Ends a cycle: stores the evaluated state and queues notifications.
    async fn finish_cycle(&self, cfg: &Config, mut state: State, transitions: Vec<Transition>, t: i64) {
        // Evaluation ran on a copy without holding the state lock. Silences may
        // have changed meanwhile through the API: the current ones win over the
        // copy's, both for persisting and for deciding what to notify.
        let notify: Vec<Transition> = {
            let mut cur = self.state.lock().await;
            state.silences = std::mem::take(&mut cur.silences);
            state.silences.retain(|s| s.until > t);
            self.persist(&state);
            let notify = transitions
                .into_iter()
                .filter(|tr| !state.silences.iter().any(|s| s.matches(&tr.rule, &tr.labels, t)))
                .collect();
            *cur = state;
            notify
        };
        if !notify.is_empty() {
            let mut q = self.queue.lock().await;
            let mut id = q.iter().map(|p| p.id).max().unwrap_or(0);
            for tr in notify {
                let Some(rule) = cfg.rules.iter().find(|r| r.id == tr.rule) else { continue };
                for c in rule.channels(&cfg.defaults) {
                    id += 1;
                    q.push(Pending {
                        id,
                        queued: t,
                        channel: c.clone(),
                        tr: tr.clone(),
                        attempts: 0,
                        next_try: 0,
                    });
                }
            }
            self.persist_outbox(&q);
        }
    }

    /// Delivers pending notifications, separately from evaluation: per channel,
    /// once the oldest has waited group_wait. A failure leaves them in the
    /// outbox with growing backoff; they only leave once delivered (or after
    /// MAX_ATTEMPTS, with a log entry).
    pub async fn deliver(&self) {
        let cfg = self.config().await;
        let wait = crate::query::parse_duration(&cfg.defaults.group_wait).unwrap_or(30);
        let t = now();
        let ready: BTreeMap<String, Vec<Pending>> = {
            let q = self.queue.lock().await;
            let mut by: BTreeMap<String, Vec<Pending>> = BTreeMap::new();
            for p in q.iter().filter(|p| p.next_try <= t) {
                by.entry(p.channel.clone()).or_default().push(p.clone());
            }
            by.retain(|_, ps| ps.iter().map(|p| p.queued).min().unwrap_or(t) + wait <= t);
            by
        };
        for (name, ps) in ready {
            let ids: Vec<u64> = ps.iter().map(|p| p.id).collect();
            let result = match cfg.channels.get(&name) {
                None => Err(None),
                Some(ch) => {
                    let trs: Vec<&Transition> = ps.iter().map(|p| &p.tr).collect();
                    let msg = Message::build(&cfg, &trs, "en");
                    send(&self.http, ch, &msg).await.map_err(|e| Some((ch.kind(), e)))
                }
            };
            let mut q = self.queue.lock().await;
            match result {
                Ok(()) => q.retain(|p| !ids.contains(&p.id)),
                Err(None) => {
                    tracing::warn!("alerts: channel {name} no longer exists; dropping {} notifications", ids.len());
                    q.retain(|p| !ids.contains(&p.id));
                }
                Err(Some((kind, e))) => {
                    for p in q.iter_mut().filter(|p| ids.contains(&p.id)) {
                        p.attempts += 1;
                        p.next_try = t + backoff(p.attempts);
                    }
                    let attempts = q.iter().filter(|p| ids.contains(&p.id)).map(|p| p.attempts).max().unwrap_or(0);
                    tracing::warn!("alerts: channel {name} ({kind}), attempt {attempts}: {e:#}");
                    let before = q.len();
                    q.retain(|p| p.attempts < MAX_ATTEMPTS);
                    if q.len() != before {
                        tracing::error!(
                            "alerts: channel {name}: dropped {} notifications after {MAX_ATTEMPTS} attempts",
                            before - q.len()
                        );
                    }
                }
            }
            self.persist_outbox(&q);
        }
    }

    /// Notifications waiting for delivery (for the alerts view)
    pub async fn pending(&self) -> usize {
        self.queue.lock().await.len()
    }

    // ── API ────────────────────────────────────────────────────────────────

    pub async fn snapshot(&self, lang: &str) -> Value {
        let cfg = self.config().await;
        let state = self.state.lock().await.clone();
        let t = now();
        // firing/pending/ok/error count rules; silenced counts silenced firing instances
        let mut counts = json!({ "firing": 0, "pending": 0, "silenced": 0, "ok": 0, "error": 0 });
        let bump = |counts: &mut Value, k: &str, n: i64| counts[k] = json!(counts[k].as_i64().unwrap_or(0) + n);
        let rules: Vec<Value> = cfg
            .rules
            .iter()
            .map(|r| {
                let st = state.rules.get(&r.id).cloned().unwrap_or_default();
                let instances: Vec<Value> = state
                    .instances
                    .values()
                    .filter(|i| i.rule == r.id)
                    .map(|i| {
                        let silenced = state.silences.iter().any(|s| s.matches(&r.id, &i.labels, t));
                        json!({ "labels": i.labels, "phase": i.phase, "since": i.since, "value": i.value, "silenced": silenced,
                                "summary": r.render_summary(lang, &i.labels, i.value) })
                    })
                    .collect();
                let firing = instances.iter().filter(|i| i["phase"] == "firing").count();
                let unsilenced = instances.iter().filter(|i| i["phase"] == "firing" && i["silenced"] == false).count();
                let pending = instances.iter().filter(|i| i["phase"] == "pending").count();
                let rule_state = if st.error.is_some() {
                    "error"
                } else if firing > 0 && unsilenced == 0 {
                    "silenced"
                } else if firing > 0 {
                    "firing"
                } else if pending > 0 {
                    "pending"
                } else {
                    "ok"
                };
                bump(&mut counts, if rule_state == "silenced" { "ok" } else { rule_state }, 1);
                bump(&mut counts, "silenced", (firing - unsilenced) as i64);
                json!({
                    "id": r.id, "title": r.title.get(lang), "severity": r.severity, "kind": r.kind,
                    "expr": if r.kind == Kind::Metric { Some(&r.expr) } else { None }, "filters": r.filters, "window": r.window_secs(), "threshold": r.threshold,
                    "for": r.for_secs(&cfg.defaults), "notify": r.channels(&cfg.defaults), "runbook": r.runbook.as_ref().map(|x| x.get(lang)),
                    "state": rule_state, "error": st.error, "last_eval": st.last_eval, "last_change": st.last_change, "matches": st.matches,
                    "instances": instances,
                })
            })
            .collect();
        let channels: Vec<Value> = cfg
            .channels
            .iter()
            .map(|(n, c)| json!({ "name": n, "type": c.kind(), "env": c.env_vars(), "configured": c.configured() }))
            .collect();
        json!({
            "time": t, "config_error": *self.cfg_error.lock().await, "every": cfg.defaults.every, "counts": counts,
            "rules": rules, "channels": channels, "silences": state.silences, "outbox": self.pending().await,
        })
    }

    /// Transitions in the range, with the rule's texts
    pub async fn history_json(&self, start: i64, end: i64, lang: &str) -> Vec<Value> {
        let cfg = self.config().await;
        let mut v: Vec<Value> = self
            .history(start, end)
            .into_iter()
            .map(|tr| {
                let rule = cfg.rules.iter().find(|r| r.id == tr.rule);
                json!({
                    "ts": tr.ts, "rule": tr.rule, "labels": tr.labels, "from": tr.from, "to": tr.to, "value": tr.value,
                    "title": rule.map(|r| r.title.get(lang).to_owned()).unwrap_or_else(|| tr.rule.clone()),
                    "severity": rule.map(|r| r.severity),
                    "summary": rule.map(|r| r.render_summary(lang, &tr.labels, tr.value)).unwrap_or_default(),
                })
            })
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x["ts"].as_i64().unwrap_or(0)));
        v
    }

    /// Events for the timeline
    pub async fn events(&self, start: i64, end: i64, lang: &str) -> Vec<Value> {
        self.history_json(start, end, lang)
            .await
            .into_iter()
            .filter(|h| h["to"] == "firing" || h["to"] == "resolved")
            .map(|h| {
                let labels = h["labels"].as_object().cloned().unwrap_or_default();
                let who: Vec<String> = ["instance", "job", "mountpoint", "task"]
                    .iter()
                    .filter_map(|k| labels.get(*k).and_then(|v| v.as_str()).map(str::to_owned))
                    .collect();
                let firing = h["to"] == "firing";
                json!({
                    "time": h["ts"], "type": if firing { "alert_firing" } else { "alert_resolved" }, "severity": h["severity"],
                    "service": labels.get("job").and_then(|v| v.as_str()).unwrap_or(""), "host": labels.get("instance").and_then(|v| v.as_str()).unwrap_or(""),
                    "rule": h["rule"],
                    "title": format!("{}{}{}", h["title"].as_str().unwrap_or(""), if who.is_empty() { "" } else { " · " }, who.join(" ")),
                    "detail": if firing { h["summary"].as_str().unwrap_or("").to_owned() } else { tr_resolved(lang).to_owned() },
                })
            })
            .collect()
    }

    pub async fn silences(&self) -> Vec<Silence> {
        let t = now();
        self.state.lock().await.silences.iter().filter(|s| s.until > t).cloned().collect()
    }

    pub async fn add_silence(&self, rule: Option<String>, labels: Labels, until: i64, comment: String) -> Result<Silence> {
        if let Some(r) = &rule
            && !valid_id(r)
        {
            bail!("invalid rule");
        }
        if until <= now() {
            bail!("the silence would already be expired");
        }
        let s = Silence {
            id: format!("s-{:x}{:x}", now(), rand_u16()),
            rule,
            labels,
            until,
            comment: comment.chars().take(200).collect(),
            created: now(),
        };
        let mut state = self.state.lock().await;
        state.silences.push(s.clone());
        self.persist(&state);
        Ok(s)
    }

    pub async fn delete_silence(&self, id: &str) -> bool {
        let mut state = self.state.lock().await;
        let before = state.silences.len();
        state.silences.retain(|s| s.id != id);
        let removed = state.silences.len() != before;
        if removed {
            self.persist(&state);
        }
        removed
    }
}

fn rand_u16() -> u16 {
    // Enough to keep two silences created in the same second from colliding
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
        % 65536) as u16
}

fn tr_resolved(lang: &str) -> &'static str {
    if lang == "es" { "Resuelta" } else { "Resolved" }
}

/// Evaluation loop: runs every `defaults.every`
pub async fn run(app: Arc<App>) {
    // Delivery runs in its own loop so a slow channel does not delay evaluation
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            a.alerts.deliver().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    loop {
        app.alerts.tick(&app).await;
        let every = crate::query::parse_duration(&app.alerts.config().await.defaults.every)
            .unwrap_or(30)
            .clamp(10, 3600);
        tokio::time::sleep(Duration::from_secs(every as u64)).await;
    }
}

// ── Notifications ─────────────────────────────────────────────────────────────────

pub struct Message {
    pub title: String,
    pub text: String,
    /// Worst severity among firing alerts (None if all are resolved)
    pub worst: Option<Severity>,
    pub alerts: Vec<Value>,
}

impl Message {
    pub fn build(cfg: &Config, trs: &[&Transition], lang: &str) -> Message {
        let mut firing = Vec::new();
        let mut resolved = Vec::new();
        let mut alerts = Vec::new();
        for tr in trs {
            let Some(rule) = cfg.rules.iter().find(|r| r.id == tr.rule) else { continue };
            let who = labels_text(&tr.labels);
            let line = format!("{}{}", rule.title.get(lang), if who.is_empty() { String::new() } else { format!(" ({who})") });
            alerts.push(
                json!({ "rule": rule.id, "title": rule.title.get(lang), "severity": rule.severity, "state": tr.to, "labels": tr.labels,
                                "value": tr.value, "summary": rule.render_summary(lang, &tr.labels, tr.value), "ts": tr.ts }),
            );
            if tr.to == "firing" {
                firing.push((
                    rule.severity,
                    format!("{} {line}\n   {}", sev_icon(rule.severity), rule.render_summary(lang, &tr.labels, tr.value)),
                ));
            } else {
                resolved.push(format!("✅ {line}"));
            }
        }
        firing.sort_by_key(|f| std::cmp::Reverse(f.0));
        let worst = firing.first().map(|f| f.0);
        let n_f = firing.len();
        let n_r = resolved.len();
        let title = match (n_f, n_r) {
            (0, _) => format!("atalaya · {n_r} resolved"),
            (_, 0) => format!("atalaya · {n_f} {} firing", worst.map(sev_name).unwrap_or("")),
            _ => format!("atalaya · {n_f} firing, {n_r} resolved"),
        };
        let mut text = firing.into_iter().map(|f| f.1).chain(resolved).collect::<Vec<_>>().join("\n");
        if let Ok(base) = std::env::var("ATALAYA_PUBLIC_URL")
            && !base.is_empty()
        {
            text.push_str(&format!("\n{}/#/alerts", base.trim_end_matches('/')));
        }
        Message { title, text, worst, alerts }
    }
}

fn sev_icon(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "🔴",
        Severity::Warning => "🟠",
        Severity::Info => "🔵",
    }
}
fn sev_name(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn env_required(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => bail!("missing environment variable {name}"),
    }
}

async fn send(http: &reqwest::Client, ch: &Channel, msg: &Message) -> Result<()> {
    match ch {
        Channel::Log => {
            tracing::warn!(target: "alerts", "{}\n{}", msg.title, msg.text);
            Ok(())
        }
        Channel::Ntfy { url_env, token_env } => {
            let url = env_required(url_env)?;
            let (base, topic) = url.trim_end_matches('/').rsplit_once('/').context("the ntfy URL must end with the topic")?;
            let priority = match msg.worst {
                Some(Severity::Critical) => 5,
                Some(Severity::Warning) => 4,
                Some(Severity::Info) => 3,
                None => 2,
            };
            let tags = match msg.worst {
                Some(Severity::Critical) => "rotating_light",
                Some(Severity::Warning) => "warning",
                Some(Severity::Info) => "information_source",
                None => "white_check_mark",
            };
            let mut req = http
                .post(base)
                .json(&json!({ "topic": topic, "title": msg.title, "message": msg.text, "priority": priority, "tags": [tags] }));
            if let Some(t) = token_env
                && let Ok(tok) = std::env::var(t)
                && !tok.is_empty()
            {
                req = req.bearer_auth(tok);
            }
            check(req.send().await.map_err(reqwest::Error::without_url)?).await
        }
        Channel::Telegram { bot_token_env, chat_id_env } => {
            let token = env_required(bot_token_env)?;
            let chat = env_required(chat_id_env)?;
            let req = http
                .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
                .json(&json!({ "chat_id": chat, "text": format!("{}\n{}", msg.title, msg.text), "disable_web_page_preview": true }));
            check(req.send().await.map_err(reqwest::Error::without_url)?).await
        }
        Channel::Webhook { url_env } => {
            let url = env_required(url_env)?;
            let status = match (msg.worst.is_some(), msg.alerts.iter().any(|a| a["state"] == "resolved")) {
                (true, false) => "firing",
                (false, true) => "resolved",
                _ => "mixed",
            };
            let req = http
                .post(url)
                .json(&json!({ "status": status, "title": msg.title, "text": msg.text, "alerts": msg.alerts }));
            check(req.send().await.map_err(reqwest::Error::without_url)?).await
        }
    }
}

/// Error with the status code and the start of the body, without the URL (it may contain the token).
/// Errors from `send` drop it too (`without_url`): a timeout would print it
async fn check(resp: reqwest::Response) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    bail!("response {}: {}", status.as_u16(), body.chars().take(200).collect::<String>())
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn pending_then_firing_then_resolved() {
        let mut inst = BTreeMap::new();
        let m = vec![(labels(&[("instance", "host-1")]), 0.0)];
        // First time: pending, no transition
        assert!(step(&mut inst, "down", &m, 1000, 180).is_empty());
        assert_eq!(inst.len(), 1);
        assert_eq!(inst.values().next().unwrap().phase, Phase::Pending);
        // `for` has not elapsed yet
        assert!(step(&mut inst, "down", &m, 1100, 180).is_empty());
        // `for` elapsed: firing
        let tr = step(&mut inst, "down", &m, 1180, 180);
        assert_eq!(tr.len(), 1);
        assert_eq!((tr[0].from.as_str(), tr[0].to.as_str()), ("pending", "firing"));
        assert_eq!(inst.values().next().unwrap().since, 1180);
        // Condition clears: resolved and removed
        let tr = step(&mut inst, "down", &[], 1300, 180);
        assert_eq!(tr.len(), 1);
        assert_eq!(tr[0].to, "resolved");
        assert!(inst.is_empty());
    }

    #[test]
    fn pending_that_clears_is_silent() {
        let mut inst = BTreeMap::new();
        let m = vec![(labels(&[("instance", "host-2")]), 1.0)];
        step(&mut inst, "r", &m, 10, 300);
        assert!(step(&mut inst, "r", &[], 20, 300).is_empty());
        assert!(inst.is_empty());
    }

    #[test]
    fn for_zero_fires_at_once_and_rules_do_not_mix() {
        let mut inst = BTreeMap::new();
        let tr = step(&mut inst, "ups", &[(labels(&[]), 1.0)], 5, 0);
        assert_eq!(tr.len(), 1);
        assert_eq!(tr[0].from, "inactive");
        // Another rule with no matches does not resolve the UPS one
        assert!(step(&mut inst, "other", &[], 6, 0).is_empty());
        assert_eq!(inst.len(), 1);
    }

    #[test]
    fn silence_matching() {
        let s = Silence {
            id: "s".into(),
            rule: Some("disk".into()),
            labels: labels(&[("instance", "host-1")]),
            until: 100,
            comment: String::new(),
            created: 0,
        };
        assert!(s.matches("disk", &labels(&[("instance", "host-1"), ("mountpoint", "/")]), 50));
        assert!(!s.matches("disk", &labels(&[("instance", "host-2")]), 50));
        assert!(!s.matches("mem", &labels(&[("instance", "host-1")]), 50));
        assert!(!s.matches("disk", &labels(&[("instance", "host-1")]), 100));
        let all = Silence {
            id: "a".into(),
            rule: None,
            labels: Labels::new(),
            until: 100,
            comment: String::new(),
            created: 0,
        };
        assert!(all.matches("anything", &labels(&[("x", "y")]), 1));
    }

    #[test]
    fn a_rule_without_summary_loads_and_shows_its_value() {
        // The logs example in docs/alerts.md has no summary: with it required, the
        // whole file was rejected
        let cfg: Config = serde_saphyr::from_str(
            "rules:\n  - id: app-errors\n    title: Errors in my-app\n    kind: logs\n    filters: ['service:my-app']\n    threshold: 20\n",
        )
        .unwrap();
        assert_eq!(cfg.rules[0].render_summary("en", &labels(&[]), 23.0), "23");
    }

    #[test]
    fn summary_rendering_and_values() {
        let l = labels(&[("instance", "host-1"), ("mountpoint", "/data")]);
        assert_eq!(render("{{instance}} {{mountpoint}} at {{value}} %", &l, 91.456), "host-1 /data at 91.46 %");
        assert_eq!(fmt_value(3.0), "3");
        assert_eq!(fmt_value(1234.5), "1234");
        assert_eq!(fmt_value(0.5), "0.5");
    }

    #[test]
    fn yaml_parses_and_validates() {
        let y = r#"
defaults: { every: 30s, for: 5m, notify: [default] }
channels:
  default: { type: log }
  n: { type: ntfy, url_env: NTFY_URL, token_env: NTFY_TOKEN }
rules:
  - id: target-down
    title: { en: Target down, es: Objetivo caído }
    severity: critical
    expr: up == 0
    for: 3m
    summary: { en: "{{job}} on {{instance}} is down", es: "{{job}} en {{instance}} no responde" }
  - id: ch-errors
    title: ClickHouse errors
    kind: logs
    filters: ["service:clickhouse", "level:ERROR"]
    window: 10m
    threshold: 200
    for: 0s
    summary: "{{value}} errors in 10 min"
"#;
        let cfg: Config = serde_saphyr::from_str(y).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.rules[0].for_secs(&cfg.defaults), 180);
        assert_eq!(cfg.rules[1].for_secs(&cfg.defaults), 0);
        assert_eq!(cfg.rules[1].window_secs(), 600);
        assert_eq!(cfg.channels["n"].env_vars(), vec!["NTFY_URL", "NTFY_TOKEN"]);
        assert_eq!(cfg.rules[0].title.get("es"), "Objetivo caído");
        // Unknown channel
        let bad = r#"
channels: { default: { type: log } }
rules:
  - { id: x, title: X, expr: up, summary: s, notify: [nope] }
"#;
        assert!(validate(&serde_saphyr::from_str(bad).unwrap()).is_err());
        // Logs rule without threshold
        let bad = r#"
channels: { default: { type: log } }
rules:
  - { id: x, title: X, kind: logs, filters: ["level:ERROR"], summary: s }
"#;
        assert!(validate(&serde_saphyr::from_str(bad).unwrap()).is_err());
    }

    #[test]
    fn message_groups_firing_first() {
        let cfg: Config = serde_saphyr::from_str(
            r#"
channels: { default: { type: log } }
rules:
  - { id: a, title: Disk full, severity: warning, expr: x, summary: "{{mountpoint}} at {{value}} %" }
  - { id: b, title: UPS on battery, severity: critical, expr: y, summary: "Running on battery" }
"#,
        )
        .unwrap();
        let t1 = Transition {
            ts: 1,
            rule: "a".into(),
            labels: labels(&[("instance", "host-1"), ("mountpoint", "/")]),
            from: "pending".into(),
            to: "firing".into(),
            value: 91.0,
        };
        let t2 = Transition {
            ts: 2,
            rule: "b".into(),
            labels: Labels::new(),
            from: "inactive".into(),
            to: "firing".into(),
            value: 1.0,
        };
        let t3 = Transition {
            ts: 3,
            rule: "a".into(),
            labels: labels(&[("instance", "host-2"), ("mountpoint", "/")]),
            from: "firing".into(),
            to: "resolved".into(),
            value: 80.0,
        };
        let m = Message::build(&cfg, &[&t1, &t2, &t3], "en");
        assert_eq!(m.worst, Some(Severity::Critical));
        assert!(m.title.contains("2 firing, 1 resolved"));
        let first = m.text.lines().next().unwrap();
        assert!(first.contains("UPS on battery"), "{first}");
        assert!(m.text.contains("/ at 91 %"));
        assert!(m.text.contains("✅ Disk full (instance=host-2,mountpoint=/)"));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atalaya-alerts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The rules in the Compose example load and validate
    #[test]
    fn compose_example_alerts_are_valid() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/data/alerts.yaml"));
        let cfg = load_config(path).unwrap();
        assert!(cfg.rules.len() >= 10);
    }

    #[test]
    fn instance_keys_are_unambiguous() {
        let a = labels(&[("a", "x,b=y")]);
        let b = labels(&[("a", "x"), ("b", "y")]);
        assert_eq!(labels_text(&a), labels_text(&b), "the human-readable text does collide");
        assert_ne!(instance_key("r", &a), instance_key("r", &b));
    }

    #[tokio::test]
    async fn old_state_keys_are_migrated_on_load() {
        let dir = temp_dir("migrate");
        std::fs::create_dir_all(dir.join("alerts")).unwrap();
        let inst = Instance {
            rule: "r".into(),
            labels: labels(&[("instance", "host-1")]),
            phase: Phase::Firing,
            since: 1,
            value: 1.0,
            last_seen: 1,
        };
        let old = State {
            instances: [("r|instance=host-1".to_owned(), inst.clone())].into(),
            ..Default::default()
        };
        std::fs::write(dir.join("alerts/state.json"), serde_json::to_vec(&old).unwrap()).unwrap();
        let a = Alerts::new(dir.join("alerts.yaml"), &dir);
        let st = a.state.lock().await;
        assert!(st.instances.contains_key(&instance_key("r", &inst.labels)));
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A silence created or deleted during evaluation survives the end of the cycle
    #[tokio::test]
    async fn silences_changed_during_evaluation_survive() {
        let dir = temp_dir("silences");
        let a = Alerts::new(dir.join("alerts.yaml"), &dir);
        let old = a
            .add_silence(None, labels(&[("instance", "host-1")]), now() + 3600, "old".into())
            .await
            .unwrap();
        // The cycle copies the state and evaluates…
        let copy = a.state.lock().await.clone();
        // …while the API creates one silence and deletes another
        let new = a
            .add_silence(None, labels(&[("instance", "host-2")]), now() + 3600, "new".into())
            .await
            .unwrap();
        assert!(a.delete_silence(&old.id).await);
        a.finish_cycle(&Config::default(), copy, vec![], now()).await;
        let ids: Vec<String> = a.silences().await.into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![new.id.clone()]);
        // And that is what gets persisted
        let b = Alerts::new(dir.join("alerts.yaml"), &dir);
        assert_eq!(b.silences().await.into_iter().map(|s| s.id).collect::<Vec<_>>(), vec![new.id]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An undeliverable notification stays in the outbox with backoff and survives a restart
    #[tokio::test]
    async fn undelivered_notifications_stay_queued_across_restarts() {
        let dir = temp_dir("outbox");
        std::fs::create_dir_all(&dir).unwrap();
        let yaml = dir.join("alerts.yaml");
        std::fs::write(
            &yaml,
            r#"
defaults: { group_wait: 0s, notify: [hook] }
channels: { hook: { type: webhook, url_env: ATALAYA_TEST_HOOK_NOT_SET } }
rules:
  - { id: a, title: Disk full, severity: warning, expr: x, summary: "full" }
"#,
        )
        .unwrap();
        let a = Alerts::new(yaml.clone(), &dir);
        let cfg = a.config().await;
        let tr = Transition {
            ts: 1,
            rule: "a".into(),
            labels: labels(&[("instance", "host-1")]),
            from: "pending".into(),
            to: "firing".into(),
            value: 1.0,
        };
        let copy = a.state.lock().await.clone();
        a.finish_cycle(&cfg, copy, vec![tr], now()).await;
        assert_eq!(a.pending().await, 1);
        a.deliver().await; // fails: the webhook variable is not set
        {
            let q = a.queue.lock().await;
            assert_eq!(q.len(), 1);
            assert_eq!(q[0].attempts, 1);
            assert!(q[0].next_try > now());
        }
        let b = Alerts::new(yaml, &dir);
        assert_eq!(b.pending().await, 1, "the outbox is restored on startup");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
