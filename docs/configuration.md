# Configuration

## Server environment

| Variable | Default | Meaning |
|---|---|---|
| `ATALAYA_LISTEN` | `0.0.0.0:8080` | Listen address |
| `CLICKHOUSE_URL` | `http://127.0.0.1:8123/` | ClickHouse HTTP endpoint |
| `CLICKHOUSE_USER` | `reader` | A read-only user is enough for the server |
| `CLICKHOUSE_PASSWORD` | empty | |
| `ATALAYA_DB` | `atalaya` | Database for queries and for `atalaya-server schema` |
| `ATALAYA_REPLICATED` | unset | `1`: `atalaya-server schema` writes replicated tables for a ClickHouse cluster (see [deployment](deployment.md#replicated-clickhouse)) |
| `ATALAYA_DASHBOARDS` | `dashboards` | Directory of dashboard YAML files |
| `ATALAYA_OVERVIEW` | next to the dashboards directory | `overview.yaml` (home page) |
| `ATALAYA_TIMELINE` | same | `timeline.yaml` |
| `ATALAYA_ALERTS` | same | `alerts.yaml` |
| `ATALAYA_DATA` | `data` (`/var/lib/atalaya` in the image) | Notebooks, saved searches, alert state, history and outbox |
| `ATALAYA_PUBLIC_URL` | unset | UI link added to notifications; with sign-in, the base of the callback URL |
| `ATALAYA_ALLOWED_HOSTS` | unset | Without sign-in, other names the server answers to (comma-separated; see below) |
| `ATALAYA_OIDC_ISSUER` | unset | OpenID Connect issuer: sign-in required for everything (see below) |
| `ATALAYA_OIDC_CLIENT_ID` | `atalaya` | Client at the provider |
| `ATALAYA_OIDC_INTERNAL` | the issuer | Where the server reaches the provider, if not at the issuer URL |
| `ATALAYA_SESSION_KEY` | unset | Secret that signs the session cookie, 32+ characters (needed with sign-in) |
| `ATALAYA_SESSION_DAYS` | `30` | Session length |
| `ATALAYA_NOW_DELAY` | `45` | Seconds panels, alerts and the home page stay behind now: the time for data to arrive. 45 fits the collector's default 30 s `flush_interval`; with 15 s, 30 is enough |
| `ATALAYA_REFRESH` | `60` | Seconds between automatic refreshes of the pages that are not dashboards (each dashboard has its own `refresh`) |

Next to the dashboards directory means: with `ATALAYA_DASHBOARDS=/etc/atalaya/dashboards`
the server reads `/etc/atalaya/overview.yaml`, `/etc/atalaya/timeline.yaml` and
`/etc/atalaya/alerts.yaml`.

The server re-reads dashboards on every request, the home page and timeline
files on every refresh, and alert rules on every evaluation. Invalid alert rules
keep the previous ones and show the error on the alerts page.

`atalaya-server schema` prints the schema for `ATALAYA_DB`; `atalaya-server schema --apply`
applies it (idempotent) with `CLICKHOUSE_USER`, which then needs permission to
create databases and tables.

## Sign-in

With `ATALAYA_OIDC_ISSUER` set, every page and API call needs a session
(`/healthz`, `/readyz` and `/auth/*` excepted): pages send the browser to the
provider, API calls get 401 and the UI goes through the sign-in back to the
same page. Register at the provider a **public client with PKCE** (no secret)
whose callback URL is `<ATALAYA_PUBLIC_URL>/auth/callback` and whose logout
URL is `<ATALAYA_PUBLIC_URL>/`; who may sign in is decided there (for example,
the client's allowed groups in Pocket ID). The session is a cookie signed with
`ATALAYA_SESSION_KEY`: nothing is stored on the server, and changing the key
signs everyone out. `/auth/logout` ends the session here and at the provider.

Without sign-in the server answers only to IP addresses, `localhost`, the host
of `ATALAYA_PUBLIC_URL` and the names in `ATALAYA_ALLOWED_HOSTS`; other names
get 403 (`/healthz` and `/readyz` excepted). Otherwise a page on another site
could point its own name at the server's address (DNS rebinding) and read
everything through its visitor's browser. Behind a proxy, or when you reach the
server by a name, put that name in one of the two variables.

## Home page: `overview.yaml`

The home page shows one tile per service and host. A service is a scrape job;
a host is a collector's `node`.

```yaml
node_job: node-exporter        # job that provides CPU, memory and network per host
ignore_tasks: []               # Nomad tasks whose logs do not count (sidecars)

defaults:                      # applied to every service
  spark: '…'                   # PromQL for the small chart ($job and $host are substituted)
  spark_unit: percent          # percent, bytes, watt, short…
services:
  node-exporter:
    status:                    # pieces of the status line, joined with " · "
      - { text: { en: "load {{v}}", es: "carga {{v}}" }, query: 'node_load1{instance="$host"}', decimals: 2 }
      - { text: "{{v}} uptime", query: 'node_time_seconds - node_boot_time_seconds', unit: secs }
    warn: 'node_load1{instance="$host"} > bool 8'   # > 0 turns the tile amber
  atalaya-collector:
    hosts_from: atalaya_collector_points_total      # services without `up`: hosts from this metric
```

`{{v}}` is the query's value; `unit: bytes | secs` formats it. Texts can be a
single string or `{en, es}`.

## Timeline: `timeline.yaml`

```yaml
lanes:                         # metric lanes under the events
  - { title: { en: "CPU (%)", es: "CPU (%)" }, query: '100 * (1 - avg(rate(node_cpu_seconds_total{mode="idle"}[5m])))', unit: percent }
log_lanes: [my-app, nginx]     # services with a log lane; empty = the six busiest
ignore_tasks: []               # Nomad tasks left out of allocs, restarts and logs
```

The timeline also shows Nomad alloc starts, stops and restarts (from the Nomad
client metrics, when present), log error bursts and alert transitions.

## Dashboards and alerts

See [dashboards](dashboards.md) and [alerts](alerts.md). The collector has its
own configuration file: see [the collector](collector.md).
