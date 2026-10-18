# Collector

`atalaya-collector <config.yaml>` runs once per host. The ClickHouse password
comes from the `CLICKHOUSE_PASSWORD` environment variable (unset means empty),
never from the file. The log level follows `RUST_LOG` (default
`warn,atalaya_collector=info`).

## Configuration

```yaml
node: host-1                   # required: the host name (`instance` in PromQL)
state_dir: /state              # required: log positions and the disk queue

clickhouse:
  url: http://clickhouse:8123/ # required
  user: collector              # required: INSERT and SELECT on atalaya.* (docs/deployment.md#clickhouse)
  database: atalaya            # default atalaya
  flush_interval: 30s          # how often rows are sent (default 30s)
  max_spool_mb: 1024           # disk queue limit; the oldest batches go first (default 1024)

nomad:                         # optional: service discovery and allocation logs
  addr: http://10.0.0.5:4646   # https://… when the cluster uses TLS
  node_id: 9b1c…               # this client's node ID
  ca_file: /etc/nomad.d/tls/ca.pem   # optional: the cluster's CA (also trusted by the scrapes)
  token_file: /secrets/nomad-token   # optional: ACL token with read-job; read once at start

scrape:
  interval: 15s                # default 15s
  static_targets:              # endpoints that are not discovered through Nomad
    - { service: node-exporter, url: http://localhost:9100/metrics }

logs:
  enabled: true                # default true
  alloc_dir: /var/lib/nomad/alloc          # Nomad allocation logs (only with `nomad`)
  docker_dir: /var/lib/docker/containers   # Docker json-file logs (optional)

host:
  enabled: true                # default true: hardware metrics from sysfs
  root: /host                  # where the host's / is mounted; <root>/sys is read (default /host)
  smart:
    enabled: true              # default true
    interval: 1h               # default 1h
    smartctl: smartctl         # binary (it ships in the image)

syslog:                        # optional: receive syslog
  listen: 0.0.0.0:5514         # UDP and TCP on the same address (default)
  service: syslog              # service of the received lines (default syslog)

rules: {}                      # metric rules, see below
```

Durations take `s`, `m` or `h` (a bare number is seconds) and must be greater
than zero. An unknown key is an error, so a misspelt setting does not silently
keep its default.

On a host without containers, `host.root` is `/`.

## Privileges

The collector runs as root, or as a privileged container with the host's `/`
mounted at `/host`, because several sources are root-only:

- RAPL energy counters (`energy_uj`) on current kernels;
- SMART: `smartctl` opens the disks under the collector's own `/dev`, so in a
  container it needs the host's devices (`privileged: true`);
- Docker's log directory.

Without those permissions the affected metrics or logs are simply missing.

Running as root, it does not let the jobs it watches steer it:

- **Nomad logs:** `alloc/` is writable by the tasks. The `logs` directory is
  opened without following a symlink, each file is looked up through that open
  directory, and only regular files with a single link are read. A task cannot
  get another file of the host into its logs, through a symlink, a hard link or
  by swapping its `logs` directory.
