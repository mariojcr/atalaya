// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0
// Language and theme. Loaded first (in <head>) to set the theme and `lang`
// before anything is painted. English is the default; Spanish is picked in the
// top bar and remembered in localStorage. Server-side texts (/overview, /events)
// are requested with `lang=` and come back already translated.
//
//   t("key", { n: 3 })   → text with {n} replaced (unknown placeholders are left as is)
//   tn("key", n)         → picks key_one / key_other depending on n
//   fmtNum / fmtInt / fmtDec / fmtDate → Intl in the active language ("4.56 GiB" / "4,56 GiB")
"use strict";

const I18N = {
  en: {
    // Navigation and top bar
    "nav.home": "Home", "nav.timeline": "Timeline", "nav.notebooks": "Notebooks",
    "nav.metrics": "Metrics explorer", "nav.dashboards": "Dashboards", "nav.logs": "Logs explorer",
    "menu": "Menu", "lang.label": "Language", "theme.label": "Theme", "theme.system": "System", "theme.light": "Light", "theme.dark": "Dark",
    "theme.tip": "Theme: {t} · click to change",
    "time.presets": "Preset ranges", "time.from": "From", "time.to": "To", "apply": "Apply", "cancel": "Cancel", "add": "Add", "delete": "Delete",
    "auto": "Auto", "auto.tip": "Refresh every {t}", "min_short": "min", "sec_short": "s",
    "p.15m": "Last 15 minutes", "p.1h": "Last hour", "p.6h": "Last 6 hours", "p.12h": "Last 12 hours", "p.1d": "Last day", "p.2d": "Last 2 days",
    "p.7d": "Last week", "p.30d": "Last 30 days", "p.42d": "Last 6 weeks", "p.365d": "Last year", "p.strip": "^Last ",
    "loading": "Loading…", "querying": "Querying…", "nodata": "No data", "chart.nodata": "No data in the range.",
    // Home
    "home.view": "Home view", "home.tiles": "Board", "home.table": "Table", "home.filter_ph": "host or service", "home.filter_aria": "Filter by host or service",
    "th.host": "Host / service", "th.state": "State", "th.cpu": "CPU", "th.mem": "Mem", "th.net": "Net", "th.err": "Err/min", "th.series": "Series", "th.lastlog": "Last log",
    "keys.move": "move", "keys.enter": "Enter", "keys.expand": "expand", "keys.filter": "filter", "keys.explorer": "explorer", "keys.logs": "logs", "keys.more": "→ more columns",
    "home.allok": "All good", "home.degraded_one": "{n} query failed, data incomplete", "home.degraded_other": "{n} queries failed, data incomplete", "home.warnings_one": "{n} warning", "home.warnings_other": "{n} warnings", "home.targets": "{up}/{total} targets up", "n.series": "{n} series",
    "home.updated": "updated {t}", "home.empty": "No service with data in the range.", "home.rows": "{n} rows · {s} series", "home.nomatch": "Nothing matches “{q}”.",
    "health.ok": "OK", "health.warn": "Warning", "health.bad": "Down", "health.unknown": "No data", "tile.aria": "{job} on {host}: {health}",
    "detail.cpu": "Alloc CPU", "detail.mem": "Memory", "detail.err": "Errors per hour", "detail.logs": "Latest logs", "detail.errmin": "Errors/min", "nologs": "No logs in the range.",
    "chart.cpu": "CPU", "chart.mem": "Memory", "chart.err": "Errors",
    "drawer.close": "Close", "drawer.explore": "Explorer ↗", "drawer.logs": "Logs", "drawer.dash": "Dashboard", "drawer.alloc": "alloc {id}", "drawer.range_custom": "chosen range",
    // Timeline
    "tl.service": "service:", "tl.clear_service": "Remove the service filter", "tl.back": "⤺ back", "tl.add": "+ lane", "tl.add_dialog": "Add lane",
    "tl.query": "PromQL query", "tl.title": "Title", "tl.title_ph": "Disk · host-1", "tl.unit": "Unit", "unit.number": "number", "unit.bytes": "bytes", "unit.Bps": "bytes/s", "unit.s": "seconds",
    "tl.session_hint": "This session only; fixed lanes live in timeline.yaml.", "tl.events": "Events", "tl.logs_slot": "Logs in this slot", "tl.to_nb": "Add to notebook", "tl.values": "Values",
    "tl.help": "Drag to zoom into a span · double-click to go back to the range · ← → move the cursor",
    "tl.count_one": "{n} event", "tl.count_other": "{n} events", "tl.buckets": "{b} buckets", "tl.deploys": "Deploys and restarts", "tl.logs": "Logs ·", "tl.alerts": "Alerts",
    "tl.remove_lane": "Remove lane {t}", "tl.noevents": "No events in the range.", "tl.nothing": "Nothing in this slot.", "tl.nometrics": "No metric lanes.", "tl.nologs": "No logs in this slot.", "tl.err": "err",
    "ev.alloc_started": "New alloc", "ev.alloc_stopped": "Alloc stopped", "ev.restart": "Restart", "ev.errors": "Errors", "ev.log_burst": "Log burst",
    // Notebooks
    "nb.intro": "Investigations saved as documents: text, PromQL charts and logs, with their time range.", "nb.new": "+ Notebook", "nb.back": "‹ Notebooks",
    "nb.edit_title": "Click to edit the title", "nb.title_aria": "Notebook title", "nb.share": "Share", "nb.save": "Save", "nb.range": "Notebook range", "nb.freeze": "Freeze range", "nb.frozen": "Range frozen",
    "nb.cells": "Cells", "nb.add_cell": "+ cell", "nb.text": "Text", "nb.note": "Note", "nb.promql": "PromQL", "nb.logs": "Logs", "nb.outline": "Notebook outline",
    "nb.untitled": "Untitled", "nb.cells_one": "{n} cell", "nb.cells_other": "{n} cells", "nb.none": "No notebooks yet.", "nb.confirm_del": "Delete this notebook?", "no": "No", "nb.yes_del": "Yes, delete",
    "nb.new_title": "New notebook", "nb.unsaved": "unsaved", "nb.saved": "saved {t}", "nb.save_fail": "Could not save: {e}", "nb.copied": "Link copied", "nb.copy_manual": "Copy the link",
    "nb.empty": "This notebook is empty. Add a cell.", "nb.cell_n": "Cell {n}", "up": "Move up", "down": "Move down", "dup": "Duplicate", "nb.edit_text": "Edit text",
    "nb.write_here": "Write here (simple markdown: headings, lists, **bold**, `code`).", "nb.write_short": "Write here (simple markdown).", "nb.use_doc_range": "Use the notebook range",
    "run": "Run ▸", "nb.step": "Step", "nb.compare": "Compare", "nb.legend": "Legend", "nb.show": "Show", "nb.chart": "Chart", "nb.table": "Table",
    "nb.service_ph": "service", "nb.text_ph": "text in the message", "lvl.all": "all", "lvl.warn": "warning or higher", "lvl.error": "error or higher", "lvl.info": "info or higher",
    "nb.write_query": "Write a PromQL query and press Run.", "nb.filter": "Filter: {f} · {a} – {b} · first 50", "nb.f_service": "service = {v}", "nb.f_level": "level ∈ {{v}}", "nb.f_msg": "message contains “{v}”", "nb.nofilter": "no filters",
    "nb.nologs": "No entries with these filters in the range.", "nb.save_in": "Save to a notebook", "nb.save_to": "Save to…", "nb.new_nb": "+ New notebook", "nb.investigation": "Investigation {d}",
    "nb.only_promql": "Only PromQL queries can be saved (the card's PromQL tab).", "nb.press_again": "Click again to delete", "ex.to_nb": "Save to notebook",
    "cmp.no": "no", "cmp.1h_ago": "1 h ago", "cmp.1d_ago": "1 day ago", "cmp.7d_ago": "1 week ago",
    "ago.now": "just now", "ago.min": "{n} min ago", "ago.h": "{n} h ago", "ago.yesterday": "yesterday", "ago.d": "{n} days ago",
    "th.serie": "Series", "th.last": "Last", "th.min": "Min", "th.max": "Max", "th.mean": "Mean",
    "nb.ex_title": "Example: investigating a CPU spike",
    "nb.ex_1": "A notebook mixes notes, queries and logs over one time range. Start with the busy cores of every host: a spike shows which host to look at.",
    "nb.ex_2": "Then split that CPU by mode: **user** points at an application, **system** or **iowait** at the kernel or the disks.",
    "nb.ex_3": "Freeze the range (button above) to keep the evidence as it was, and write down what you found.",
    // Metrics explorer
    "tab.builder": "Builder", "tab.code": "Query", "tab.sql": "SQL", "ex.add_query": "+ Add query", "ex.add_formula": "+ Add formula",
    "ex.code_hint": "Queries in the <code>timeSeries</code> API format (or <code>{\"promql\": …}</code>) and the formulas that combine them. Edit them and run.",
    "ex.run": "Run query", "ex.sql_hint": "The SQL run in ClickHouse for each query (or the expression, for PromQL ones).",
    "ex.chart_type": "Chart type", "ex.line": "Line", "ex.area": "Stacked area", "ex.bar": "Stacked bars", "ex.log": "Log scale", "ex.compare": "Compare with",
    "cmp.none": "No comparison", "cmp.1h": "1 hour ago", "cmp.1d": "1 day ago", "cmp.7d": "1 week ago", "cmp.28d": "4 weeks ago", "cmp.prev": "Previous period", "cmp.custom": "Custom…",
    "ex.custom_ph": "3h, 2d…", "ex.custom_aria": "Custom offset", "ex.pick": "Pick a metric to start.", "legend.table": "Table",
    "ex.bad_offset": "Invalid offset: use e.g. 30m, 3h, 2d or 1w.", "ex.bad_query": "Invalid query: {e}", "ex.query_n": "Query {id}", "ago": "{o} ago", "wk": "wk",
    "st.series": "Series", "st.resolution": "Resolution", "st.alignment": "Alignment", "st.step": "Step", "st.query": "Query", "st.entries": "Entries",
    "tier.raw": "raw", "tier.1m": "1 minute", "tier.10m": "10 minutes",
    "q.mode": "Query mode", "q.builder": "Builder", "q.promql": "PromQL", "show": "Show", "hide": "Hide", "q.show_chart": "Show in the chart", "q.hide_chart": "Hide from the chart", "q.delete": "Delete query",
    "q.promql_aria": "PromQL query", "q.legend_ph": "Legend, e.g. {{device}} (optional)", "q.legend_aria": "Legend template",
    "q.promql_hint": "Ctrl+Enter runs the query. <code>$__rate_interval</code> adapts to the data resolution.",
    "q.select_metric": "Select a metric", "q.unit": ", unit {u}", "q.filters": "Filters", "q.label": "Label", "q.operator": "Operator", "q.value_ph": "value", "q.value": "Value",
    "q.add_filter": "+ Add filter", "q.aggregation": "Aggregation", "q.agg_fn": "Aggregation function",
    "red.none": "No aggregation", "red.mean": "Mean", "red.sum": "Sum", "red.min": "Minimum", "red.max": "Maximum", "red.count": "Count", "red.p50": "Percentile 50", "red.p95": "Percentile 95", "red.p99": "Percentile 99",
    "q.by": "by", "q.group_by": "Group by", "q.label_ellipsis": "label…", "q.rm_group": "Remove grouping", "q.rm_filter": "Remove filter", "q.advanced": "Advanced options", "q.aligner": "Aligner",
    "al.auto": "Automatic (rate for counters, mean for the rest)", "al.rate": "Rate per second", "al.delta": "Increase", "q.min_period": "Minimum alignment period", "automatic": "Automatic", "q.metric_tag": " (metric)",
    "ns.resource": "resource", "ns.metric": "metric",
    "f.aria": "Formula", "f.delete": "Delete formula", "f.name_ph": "Name (optional)", "f.name_aria": "Formula name",
    "f.hint": "Combine queries by their letter with + − × ÷ and parentheses. Series are matched by their labels; a query with a single series applies to all of them.",
    "f.early": "the formula ends too early", "f.paren": "missing closing parenthesis", "f.symbol": "invalid symbol: {t}", "f.extra": "unexpected \"{t}\"", "f.noref": "the formula uses no query", "f.missing": "no query {L}",
    "kind.gauge": "Gauge", "kind.cumulative": "Counter", "kind.delta": "Delta", "kind.distribution": "Distribution",
    "calc.value": "Value", "calc.last": "Last", "calc.first": "First", "calc.mean": "Mean", "calc.max": "Max", "calc.min": "Min", "calc.sum": "Total", "calc.count": "Count", "calc.range": "Range", "calc.delta": "Delta", "calc.diff": "Difference",
    "total": "total", "picker.aria": "Select a metric", "picker.ph": "Filter by resource and metric name", "picker.filter_aria": "Filter metrics",
    "picker.resources": "Resources", "picker.categories": "Categories", "picker.metrics": "Metrics", "all": "All", "all_f": "All", "picker.more": "Show {n} more ({r} left)", "picker.none": "No metric matches.", "picker.count": "{n} metrics",
    // Logs explorer
    "logs.search_ph": "Search in messages", "logs.text": "Text", "logs.severity": "Severity", "sev.all": "All severities", "sev.debug_up": "Debug or higher", "sev.info_up": "Info or higher", "sev.warn_up": "Warning or higher", "sev.error_up": "Error or higher",
    "logs.more": "Load {n} more ({r} left)", "logs.empty": "No entries in the range with these filters.", "logs.service": "Service", "logs.host": "Host",
    "sev.FATAL": "Fatal", "sev.ERROR": "Error", "sev.WARN": "Warning", "sev.INFO": "Info", "sev.DEBUG": "Debug", "sev.TRACE": "Trace", "sev.none": "No level",
    // Logs explorer (Discover)
    "lg.ac": "Suggestions", "lg.ac_fields": "Fields", "lg.ac_values": "Values of {f}", "lg.ac_values_hint": "top values in the current range · type to narrow", "lg.ac_none": "No matching values", "lg.ac_loading": "Looking up values…", "lg.ac_ops": "Operators",
    "lg.ac_op_eq": "equals", "lg.ac_op_neq": "does not equal (prefix -)", "lg.ac_op_has": "contains text", "lg.ac_op_gt": "greater than", "lg.ac_op_lt": "less than", "lg.ac_op_exists": "exists",
    "nav.alerts": "Alerts", "ev.alert_firing": "Alert", "ev.alert_resolved": "Alert resolved", "tl.legend": "Legend", "home.alerts_firing_one": "{n} alert firing", "home.alerts_firing_other": "{n} alerts firing",
    "al.c_firing": "firing", "al.c_pending": "pending", "al.c_silenced": "silenced", "al.c_error": "errors", "al.c_ok": "OK",
    "al.refresh": "Refresh", "al.meta": "{n} rules · evaluated every {every} · updated {t}", "al.config_error": "alerts.yaml has an error and the previous rules stay in force: {e}",
    "al.chan_missing": "not configured", "al.chan_noenv": "No environment variables needed", "al.norules": "No rules in alerts.yaml.",
    "al.state_firing": "Firing", "al.state_pending": "Pending", "al.state_silenced": "Silenced", "al.state_ok": "OK", "al.state_error": "Error", "al.resolved": "Resolved",
    "al.sev_critical": "Critical", "al.sev_warning": "Warning", "al.sev_info": "Info",
    "al.kind_metric": "PromQL", "al.kind_logs": "logs · {n} in {w}", "al.for": "for {d}", "al.for_now": "immediately", "al.changed": "changed {t}",
    "al.n_firing_one": "{n} firing", "al.n_firing_other": "{n} firing", "al.n_pending_one": "{n} pending", "al.n_pending_other": "{n} pending",
    "al.eval_error": "Evaluation error: {e}", "al.no_instances": "Nothing matches right now.", "al.test": "Test now", "al.silence": "Silence", "al.silence_rule": "Silence rule",
    "al.notify": "Notifies: {c}", "al.test_matches": "{n} match(es) right now ({ms} ms); the state is not changed", "al.test_none": "No matches right now ({ms} ms)",
    "al.silences": "Silences", "al.nosilences": "No active silences.", "al.all_rules": "All rules", "al.until": "until {t}", "al.unsilence": "Remove silence",
    "al.history": "History", "al.history_range": "{n} changes in {r}", "al.nohistory": "No alert changes in the range.",
    "al.silence_title": "Silence", "al.silence_hint": "Silenced alerts stay visible here but are not sent to the channels.", "al.duration": "Duration", "al.until_custom": "Until…", "al.until_label": "Until",
    "al.comment": "Comment", "al.comment_ph": "Why it is silenced", "al.sil_all_instances": "every instance of the rule",
    "lg.search_ph": "Search: free text, field:value, -field:value, field>10, field~text, \"a phrase\"", "lg.search_aria": "Query", "lg.clear": "Clear query", "lg.run": "Search",
    "lg.open": "Open", "lg.save": "Save", "lg.saved": "Saved searches", "lg.save_as": "Save search as", "lg.name_ph": "Name", "lg.saved_none": "No saved searches yet.", "lg.del_search": "Delete the search {t}", "lg.saved_ok": "Saved “{t}”",
    "lg.to_nb": "Add to notebook", "lg.not": "NOT", "lg.exists": "exists", "lg.pill_toggle": "Click to include or exclude", "lg.pill_rm": "Remove the filter {f}",
    "lg.fields": "Fields", "lg.field_q": "Search field names", "lg.selected": "Selected fields", "lg.available": "Available fields", "lg.no_fields": "No field matches.", "lg.fields_loading": "Reading the fields…",
    "lg.top_values": "Top 5 values in the {n} loaded documents", "lg.top_range": "Most frequent values in the range", "lg.distinct": "{n} distinct", "lg.blank": "(empty)",
    "lg.filter_for": "Filter for value", "lg.filter_out": "Filter out value", "lg.add_col": "Add as column", "lg.rm_col": "Remove column", "lg.col_left": "Move column left", "lg.col_right": "Move column right", "lg.visualize": "Break down the histogram by this field",
    "lg.hits_one": "{n} hit", "lg.hits_other": "{n} hits", "lg.in": "in {ms} ms", "lg.bk": "Break down by", "lg.bk_none": "nothing", "lg.other": "Other", "lg.count": "Documents",
    "lg.zoom_hint": "Drag to zoom · click a bar to open that bucket", "lg.time": "Time", "lg.document": "Document", "lg.expand": "Expand document", "lg.collapse": "Collapse document",
    "lg.tab_table": "Table", "lg.context": "View surrounding documents", "lg.ctx_title": "Documents around {t}", "lg.ctx_n": "Before and after", "lg.ctx_empty": "No documents around this one.",
    "lg.more": "Show {n} more ({r} left)", "lg.older": "Load older documents", "lg.all_loaded": "All {n} loaded documents are shown",
    "lg.empty": "No documents match in this range. Widen the range or remove a filter.",
    "nb.f_filters": "filters {v}",
    // Dashboards
    "dash.open": "Open in the metrics explorer", "dash.open_logs": "Open in the logs explorer", "dash.open_aria": "Open in the explorer", "dash.desc": "Description: {d}", "dash.untitled": "Untitled", "dash.none": "None",
    "dash.empty": "No dashboards. Add a YAML file to the dashboards directory.",
    // Units
    "u.year": "year", "u.years": "years", "u.wk": "wk", "u.day": "day", "u.days": "days", "u.hour": "hour", "u.hours": "hours", "u.minute": "minute", "u.minutes": "minutes", "u.second": "second", "u.seconds": "seconds",
    "u.few_seconds": "a few seconds", "u.ago": "{t} ago", "u.in": "in {t}", "u.true": "True", "u.false": "False", "u.yes": "Yes", "u.on": "On", "u.off": "Off",
  },
  es: {
    "nav.home": "Inicio", "nav.timeline": "Línea de tiempo", "nav.notebooks": "Cuadernos",
    "nav.metrics": "Explorador de métricas", "nav.dashboards": "Paneles", "nav.logs": "Explorador de logs",
    "menu": "Menú", "lang.label": "Idioma", "theme.label": "Tema", "theme.system": "Sistema", "theme.light": "Claro", "theme.dark": "Oscuro",
    "theme.tip": "Tema: {t} · pulsa para cambiar",
    "time.presets": "Rangos predefinidos", "time.from": "Desde", "time.to": "Hasta", "apply": "Aplicar", "cancel": "Cancelar", "add": "Añadir", "delete": "Borrar",
    "auto": "Auto", "auto.tip": "Actualizar cada {t}", "min_short": "min", "sec_short": "s",
    "p.15m": "Últimos 15 minutos", "p.1h": "Última hora", "p.6h": "Últimas 6 horas", "p.12h": "Últimas 12 horas", "p.1d": "Último día", "p.2d": "Últimos 2 días",
    "p.7d": "Última semana", "p.30d": "Últimos 30 días", "p.42d": "Últimas 6 semanas", "p.365d": "Último año", "p.strip": "^Últim[oa]s? ",
    "loading": "Cargando…", "querying": "Consultando…", "nodata": "Sin datos", "chart.nodata": "No hay datos en el rango.",
    "home.view": "Vista del inicio", "home.tiles": "Tablero", "home.table": "Tabla", "home.filter_ph": "host o servicio", "home.filter_aria": "Filtrar por host o servicio",
    "th.host": "Host / servicio", "th.state": "Est.", "th.cpu": "CPU", "th.mem": "Mem", "th.net": "Red", "th.err": "Err/min", "th.series": "Series", "th.lastlog": "Último log",
    "keys.move": "mover", "keys.enter": "Intro", "keys.expand": "expandir", "keys.filter": "filtrar", "keys.explorer": "explorador", "keys.logs": "logs", "keys.more": "→ más columnas",
    "home.allok": "Todo bien", "home.degraded_one": "{n} consulta falló, datos incompletos", "home.degraded_other": "{n} consultas fallaron, datos incompletos", "home.warnings_one": "{n} aviso", "home.warnings_other": "{n} avisos", "home.targets": "{up}/{total} objetivos arriba", "n.series": "{n} series",
    "home.updated": "actualizado {t}", "home.empty": "Ningún servicio con datos en el rango.", "home.rows": "{n} filas · {s} series", "home.nomatch": "Nada coincide con «{q}».",
    "health.ok": "OK", "health.warn": "Aviso", "health.bad": "Mal", "health.unknown": "Sin datos", "tile.aria": "{job} en {host}: {health}",
    "detail.cpu": "CPU de la alloc", "detail.mem": "Memoria", "detail.err": "Errores por hora", "detail.logs": "Últimos logs", "detail.errmin": "Errores/min", "nologs": "Sin logs en el rango.",
    "chart.cpu": "CPU", "chart.mem": "Memoria", "chart.err": "Errores",
    "drawer.close": "Cerrar", "drawer.explore": "Explorador ↗", "drawer.logs": "Logs", "drawer.dash": "Panel", "drawer.alloc": "alloc {id}", "drawer.range_custom": "rango elegido",
    "tl.service": "servicio:", "tl.clear_service": "Quitar el filtro de servicio", "tl.back": "⤺ volver", "tl.add": "+ carril", "tl.add_dialog": "Añadir carril",
    "tl.query": "Consulta PromQL", "tl.title": "Título", "tl.title_ph": "Disco · host-1", "tl.unit": "Unidad", "unit.number": "número", "unit.bytes": "bytes", "unit.Bps": "bytes/s", "unit.s": "segundos",
    "tl.session_hint": "Solo para esta sesión; los carriles fijos están en timeline.yaml.", "tl.events": "Eventos", "tl.logs_slot": "Logs en ese tramo", "tl.to_nb": "Añadir al cuaderno", "tl.values": "Valores",
    "tl.help": "Arrastra para ampliar un tramo · doble clic para volver al rango · ← → mueven el cursor",
    "tl.count_one": "{n} evento", "tl.count_other": "{n} eventos", "tl.buckets": "tramos de {b}", "tl.deploys": "Despliegues y reinicios", "tl.logs": "Logs ·", "tl.alerts": "Alertas",
    "tl.remove_lane": "Quitar carril {t}", "tl.noevents": "Sin eventos en el rango.", "tl.nothing": "Nada en este tramo.", "tl.nometrics": "Sin carriles de métricas.", "tl.nologs": "Sin logs en este tramo.", "tl.err": "err",
    "ev.alloc_started": "Alloc nueva", "ev.alloc_stopped": "Alloc parada", "ev.restart": "Reinicio", "ev.errors": "Errores", "ev.log_burst": "Ráfaga de logs",
    "nb.intro": "Investigaciones guardadas como documentos: texto, gráficas PromQL y logs, con su rango de tiempo.", "nb.new": "+ Cuaderno", "nb.back": "‹ Cuadernos",
    "nb.edit_title": "Pulsa para editar el título", "nb.title_aria": "Título del cuaderno", "nb.share": "Compartir", "nb.save": "Guardar", "nb.range": "Rango del cuaderno", "nb.freeze": "Congelar rango", "nb.frozen": "Rango congelado",
    "nb.cells": "Celdas", "nb.add_cell": "+ celda", "nb.text": "Texto", "nb.note": "Nota", "nb.promql": "PromQL", "nb.logs": "Logs", "nb.outline": "Índice del cuaderno",
    "nb.untitled": "Sin título", "nb.cells_one": "{n} celda", "nb.cells_other": "{n} celdas", "nb.none": "Aún no hay cuadernos.", "nb.confirm_del": "¿Borrar este cuaderno?", "no": "No", "nb.yes_del": "Sí, borrar",
    "nb.new_title": "Cuaderno nuevo", "nb.unsaved": "sin guardar", "nb.saved": "guardado {t}", "nb.save_fail": "No se pudo guardar: {e}", "nb.copied": "Enlace copiado", "nb.copy_manual": "Copia el enlace",
    "nb.empty": "Este cuaderno está vacío. Añade una celda.", "nb.cell_n": "Celda {n}", "up": "Subir", "down": "Bajar", "dup": "Duplicar", "nb.edit_text": "Editar texto",
    "nb.write_here": "Escribe aquí (markdown sencillo: títulos, listas, **negrita**, `código`).", "nb.write_short": "Escribe aquí (markdown sencillo).", "nb.use_doc_range": "Usar el rango del cuaderno",
    "run": "Ejecutar ▸", "nb.step": "Paso", "nb.compare": "Comparar", "nb.legend": "Leyenda", "nb.show": "Mostrar", "nb.chart": "Gráfico", "nb.table": "Tabla",
    "nb.service_ph": "servicio", "nb.text_ph": "texto en el mensaje", "lvl.all": "todas", "lvl.warn": "aviso o superior", "lvl.error": "error o superior", "lvl.info": "info o superior",
    "nb.write_query": "Escribe una consulta PromQL y pulsa Ejecutar.", "nb.filter": "Filtro: {f} · {a} – {b} · 50 primeras", "nb.f_service": "servicio = {v}", "nb.f_level": "nivel ∈ {{v}}", "nb.f_msg": "mensaje contiene “{v}”", "nb.nofilter": "sin filtros",
    "nb.nologs": "Sin entradas con estos filtros en el rango.", "nb.save_in": "Guardar en un cuaderno", "nb.save_to": "Guardar en…", "nb.new_nb": "+ Cuaderno nuevo", "nb.investigation": "Investigación {d}",
    "nb.only_promql": "Solo se guardan consultas PromQL (pestaña PromQL de la tarjeta).", "nb.press_again": "Pulsa otra vez para borrar", "ex.to_nb": "Guardar en cuaderno",
    "cmp.no": "no", "cmp.1h_ago": "hace 1 h", "cmp.1d_ago": "hace 1 día", "cmp.7d_ago": "hace 1 semana",
    "ago.now": "hace un momento", "ago.min": "hace {n} min", "ago.h": "hace {n} h", "ago.yesterday": "ayer", "ago.d": "hace {n} días",
    "th.serie": "Serie", "th.last": "Último", "th.min": "Mín.", "th.max": "Máx.", "th.mean": "Media",
    "nb.ex_title": "Ejemplo: investigar un pico de CPU",
    "nb.ex_1": "Un cuaderno junta notas, consultas y logs sobre un mismo rango. Empieza por los núcleos ocupados de cada host: un pico dice a qué host mirar.",
    "nb.ex_2": "Después reparte esa CPU por modo: **user** apunta a una aplicación; **system** o **iowait**, al kernel o a los discos.",
    "nb.ex_3": "Congela el rango (botón de arriba) para guardar la evidencia tal cual y apunta lo que encuentres.",
    "tab.builder": "Constructor", "tab.code": "Consulta", "tab.sql": "SQL", "ex.add_query": "+ Añadir consulta", "ex.add_formula": "+ Añadir fórmula",
    "ex.code_hint": "Las consultas en el formato de la API <code>timeSeries</code> (o <code>{\"promql\": …}</code>) y las fórmulas que las combinan. Edítalas y ejecútalas.",
    "ex.run": "Ejecutar consulta", "ex.sql_hint": "El SQL que se ha ejecutado en ClickHouse para cada consulta (o la expresión, en las de PromQL).",
    "ex.chart_type": "Tipo de gráfica", "ex.line": "Línea", "ex.area": "Área apilada", "ex.bar": "Barras apiladas", "ex.log": "Escala logarítmica", "ex.compare": "Comparar con",
    "cmp.none": "Sin comparar", "cmp.1h": "Hace 1 hora", "cmp.1d": "Hace 1 día", "cmp.7d": "Hace 1 semana", "cmp.28d": "Hace 4 semanas", "cmp.prev": "Periodo anterior", "cmp.custom": "Personalizado…",
    "ex.custom_ph": "3h, 2d…", "ex.custom_aria": "Desfase personalizado", "ex.pick": "Elige una métrica para empezar.", "legend.table": "Tabla",
    "ex.bad_offset": "Desfase no válido: usa por ejemplo 30m, 3h, 2d o 1w.", "ex.bad_query": "Consulta no válida: {e}", "ex.query_n": "Consulta {id}", "ago": "hace {o}", "wk": "sem.",
    "st.series": "Series", "st.resolution": "Resolución", "st.alignment": "Alineación", "st.step": "Paso", "st.query": "Consulta", "st.entries": "Entradas",
    "tier.raw": "crudo", "tier.1m": "1 minuto", "tier.10m": "10 minutos",
    "q.mode": "Modo de la consulta", "q.builder": "Constructor", "q.promql": "PromQL", "show": "Mostrar", "hide": "Ocultar", "q.show_chart": "Mostrar en la gráfica", "q.hide_chart": "Ocultar en la gráfica", "q.delete": "Borrar consulta",
    "q.promql_aria": "Consulta PromQL", "q.legend_ph": "Leyenda, p. ej. {{device}} (opcional)", "q.legend_aria": "Plantilla de la leyenda",
    "q.promql_hint": "Ctrl+Intro ejecuta la consulta. <code>$__rate_interval</code> se ajusta a la resolución de los datos.",
    "q.select_metric": "Selecciona una métrica", "q.unit": ", unidad {u}", "q.filters": "Filtros", "q.label": "Etiqueta", "q.operator": "Operador", "q.value_ph": "valor", "q.value": "Valor",
    "q.add_filter": "+ Añadir filtro", "q.aggregation": "Agregación", "q.agg_fn": "Función de agregación",
    "red.none": "Sin agregar", "red.mean": "Media", "red.sum": "Suma", "red.min": "Mínimo", "red.max": "Máximo", "red.count": "Recuento", "red.p50": "Percentil 50", "red.p95": "Percentil 95", "red.p99": "Percentil 99",
    "q.by": "por", "q.group_by": "Agrupar por", "q.label_ellipsis": "etiqueta…", "q.rm_group": "Quitar agrupación", "q.rm_filter": "Quitar filtro", "q.advanced": "Opciones avanzadas", "q.aligner": "Alineador",
    "al.auto": "Automático (tasa para contadores, media para el resto)", "al.rate": "Tasa por segundo", "al.delta": "Incremento", "q.min_period": "Periodo mínimo de alineación", "automatic": "Automático", "q.metric_tag": " (métrica)",
    "ns.resource": "recurso", "ns.metric": "métrica",
    "f.aria": "Fórmula", "f.delete": "Borrar fórmula", "f.name_ph": "Nombre (opcional)", "f.name_aria": "Nombre de la fórmula",
    "f.hint": "Combina las consultas por su letra con + − × ÷ y paréntesis. Las series se emparejan por sus etiquetas; una consulta con una sola serie se aplica a todas.",
    "f.early": "la fórmula termina antes de tiempo", "f.paren": "falta un paréntesis de cierre", "f.symbol": "símbolo no válido: {t}", "f.extra": "sobra \"{t}\"", "f.noref": "la fórmula no usa ninguna consulta", "f.missing": "no hay consulta {L}",
    "kind.gauge": "Gauge", "kind.cumulative": "Contador", "kind.delta": "Delta", "kind.distribution": "Distribución",
    "calc.value": "Valor", "calc.last": "Último", "calc.first": "Primero", "calc.mean": "Media", "calc.max": "Máx.", "calc.min": "Mín.", "calc.sum": "Total", "calc.count": "Recuento", "calc.range": "Rango", "calc.delta": "Delta", "calc.diff": "Diferencia",
    "total": "total", "picker.aria": "Seleccionar métrica", "picker.ph": "Filtrar por recurso y nombre de métrica", "picker.filter_aria": "Filtrar métricas",
    "picker.resources": "Recursos", "picker.categories": "Categorías", "picker.metrics": "Métricas", "all": "Todos", "all_f": "Todas", "picker.more": "Mostrar {n} más ({r} restantes)", "picker.none": "Ninguna métrica coincide.", "picker.count": "{n} métricas",
    "logs.search_ph": "Buscar en los mensajes", "logs.text": "Texto", "logs.severity": "Gravedad", "sev.all": "Todas las gravedades", "sev.debug_up": "Debug o superior", "sev.info_up": "Info o superior", "sev.warn_up": "Aviso o superior", "sev.error_up": "Error o superior",
    "logs.more": "Cargar {n} más ({r} restantes)", "logs.empty": "No hay entradas en el rango con estos filtros.", "logs.service": "Servicio", "logs.host": "Host",
    "sev.FATAL": "Crítico", "sev.ERROR": "Error", "sev.WARN": "Aviso", "sev.INFO": "Info", "sev.DEBUG": "Debug", "sev.TRACE": "Traza", "sev.none": "Sin nivel",
    // Logs explorer (Discover)
    "lg.ac": "Sugerencias", "lg.ac_fields": "Campos", "lg.ac_values": "Valores de {f}", "lg.ac_values_hint": "valores más frecuentes en el rango actual · escribe para acotar", "lg.ac_none": "Ningún valor coincide", "lg.ac_loading": "Buscando valores…", "lg.ac_ops": "Operadores",
    "lg.ac_op_eq": "igual a", "lg.ac_op_neq": "distinto de (prefijo -)", "lg.ac_op_has": "contiene el texto", "lg.ac_op_gt": "mayor que", "lg.ac_op_lt": "menor que", "lg.ac_op_exists": "existe",
    "nav.alerts": "Alertas", "ev.alert_firing": "Alerta", "ev.alert_resolved": "Alerta resuelta", "tl.legend": "Leyenda", "home.alerts_firing_one": "{n} alerta activa", "home.alerts_firing_other": "{n} alertas activas",
    "al.c_firing": "activas", "al.c_pending": "pendientes", "al.c_silenced": "silenciadas", "al.c_error": "con error", "al.c_ok": "OK",
    "al.refresh": "Actualizar", "al.meta": "{n} reglas · se evalúan cada {every} · actualizado {t}", "al.config_error": "alerts.yaml tiene un error y siguen en vigor las reglas anteriores: {e}",
    "al.chan_missing": "sin configurar", "al.chan_noenv": "No necesita variables de entorno", "al.norules": "No hay reglas en alerts.yaml.",
    "al.state_firing": "Activa", "al.state_pending": "Pendiente", "al.state_silenced": "Silenciada", "al.state_ok": "OK", "al.state_error": "Error", "al.resolved": "Resuelta",
    "al.sev_critical": "Crítica", "al.sev_warning": "Aviso", "al.sev_info": "Información",
    "al.kind_metric": "PromQL", "al.kind_logs": "logs · {n} en {w}", "al.for": "durante {d}", "al.for_now": "al momento", "al.changed": "cambió {t}",
    "al.n_firing_one": "{n} activa", "al.n_firing_other": "{n} activas", "al.n_pending_one": "{n} pendiente", "al.n_pending_other": "{n} pendientes",
    "al.eval_error": "Error al evaluar: {e}", "al.no_instances": "Ahora mismo no se cumple.", "al.test": "Probar ahora", "al.silence": "Silenciar", "al.silence_rule": "Silenciar la regla",
    "al.notify": "Avisa a: {c}", "al.test_matches": "{n} coincidencia(s) ahora mismo ({ms} ms); el estado no cambia", "al.test_none": "Nada coincide ahora mismo ({ms} ms)",
    "al.silences": "Silencios", "al.nosilences": "No hay silencios activos.", "al.all_rules": "Todas las reglas", "al.until": "hasta {t}", "al.unsilence": "Quitar el silencio",
    "al.history": "Historia", "al.history_range": "{n} cambios en {r}", "al.nohistory": "Sin cambios de alertas en el rango.",
    "al.silence_title": "Silenciar", "al.silence_hint": "Las alertas silenciadas siguen viéndose aquí, pero no se mandan a los canales.", "al.duration": "Duración", "al.until_custom": "Hasta…", "al.until_label": "Hasta",
    "al.comment": "Comentario", "al.comment_ph": "Por qué se silencia", "al.sil_all_instances": "todas las instancias de la regla",
    "lg.search_ph": "Buscar: texto libre, campo:valor, -campo:valor, campo>10, campo~texto, \"una frase\"", "lg.search_aria": "Consulta", "lg.clear": "Limpiar la consulta", "lg.run": "Buscar",
    "lg.open": "Abrir", "lg.save": "Guardar", "lg.saved": "Búsquedas guardadas", "lg.save_as": "Guardar la búsqueda como", "lg.name_ph": "Nombre", "lg.saved_none": "Aún no hay búsquedas guardadas.", "lg.del_search": "Borrar la búsqueda {t}", "lg.saved_ok": "Guardada «{t}»",
    "lg.to_nb": "Añadir al cuaderno", "lg.not": "NO", "lg.exists": "existe", "lg.pill_toggle": "Pulsa para incluir o excluir", "lg.pill_rm": "Quitar el filtro {f}",
    "lg.fields": "Campos", "lg.field_q": "Buscar campos", "lg.selected": "Campos seleccionados", "lg.available": "Campos disponibles", "lg.no_fields": "Ningún campo coincide.", "lg.fields_loading": "Leyendo los campos…",
    "lg.top_values": "5 valores más frecuentes en los {n} documentos cargados", "lg.top_range": "Valores más frecuentes en el rango", "lg.distinct": "{n} distintos", "lg.blank": "(vacío)",
    "lg.filter_for": "Filtrar por este valor", "lg.filter_out": "Excluir este valor", "lg.add_col": "Añadir como columna", "lg.rm_col": "Quitar la columna", "lg.col_left": "Mover la columna a la izquierda", "lg.col_right": "Mover la columna a la derecha", "lg.visualize": "Desglosar el histograma por este campo",
    "lg.hits_one": "{n} resultado", "lg.hits_other": "{n} resultados", "lg.in": "en {ms} ms", "lg.bk": "Desglosar por", "lg.bk_none": "nada", "lg.other": "Otros", "lg.count": "Documentos",
    "lg.zoom_hint": "Arrastra para acercar · pulsa una barra para abrir ese tramo", "lg.time": "Hora", "lg.document": "Documento", "lg.expand": "Desplegar el documento", "lg.collapse": "Plegar el documento",
    "lg.tab_table": "Tabla", "lg.context": "Ver los documentos cercanos", "lg.ctx_title": "Documentos alrededor de {t}", "lg.ctx_n": "Antes y después", "lg.ctx_empty": "No hay documentos alrededor de este.",
    "lg.more": "Mostrar {n} más ({r} restantes)", "lg.older": "Cargar documentos anteriores", "lg.all_loaded": "Se muestran los {n} documentos cargados",
    "lg.empty": "Ningún documento coincide en este rango. Amplía el rango o quita un filtro.",
    "nb.f_filters": "filtros {v}",
    "dash.open": "Abrir en el explorador de métricas", "dash.open_logs": "Abrir en el explorador de logs", "dash.open_aria": "Abrir en el explorador", "dash.desc": "Descripción: {d}", "dash.untitled": "Sin título", "dash.none": "Ninguno",
    "dash.empty": "No hay paneles. Añade un fichero YAML al directorio de paneles.",
    "u.year": "año", "u.years": "años", "u.wk": "sem.", "u.day": "día", "u.days": "días", "u.hour": "hora", "u.hours": "horas", "u.minute": "minuto", "u.minutes": "minutos", "u.second": "segundo", "u.seconds": "segundos",
    "u.few_seconds": "unos segundos", "u.ago": "hace {t}", "u.in": "dentro de {t}", "u.true": "Verdadero", "u.false": "Falso", "u.yes": "Sí", "u.on": "Encendido", "u.off": "Apagado",
  },
};

