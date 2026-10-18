# Getting started

## Run the example

```sh
cd examples/compose
for u in ADMIN READER COLLECTOR; do echo "CLICKHOUSE_${u}_PASSWORD=$(openssl rand -hex 16)"; done > .env
docker compose up -d
```

The `.env` file holds one password per ClickHouse user: `admin` applies the
schema, `reader` serves the UI and `collector` inserts (see
[examples/compose](../examples/compose/README.md#clickhouse-users)).

Open http://localhost:8080. In a minute or two the home page shows the host
and its node-exporter (the collector sends every 30 seconds and the server stays
`ATALAYA_NOW_DELAY`, 45 seconds, behind the clock); longer charts fill in as
data accumulates.

What started: ClickHouse, a one-shot `schema` container that creates the tables,
the server on port 8080 (of this machine only), a collector that scrapes node-exporter, reads the
host's hardware and tails Docker logs, and node-exporter itself.

## Add a scrape target

Any Prometheus endpoint works. Add it to `examples/compose/collector.yaml`:

```yaml
scrape:
  static_targets:
    - { service: node-exporter, url: http://node-exporter:9100/metrics }
    - { service: my-app, url: http://my-app:9000/metrics }
```

and restart the collector (`docker compose restart collector`). The service name
becomes the `job` label in PromQL and the tile name on the home page.

## Add a host

Run one collector per host, all writing to the same ClickHouse. Copy
`collector.yaml`, set `node` to the host's name and `clickhouse.url` to an
address reachable from that host. Each host shows up on the home page and in
the `instance` label.

## Add a dashboard

Drop a YAML file in `examples/data/dashboards/` (see
[dashboards](dashboards.md)). The server reads dashboards on every request: no
restart needed.

## Build from source

Rust is pinned in `rust-toolchain.toml`.

```sh
cargo build --release                 # target/release/atalaya-server and atalaya-collector
cargo test --release
```

The images build both binaries in a container, one target each:

```sh
docker build --target server -t atalaya-server .
docker build --target collector -t atalaya-collector .
```

Run the server against an existing ClickHouse while developing:

```sh
CLICKHOUSE_URL=http://localhost:8123/ CLICKHOUSE_USER=reader CLICKHOUSE_PASSWORD=… \
ATALAYA_DASHBOARDS=examples/data/dashboards \
ATALAYA_LISTEN=127.0.0.1:8090 cargo run --release --bin atalaya-server
```

The UI files are embedded at build time, so UI changes need a rebuild.