- **Nomad targets:** a service registration's address is the job's to choose,
  so a target is only scraped at an address of its own allocation (see
  [Sources](#sources)), and a redirect in its answer is not followed.
- **Answers:** a scrape over 32 MiB fails instead of filling the collector's
  memory.

## Sources

**Prometheus endpoints.** Each static target, and with `nomad` every service of
this node tagged `metrics=true`. The path is `/metrics` unless a
`metrics_path=/other` tag says otherwise. Scrapes time out after 5 s, an answer
over 32 MiB fails the scrape, a redirect is not followed (the scrape fails),
and static targets take no authentication. Every
target also gets Prometheus-style `up` and `scrape_duration_seconds`. Histograms
are stored as distributions; summaries as their quantiles plus `_sum`/`_count`.
Samples whose metric name is not `[a-zA-Z_:][a-zA-Z0-9_:]*` are dropped, with a
warning per target at most once an hour.

What Nomad discovery covers:

- services registered with Nomad's own provider (`provider = "nomad"`), not
  Consul;
- the `default` namespace;
- plain `http://` endpoints;
- addresses of the service's own allocation: its address in its group network
  (`address_mode = "alloc"`, any port) or a host address and port the
  allocation was given (`address_mode = "host"` or `"auto"`). A registration
  pointing anywhere else (a custom `address`, another allocation's port, a
  `driver` address) is not scraped, and the collector says so once in its log.
  Reading the allocation needs `read-job` on the namespace.

The service of a discovered target (`job` in PromQL) is its Nomad job ID. The
series also record the Nomad service, task group and allocation index
(`nomad_service`, `nomad_group` and `nomad_alloc_index` in PromQL), so the
replicas and services of one job are separate series.

**Logs.**

- **Nomad:** with `nomad`, the stdout and stderr files of every allocation on
  the node. The service is the job, and the job, group, task, allocation and
  namespace go in the resource.
- **Docker:** with `docker_dir`, the json-file logs of every container. The
  service is the Compose service label, or the container name.
- **Both at once** when both are configured.

The first run starts at the end of the existing files. The level is taken from
JSON logs (`level`, `severity`, …) or from the text, and JSON logs keep their
keys as attributes. Bodies are stored up to 64 KiB.

Over-long lines:

- **Nomad:** a line longer than 4 MiB is cut and the rest skipped, counted in
  `atalaya_collector_log_lines_truncated_total`.
- **Docker:** a line the json-file driver split into chunks is joined up to
  64 KiB, and a single entry of 4 MiB or more is dropped.

Log times are written without a time zone, so ClickHouse must run in UTC (the
default of the official image).

**Syslog.** With `syslog`, lines over UDP, or over TCP one per line (no octet
counting, no TLS):

- **Headers:** RFC 3164 and RFC 5424.
- **Bodies:** CEF and `KEY=value` bodies (netfilter's `IN= OUT= SRC= …`) also
  become attributes.
- **Host:** the header's hostname, or the sender's IP. Anyone can claim any
  hostname or severity; the address the message really came from is always in
  the resource, as `source.ip`.
- **Time:** when the line arrived.
- **TCP limits:** a line over 64 KiB, or 15 minutes without one, closes the
  connection, and at most 256 are open at once, 16 of them from one address
  (one /64 in IPv6); more are refused until one closes. UDP datagrams are 64 KiB
  at most anyway.

There is no authentication: listen only where the senders are trusted.

**Host metrics** (service `host`), from sysfs:

| Metric | What |
|---|---|
| `host_temperature_celsius` | Every hwmon temperature: CPU package and cores, NVMe, memory modules, chipset, NIC, GPU |
| `host_fan_rpm`, `host_voltage_volts`, `host_current_amperes` | Fans and electrical sensors |
| `host_power_watts`, `host_frequency_hertz`, `host_energy_joules_total` | Power, clocks and energy reported by sensors (e.g. the GPU) |
| `host_rapl_energy_joules_total{zone}` | CPU energy per RAPL domain; `rate()` gives watts |
| `host_gpu_busy_ratio`, `host_gpu_memory_{used,total}_bytes{pool}` | AMD GPU (amdgpu) load and VRAM/GTT memory |
| `host_power_supply_online`, `host_battery_*` | Mains and USB-C adapters, and batteries |
| `host_memory_ecc_errors_total{type}` | Corrected and uncorrected ECC errors (EDAC) |

Some probes are skipped:

- **Temperatures at or beyond -40 °C or 127 °C:** the "no sensor" values of
  several chips.
- **Temperature, fan, voltage and current sensors that have only read 0:**
  skipped until they show a signal.

**Disk health** (service `host`), from `smartctl --json` once per interval,
never waking a sleeping disk.

`host_disk_standby` and `host_disk_smart_up` carry `device`. Every other disk
series also carries `model`, `serial` and `kind` (hdd, ssd or nvme):

- `host_disk_smart_healthy` and `host_disk_temperature_celsius`;
- power-on time and cycles, capacity, and `host_disk_endurance_used_ratio`;
- ATA attributes (`host_disk_ata_attribute_{raw,value,threshold}`);
- the NVMe health log (`host_nvme_*`): critical warning, spare, bytes read and
  written, media errors, error log entries, unsafe shutdowns and time over
  temperature.

**Self metrics** (service `atalaya-collector`):

- `atalaya_collector_points_total`, `_log_lines_total`, `_syslog_lines_total`
  and `_log_lines_truncated_total`;
- `_scrape_errors_total`;
- `_rows_sent_total`, `_batches_sent_total` and `_send_errors_total`;
- `_spooled_batches_total`, `_spool_bytes`, `_dropped_batches_total` and
  `_dropped_rows_total`.

## Metric rules

Rules run on every scrape before anything is stored:

```yaml
rules:
  allow:                       # per service: keep only matching metrics
    - service: clickhouse
      keep: ['ClickHouseMetrics_(Query|Merge).*', 'ClickHouseAsyncMetrics_Disk.*']
  drop: ['go_gc_.*']           # never store these metrics
  drop_series:                 # drop one label value of a metric
    - { metric: node_systemd_unit_state, label: state, value: '(activating|deactivating|inactive)' }
  aggregate:                   # sum away labels (per core → per host)
    - { metric: node_cpu_seconds_total, without: [cpu] }
  slow:                        # send these less often
    interval: 5m
    metrics: ['node_filesystem_size_bytes']
```

Expressions are anchored regular expressions, and histograms match by their
base name. A service without an `allow` entry is not filtered, and `up` and
`scrape_duration_seconds` are added after the rules.

## Delivery

Rows are kept in memory per table and sent, gzip-compressed, every
`flush_interval`.

- **Sending fails:** a network error, a 5xx, a timeout (408), a rate limit (429),
  or ClickHouse refusing the credentials or not knowing the database or table
  (401, 403, 404). The batch goes to the disk queue, which is retried in order
  before anything new.
- **The queue passes `max_spool_mb`:** the oldest batches are dropped and
  counted.
- **ClickHouse rejects a batch for its content** (any other 4xx): a new batch is
  dropped and counted, and a queued one is moved to `state_dir/spool/rejected/`
  for inspection. That directory is not counted in `max_spool_mb`, and nothing
  cleans it.

The guarantees differ by source:

- **Nomad and Docker logs** are at-least-once. A file position is saved only
  after the lines read up to it are in ClickHouse or in the disk queue. A crash
  re-reads from the last saved position, so a batch may arrive twice.
- **Metrics, SMART, self metrics and syslog** are best-effort. They stay in
  memory until the next flush, so a crash or a `SIGKILL` loses up to one
  `flush_interval`. If sending stalls, memory is capped at 500,000 rows per
  table and the oldest are dropped and counted.

On `SIGTERM` the collector stops reading, finishes the send in flight and sends
what is pending. Each send times out after 60 s.

The ClickHouse client trusts the standard public certificate authorities, so an
`https://` URL with a private CA is not supported.
