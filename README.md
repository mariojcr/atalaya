# Atalaya

Metrics, logs, dashboards and alerts on top of ClickHouse, in two small Rust
binaries and a plain-JavaScript UI: the server, which answers queries and
serves the UI, and the collector, which runs on every host.

- **Store everything for long:** raw points for 7 days, 1-minute aggregates for
  6 weeks and 10-minute aggregates (min, max, sum, count and a DDSketch for
  percentiles) forever. Logs are kept forever too.
- **Query it like you already do:** a PromQL subset with the Prometheus HTTP API,
  so Grafana-style dashboards keep their queries, plus a Cloud Monitoring-style
  `timeSeries` API with aligners and reducers.
- **One collector per host:** scrapes Prometheus endpoints, reads the host's
  hardware (temperatures, fans, CPU energy, GPU, battery, ECC, SMART disk health)
  and chrony's time sync, and tails container logs, with an on-disk queue so
  nothing is lost while ClickHouse is down. Works with Nomad (service discovery
  and alloc logs) or without it (static targets and Docker logs).
- **A UI for day-to-day use:** a status home, YAML dashboards, a metrics
  explorer, a Discover-style logs explorer, a timeline of events, notebooks for
  investigations and alerting with silences. English and Spanish, light and dark.

## Quick start

```sh
cd examples/compose
for u in ADMIN READER COLLECTOR; do echo "CLICKHOUSE_${u}_PASSWORD=$(openssl rand -hex 16)"; done > .env
docker compose up -d
```

Open http://localhost:8080 (the `.env` line gives each ClickHouse user its own
password). Images (amd64) are published with every
[release](https://github.com/mariojcr/atalaya/releases):
`ghcr.io/mariojcr/atalaya-server` and `ghcr.io/mariojcr/atalaya-collector`.
The example runs ClickHouse, the schema setup, the
server, a collector and node-exporter on one host, with example dashboards and
alert rules. See [examples/compose](examples/compose/README.md).

## Documentation

| Guide | What it covers |
|---|---|
| [Architecture](docs/architecture.md) | Components, data model, storage tiers and how a query is answered |
| [Getting started](docs/getting-started.md) | Running the example, adding hosts and targets, building from source |
| [Configuration](docs/configuration.md) | Server environment, data files (dashboards, home, timeline) |
| [Collector](docs/collector.md) | Scraping, Nomad discovery, logs, host and disk metrics, metric rules, delivery guarantees |
| [Dashboards](docs/dashboards.md) | The YAML dashboard format |
| [Alerts](docs/alerts.md) | Rules, channels, silences and the alert API |
| [Querying](docs/querying.md) | Supported PromQL, the timeSeries API, the logs API and how aggregates affect results |
| [Deployment](docs/deployment.md) | Docker Compose, Nomad, binaries with systemd, security, health checks, upgrades and backups |
| [Releasing](docs/releasing.md) | Versioning, Commitizen, release artifacts and image tags |

## Repository layout

```
server/src/          server (binary `atalaya-server`) and its library (ClickHouse client, PromQL)
server/static/       the UI, embedded at build time
server/schema/       the ClickHouse schema (`atalaya-server schema` prints it)
collector/src/       collector (binary `atalaya-collector`)
examples/compose/    a single-host setup with example dashboards and alerts
examples/nomad/      the same on a Nomad cluster
docs/                the guides above
Dockerfile           both images (`--target server`, `--target collector`)
```

## Status

Atalaya runs a small homelab every day. It is young: the PromQL support covers
what dashboards commonly use, not the whole language. Sign-in is optional
(OpenID Connect, see [configuration](docs/configuration.md#sign-in)); without
it, keep the server on a private network or behind an authenticating proxy.

## License

Atalaya is licensed under the [Apache License 2.0](LICENSE). The web interface
embeds [uPlot](https://github.com/leeoniya/uPlot) (MIT), whose license travels at the top of its
files in `server/static/`.

Bug reports and pull requests are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md)
and, for vulnerabilities, [SECURITY.md](SECURITY.md).
