# atalaya with Docker Compose

Everything on one host: ClickHouse, the schema, the server, the collector and
node-exporter. No Nomad needed.

```sh
for u in ADMIN READER COLLECTOR; do echo "CLICKHOUSE_${u}_PASSWORD=$(openssl rand -hex 16)"; done > .env
docker compose up -d
```

Open http://localhost:8080. The first data shows up within about 30 s; longer
charts fill in over time.

## What runs

| Service | Purpose |
|---|---|
| `clickhouse` | Stores metrics and logs (volume `clickhouse`) |
| `schema` | Creates the database and tables (`atalaya-server schema --apply`), then exits |
| `atalaya` | Server and UI on port 8080 of this machine only |
| `collector` | Scrapes node-exporter, reads the host's hardware (sysfs at `/host`) and the logs of the Docker containers |
| `node-exporter` | Operating system metrics |

## ClickHouse users

Each job has its own user and password (from `.env`; Compose stops if one is
missing):

| User | Used by | Can |
|---|---|---|
| `admin` | `schema` | Everything: the image creates it from `CLICKHOUSE_ADMIN_PASSWORD` |
| `reader` | `atalaya` | Read `atalaya.*`, nothing else (`clickhouse-users.xml`) |
| `collector` | `collector` | Insert into `atalaya.*` and read it, which the materialized views behind the aggregate tiers need (`clickhouse-users.xml`) |

To change a password, edit `.env` and run `docker compose up -d` again.

## What to change

- **`collector.yaml`:** the host name (`node`), the targets to scrape
  (`scrape.static_targets`: a service name and a Prometheus `/metrics` URL) and
  whether to read disk health (SMART, which needs `privileged: true` in the
  compose file).
- **`../data/`:** dashboards (`dashboards/*.yaml`), the home page
  (`overview.yaml`), the timeline (`timeline.yaml`) and alert rules
  (`alerts.yaml`). The server re-reads them: no restart needed to see a changed
  dashboard.
- **Variables** (in `.env`): the three passwords, `ATALAYA_PORT` (8080),
  `ATALAYA_VERSION` (image tag, `latest`) and `DOCKER_CONTAINERS_DIR` if Docker
  keeps containers somewhere else.

## More hosts

One collector per host, all writing to the same ClickHouse: the same
`collector.yaml` with another `node` and a ClickHouse URL reachable from that
host (publish port 8123 of `clickhouse`, ideally only on a private network).
Each one shows up on the home page under its name.

## No authentication

The example runs without sign-in: anyone who reaches the port can read the data
and edit notebooks and silences. That is why it listens on `127.0.0.1` only. To
reach it from other machines, change the `ports` line of `atalaya` and put it on
a private network, behind an authenticating proxy, or set up
[sign-in](../../docs/configuration.md#sign-in).

## Upgrading from an earlier version of this example

Earlier versions used one ClickHouse user (`atalaya`, password
`CLICKHOUSE_PASSWORD`) for everything and ran the server as root. Create the
`.env` above, then give the server's volume to the image's user (uid 10001):

```sh
docker compose run --rm --no-deps --user 0 --entrypoint chown atalaya -R 10001:10001 /var/lib/atalaya
docker compose up -d
```
