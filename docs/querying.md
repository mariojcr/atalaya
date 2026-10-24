# Querying

## PromQL

The server implements the Prometheus HTTP API used by dashboards:
`/api/v1/query`, `/api/v1/query_range`, `/api/v1/series`,
`/api/v1/label/{name}/values`, plus `POST /api/v1/query_range:batch` to run many
range queries in one request.

Supported:

- vector and range selectors with all matchers, `offset` and `@`;
- arithmetic, comparison (with `bool`) and set operators (`and`, `or`,
  `unless`), with `on`/`ignoring` and `group_left`/`group_right`;
- aggregations with `by`/`without`: `sum`, `avg`, `min`, `max`, `count`,
  `group`, `stddev`, `stdvar`, `topk`, `bottomk`, `quantile`, `count_values`;
- range functions: `rate`, `irate`, `increase`, `delta`, `idelta`, `deriv`,
  `predict_linear`, `changes`, `resets` and every `*_over_time`
  (`avg`, `min`, `max`, `sum`, `count`, `last`, `quantile`, `stddev`, `stdvar`,
  `present`, `absent`);
- `histogram_quantile`, `label_replace`, `label_join`, `absent`, `scalar`,
  `vector`, `time`, `timestamp`, `sort`/`sort_desc`/`sort_by_label`/
  `sort_by_label_desc` (label values compare as plain strings), `clamp*`,
  `abs`, `ceil`, `floor`, `round`, `exp`, `ln`, `sqrt`, `sgn`, `pi` and the
  date functions (`hour`, `day_of_week`, `month`, …).

Grafana's `$__rate_interval`, `$__interval` and `$__range` are resolved by the
server.

Not supported: subqueries (`expr[1h:5m]`).

Labels: the service is `job`, the collector's `node` is `instance`, and a target
found in Nomad adds `nomad_service`, `nomad_group` and `nomad_alloc_index` (so the
replicas of a job are separate series). Metric labels come through, except that
`job` and `instance` from the exporter become `exported_job` and
`exported_instance`, and labels with empty values are dropped. Ambiguous
one-to-one matches are an error, as in Prometheus.

### Aggregated tiers and approximations

Queries read the coarsest tier that keeps the step's resolution (see
[architecture](architecture.md#tiers)). With long ranges this is deliberate
approximation, not sample-exact Prometheus:

- A bucket keeps min, max, sum, count and the time of its last sample. A gauge
  reads as the bucket mean and a counter as its maximum; `max_over_time` over a
  tier therefore sees bucket values, not every raw sample, so short peaks can
  flatten in gauges.
- Short windows are widened to two buckets so that `rate(x[1m])` on a 10-minute
  tier still returns something.
- Counter resets inside a bucket are detected from the bucket's minimum.

For exact results over a long time, query a shorter range (raw data is kept for
7 days).

## The timeSeries API

`POST /api/v1/timeSeries:query` follows Cloud Monitoring's model:

```json
{
  "metric": "node_load1",
  "filter": { "resource.host.name": "host-1" },
  "interval": { "start": 1790000000, "end": 1790003600 },
  "aggregation": {
    "alignmentPeriod": "60s",
    "perSeriesAligner": "ALIGN_PERCENTILE_99",
    "crossSeriesReducer": "REDUCE_MAX",
    "groupByFields": ["resource.host.name"]
  }
}
```

Aligners: `ALIGN_MEAN`, `ALIGN_MIN`, `ALIGN_MAX`, `ALIGN_SUM`, `ALIGN_COUNT`,
`ALIGN_RATE`, `ALIGN_DELTA`, `ALIGN_PERCENTILE_{50,95,99}` (from the DDSketch in
the aggregated tiers). Reducers: `REDUCE_NONE`, `REDUCE_SUM`, `REDUCE_MEAN`,
`REDUCE_MIN`, `REDUCE_MAX`, `REDUCE_COUNT`, `REDUCE_PERCENTILE_{50,95,99}`.
`interval` also accepts `{"range": "6h"}` instead of `start`/`end`. Every response includes the SQL it ran. The metrics
explorer's builder mode uses this API.

Counters here are deltas per bucket (`max(last - previous, 0)`), not PromQL's
extrapolated `rate`, so the two can differ slightly on the same data.

## Logs

| Method and path | What |
|---|---|
| `GET /api/v1/logs` | Lines and a histogram for a range and filters |
| `GET /api/v1/logs/fields` | Fields present, with counts and top values |
| `GET /api/v1/logs/values?field=…` | Top values of a field (autocomplete) |
| `GET /api/v1/logs/counts` | Line counts per group of fields, as series (dashboards) |
| `GET /api/v1/logs/context` | Lines around one line |
| `GET/PUT/DELETE /api/v1/searches[/{id}]` | Saved searches |

Common parameters: `range=1h` or `start`/`end`, `service`, `host`, `level`,
`q` (case-insensitive text), `limit`, `breakdown=<field>` for the histogram,
`docs_only=true` for the lines alone, and any number of `f=` filters:

```
field:value      equal             -field:value     not equal
field:*          exists            -field:*         does not exist
field>n  field<n  field>=n  field<=n   numeric
field~text       contains (case-insensitive)   -field~text  does not contain
field=~regex     matches the whole value (RE2)  -field=~regex  does not match
```

`/api/v1/logs/counts` counts the lines matching `start`/`end` and the `f=`
filters. Its answer is `{"series": [{"labels": {…}, "points": [[t, n], …]}]}`:

- `by=<field>`: groups by a field; repeat it for up to 3 fields. The labels are
  named after the fields.
- `top=10`: keeps that many groups, the most frequent in the range (up to 100).
- `step=<seconds>`: a point per bucket, with 0 for buckets without lines.
  Without it, a single count for the whole range, at `end`.

Fields are `service`, `host`, `level`, `stream`, `body` and any key of the JSON
attributes or resource labels (`nomad.job`, `container.name`, …).

## Other endpoints

`/api/v1/metricDescriptors` (metrics with kind, unit and active series),
`/api/v1/labels`, `/api/v1/overview` (home page), `/api/v1/events` (timeline),
`/api/v1/dashboards[/{name}]`, `/api/v1/notebooks[/{id}]`, `/healthz`,
`/readyz`.