// ── Language ───────────────────────────────────────────────────────────────
const LANG_KEY = "atalaya.lang";
const LANGS = ["en", "es"];
const LANG = (() => { try { const v = localStorage.getItem(LANG_KEY); if (LANGS.includes(v)) return v; } catch { /* no storage */ } return "en"; })();
// en-GB: English with day/month dates and 24 h times
const LOCALE = LANG === "es" ? "es-ES" : "en-GB";
document.documentElement.lang = LANG;

/** Text in the active language; {k} is replaced by params.k (unknown placeholders are left as is) */
function t(key, params) {
  let s = I18N[LANG][key] ?? I18N.en[key];
  if (s === undefined) { console.warn("i18n: missing key", key); s = key; }
  return params ? s.replace(/\{(\w+)\}/g, (m, k) => (k in params ? String(params[k]) : m)) : s;
}
/** Singular or plural depending on n: key_one / key_other, with {n} already formatted */
function tn(key, n, params) { return t(`${key}_${n === 1 ? "one" : "other"}`, { n: fmtInt(n), ...(params || {}) }); }
/** Switches language: saves it and reloads (every view's template is already rendered) */
function setLang(l) {
  if (!LANGS.includes(l) || l === LANG) return;
  try { localStorage.setItem(LANG_KEY, l); } catch { /* no storage */ }
  location.reload();
}
/** Applies data-i18n (text), data-i18n-html, -placeholder, -aria (aria-label), -tip (data-tip) and -title under `root` */
function applyI18n(root = document) {
  root.querySelectorAll("[data-i18n]").forEach((el) => { el.textContent = t(el.dataset.i18n); });
  root.querySelectorAll("[data-i18n-html]").forEach((el) => { el.innerHTML = t(el.dataset.i18nHtml); });
  const ATTRS = [["data-i18n-placeholder", "i18nPlaceholder", "placeholder"], ["data-i18n-aria", "i18nAria", "aria-label"], ["data-i18n-tip", "i18nTip", "data-tip"], ["data-i18n-title", "i18nTitle", "title"]];
  for (const [sel, ds, attr] of ATTRS) root.querySelectorAll(`[${sel}]`).forEach((el) => { el.setAttribute(attr, t(el.dataset[ds])); });
}

