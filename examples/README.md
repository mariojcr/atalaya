# Examples

| Directory | What |
|---|---|
| [`compose/`](compose/README.md) | Everything on one host with Docker Compose |
| [`nomad/`](nomad/README.md) | A Nomad cluster: ClickHouse, the schema, the server and a collector per client |
| [`data/`](data) | Data files both use: dashboards, home page, timeline and alert rules |

The dashboards are generic: `node-exporter-full` (the classic Node Exporter Full
dashboard), `hardware` (sensors, power, GPU, battery and disk health from the
collector), `atalaya` (the collectors themselves) and `hosts` (a builder-format
example). The alert rules cover hosts and targets down, full disks, memory,
temperatures, ECC errors and disk health.
