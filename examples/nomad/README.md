# atalaya on Nomad

Four jobs, all with the Docker driver (Podman works the same with the podman
driver and its options):

| Job | Type | What it does |
|---|---|---|
| `clickhouse.nomad.hcl` | service | ClickHouse, data in the host volume `clickhouse` |
| `atalaya-schema.nomad.hcl` | batch | Creates or upgrades the database and tables, then exits |
| `atalaya.nomad.hcl` | service | Server and UI, data files rendered from `../data`, state in the host volume `atalaya` |
| `atalaya-collector.nomad.hcl` | system | One collector per client: Nomad service discovery, alloc logs, host and disk metrics |

## Before you start

- **Host volumes** `clickhouse` and `atalaya` on the clients that should run
  them (static `host_volume` blocks or dynamic host volumes). The server's
  volume pins it to one client, and its image runs as uid and gid 10001: give
  that user the directory (`chown 10001:10001`, or `uid` and `gid` parameters
  when the `mkdir` plugin creates a dynamic host volume).
- **Docker plugin** on every client with `allow_privileged = true` (the
  collector reads SMART from the disks) and `volumes { enabled = true }`.
- **Services to scrape:** tag them `metrics=true` and, if the endpoint is not
  `/metrics`, `metrics_path=/other`. The collector on each node scrapes the
  services of that node.

## Run

One ClickHouse user per job, each with its own password: `admin` applies the
schema, `reader` serves the UI (read-only) and `collector` inserts. The
ClickHouse job needs the three to set them up; every other job reads only its
own:

```sh
admin=$(openssl rand -hex 16) reader=$(openssl rand -hex 16) collector=$(openssl rand -hex 16)
nomad var put nomad/jobs/clickhouse admin_password=$admin reader_password=$reader collector_password=$collector
nomad var put nomad/jobs/atalaya-schema password=$admin
nomad var put nomad/jobs/atalaya password=$reader
nomad var put nomad/jobs/atalaya-collector password=$collector

nomad job run clickhouse.nomad.hcl
nomad job run atalaya-schema.nomad.hcl
nomad job run -var data_dir=../data atalaya.nomad.hcl
nomad job run atalaya-collector.nomad.hcl
```

`-var version=sha-…` pins the images to a build instead of `latest`.

The server listens on a dynamic port registered as the Nomad service
`atalaya`; put your reverse proxy in front of it, with authentication unless
you set up [sign-in](../../docs/configuration.md#sign-in).

## Changing dashboards or alerts

Edit the files in `../data` and run `nomad job run -var data_dir=../data
atalaya.nomad.hcl` again: the job embeds the files, so a change is a new job
version. Add new dashboard files to the `dashboards` variable.
