## v0.3.0 (2026-10-30)

### Feat

- **server**: a replicated schema for a ClickHouse cluster (ATALAYA_REPLICATED=1) (#20)

### Fix

- **server**: named colors in time series overrides (#17)

## v0.2.0 (2026-10-25)

### Feat

- **server**: stat panels show the change against an earlier period (#15)
- **server**: dashboard annotations from timeline events (#14)
- **server**: link groups of log panels to the logs explorer (#13)
- **server**: dashboard variables from log field values (#12)
- **server**: heatmap panels, query offset and per-panel range in dashboards (#11)
- mark series stale when they stop being written (#9)
- **collector**: flatten nested JSON log fields into dotted attributes (#8)
- **collector**: parse plain-text task logs with patterns from the job (#5)
- **server**: log panels in dashboards (#6)
- **collector**: read time synchronization from chronyd (#2)

### Fix

- **server**: expire staleness markers after 42 days (#10)
- **server**: refuse PromQL ranges outside the epoch or far ahead (#7)
- **collector**: read a starting allocation again until it runs (#4)

### Refactor

- **collector**: use Scrape::put in the host and SMART readers (#3)

## v0.1.0 (2026-10-19)

First public release: the collector and the server in one workspace.
