# Dashboards

Each dashboard is a `<name>.yaml` file in the dashboards directory
(`ATALAYA_DASHBOARDS`). The format follows Grafana's model, so dashboards
converted from Grafana keep their queries, units and grid. Examples live in
`examples/data/dashboards/`.

## A minimal dashboard

```yaml
title: My app
refresh: 30s
time: 6h
variables:
  - name: host
    label: Host
    query: label_values(up{job="my-app"}, instance)
    includeAll: true
sections:
  - title: Traffic
    widgets:
      - title: Requests per second
        type: timeseries
        grid: [0, 0, 12, 8]
        unit: reqps
        queries:
          - expr: sum by (instance) (rate(http_requests_total{job="my-app", instance=~"$host"}[$__rate_interval]))
            legend: "{{instance}}"
      - title: Errors now
        type: stat
        grid: [12, 0, 4, 4]
        unit: percentunit
        thresholds: {steps: [[null, green], [0.01, orange], [0.05, red]]}
        queries:
          - expr: sum(rate(http_requests_total{job="my-app", code=~"5.."}[5m])) / sum(rate(http_requests_total{job="my-app"}[5m]))
```

## Reference

```
title, description
refresh: 30s                  default auto-refresh interval
time: 24h                     default range (unless one was picked in the session)
nowDelay: 45s                 how far "now" lags in relative ranges (time for data to arrive; ATALAYA_NOW_DELAY by default)
variables:                    drop-downs; substituted as $var, ${var}, ${var:regex} and [[var]]
  - name, label, hidden
    query: label_values(metric{…}, label) | label_values(label)
    regex: /…/                filter (and capture) of the values, as in Grafana
    multi, includeAll, allValue, sort (0 none, 1 asc, 2 desc, 3/4 numeric)
sections:                     Grafana rows; collapsed: true does not query until opened
  - title, collapsed
    widgets:
      - title, description
        type: timeseries | stat | gauge | bargauge | table | timeline | pie | bar | text
        grid: [x, y, w, h]    Grafana units: 24 columns, 30 px rows; y relative to the section
        unit, decimals, min, max, noValue
        thresholds: {mode: absolute|percentage, steps: [[null, green], [80, red]]}
        thresholdsStyle: line | dashed            thresholds as reference lines (timeseries)
        mappings: [{value: "1", text: Yes, color: green} | {from: 0, to: 10, text, color} | {match: null|nan|true|false|empty, text, color}]
        color: thresholds | palette | fixed:<color>
        style: {draw: line|bars|points, fill: 0-100, lineWidth, interpolation: linear|smooth|stepBefore|stepAfter,
                showPoints: auto|never|always, stack: normal|percent, softMin, softMax, logBase, axis: hidden, spanNulls}
        legend: {show, mode: list|table, placement: bottom|right, calcs: [mean, max, min, lastNotNull, sum, …]}
        stat:     {reduce: lastNotNull|last|mean|max|min|sum|first|count, field: <label>, graph: area|none,
                   colorMode: value|background|none, textMode: auto|value|name|value_and_name|none, orientation}
        gauge:    {reduce, markers: true}
        bargauge: {reduce, mode: basic|gradient|lcd}
        pie:      {reduce, donut: true, legend: true}
        timeline: {field: <label>, name: <row name>, showValue: auto|always|never, merge: true}
        table:    {join: <label>, columns: [{field: <label or query ref>, name, unit, decimals, scale,
                   min, max, cell: text|color-text|color-background|bar, width}], hide: [fields], rename: [{field, name}],
                   sort: {field, desc}, footer: sum|mean|max|min|count, header: true}
                  Without columns: one column per label and one per query. With join or several queries,
                  one row per key (the join label or the remaining labels) and one column per query.
        content: text/markdown (type: text)
        rename: [[regex, replacement]]            renames series (Grafana's renameByRegex)
        queries:
          - expr: PromQL ($__rate_interval, $__interval and $__range are resolved by the server)
            legend: "{{label}} …"                  legendFormat
            ref: A                                 query name (table columns, bars)
            instant: true                          an instant query (stat, tables…)
            minStep: 1m
            hide: true
        series:                                    Grafana overrides (the useful subset)
          - match: <exact name> | /regex/ | [name, name]
            name, color, unit, decimals, min, max, negative: true, fill, lineWidth, dash: true,
            draw: bars|line|points, stack: normal|percent|none, hidden: viz|legend|true, mappings, thresholds
```

Units are Grafana's: `none`, `short`, `percent`, `percentunit`, `bytes`,
`decbytes`, `bps`, `Bps`, `reqps`, `ops`, `s`, `ms`, `dtdurations`, `volt`,
`amp`, `watt`, `kwatt`, `watth`, `kwatth`, `hertz`, `rotrpm`, `celsius`,
`bool_yes_no` and a few more. Unknown units are written after the number.

## Builder dashboards

A second, older format is still supported: top-level `widgets` with a
Cloud Monitoring-style `query` (metric, filter, aligner, reducer, group-by) or
`queries` + `formulas`, and a `width`. It is the only way to use the DDSketch
percentiles (`ALIGN_PERCENTILE_99`) from a dashboard. `hosts.yaml` in the
examples is one; see [querying](querying.md#the-timeseries-api).

## Tips

- Long ranges read the aggregated tiers: prefer `$__rate_interval` over a fixed
  `[1m]` window so the window is never shorter than two buckets.
- Metrics sampled rarely (SMART is read once an hour) need
  `last_over_time(metric[2h])` to show a value at every step.