// ── Localized numbers and dates ────────────────────────────────────────────
const nfCache = new Map();
function nf(opts) { const k = JSON.stringify(opts); let f = nfCache.get(k); if (!f) { f = new Intl.NumberFormat(LOCALE, opts); nfCache.set(k, f); } return f; }
/** Number with fixed `decimals` or, without them, at most `max` decimals (2 by default) */
function fmtNum(v, decimals, max) {
  if (v == null || Number.isNaN(v)) return "—";
  if (decimals != null) return nf({ minimumFractionDigits: decimals, maximumFractionDigits: decimals }).format(v);
  return nf({ maximumFractionDigits: max ?? 2 }).format(v);
}
/** Integer ("1,234" / "1.234") */
const fmtInt = (n) => (n == null || Number.isNaN(n) ? "—" : nf({ maximumFractionDigits: 0 }).format(n));
/** Up to d decimals (1 by default), without trailing zeros */
const fmtDec = (n, d = 1) => fmtNum(n, undefined, d);
/** With `sig` significant digits (for very small values) */
const fmtSig = (v, sig = 2) => (v == null || Number.isNaN(v) ? "—" : nf({ maximumSignificantDigits: sig }).format(v));
const dfCache = new Map();
/** Date/time with Intl.DateTimeFormat options (dateStyle, timeStyle, hour…); `d` is a Date or ms */
function fmtDate(d, opts = { dateStyle: "short", timeStyle: "medium" }) {
  const k = JSON.stringify(opts); let f = dfCache.get(k); if (!f) { f = new Intl.DateTimeFormat(LOCALE, opts); dfCache.set(k, f); }
  return f.format(d instanceof Date ? d : new Date(d));
}
// Month and day names for uPlot axes (uPlot only ships English ones)
const UPLOT_NAMES = LANG === "en" ? undefined : (() => {
  const month = (m, o) => new Intl.DateTimeFormat(LOCALE, { month: m }), day = (w) => new Intl.DateTimeFormat(LOCALE, { weekday: w });
  const cap = (s) => s.charAt(0).toUpperCase() + s.slice(1).replace(/\.$/, "");
  const months = (m) => Array.from({ length: 12 }, (_, i) => cap(month(m).format(new Date(2026, i, 1))));
  const days = (w) => Array.from({ length: 7 }, (_, i) => cap(day(w).format(new Date(2026, 2, 1 + i)))); // 2026-03-01 is a Sunday
  return { MMMM: months("long"), MMM: months("short"), WWWW: days("long"), WWW: days("short") };
})();

// ── Theme: system, light or dark ───────────────────────────────────────────
const THEME_KEY = "atalaya.theme";
const THEMES = ["system", "light", "dark"];
const PAGE_COLOR = { light: "#f4f5f7", dark: "#0f1318" };
function themeGet() { try { const v = localStorage.getItem(THEME_KEY); return THEMES.includes(v) ? v : "system"; } catch { return "system"; } }
/** The effective theme (light or dark) currently painted */
function themeEffective() { const v = themeGet(); return v === "system" ? (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : v; }
function applyTheme(v) {
  const root = document.documentElement;
  if (v === "system") delete root.dataset.theme; else root.dataset.theme = v;
  // The browser bar follows the effective theme (the <meta> tags with media only work for "system")
  const color = PAGE_COLOR[themeEffective()];
  document.querySelectorAll('meta[name="theme-color"]').forEach((m) => { m.content = color; });
}
/** Sets and saves the theme, then emits atalaya:theme so charts repaint with the new tokens */
function setTheme(v) {
  if (!THEMES.includes(v)) return;
  try { localStorage.setItem(THEME_KEY, v); } catch { /* no storage */ }
  applyTheme(v);
  document.dispatchEvent(new CustomEvent("atalaya:theme", { detail: v }));
}
applyTheme(themeGet());
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (themeGet() === "system") { applyTheme("system"); document.dispatchEvent(new CustomEvent("atalaya:theme", { detail: "system" })); }
});
document.addEventListener("DOMContentLoaded", () => applyI18n());
