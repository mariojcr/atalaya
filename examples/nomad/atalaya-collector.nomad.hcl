# SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
# SPDX-License-Identifier: Apache-2.0
# atalaya-collector on every client (system job). It discovers the services of
# its node tagged `metrics=true` (path from a `metrics_path=/…` tag, /metrics by
# default), tails the logs of the node's allocs and reads the host's hardware.
#
# Requirements on each client:
#   - the docker plugin with `allow_privileged = true` (SMART needs raw disk
#     access; set host.smart.enabled: false and privileged = false otherwise)
#     and `volumes { enabled = true }`
#   - the Nomad data dir at /var/lib/nomad (change alloc_dir below if not)
#
#   nomad var put nomad/jobs/atalaya-collector password=<collector's password>
#   nomad job run atalaya-collector.nomad.hcl

variable "version" {
  type    = string
  default = "latest"
}

job "atalaya-collector" {
  type      = "system"
  node_pool = "all"
  priority  = 100

  group "collector" {
    network {
      mode = "host"
    }

    task "collector" {
      driver = "docker"

      config {
        image        = "ghcr.io/mariojcr/atalaya-collector:${var.version}"
        network_mode = "host"
        privileged   = true
        args         = ["/local/collector.yaml"]
        volumes = [
          "/:/host:ro",                                   # sysfs: sensors, RAPL, GPU, battery
          "/var/lib/nomad/alloc:/var/lib/nomad/alloc:ro", # alloc logs
          "atalaya-collector:/state",                     # log positions and disk queue
        ]
      }

      template {
        destination = "local/collector.yaml"
        data        = <<EOH
node: {{ env "attr.unique.hostname" }}
state_dir: /state

clickhouse:
  url: http://{{ range nomadService "clickhouse" }}{{ .Address }}:{{ .Port }}{{ end }}/
  user: collector
  database: atalaya

nomad:
  addr: http://{{ env "attr.unique.network.ip-address" }}:4646
  node_id: {{ env "node.unique.id" }}

scrape:
  interval: 15s

logs:
  alloc_dir: /var/lib/nomad/alloc

host:
  root: /host
  smart:
    enabled: true
    interval: 1h
EOH
      }

      template {
        destination = "secrets/clickhouse.env"
        env         = true
        data        = <<EOH
CLICKHOUSE_PASSWORD={{ with nomadVar "nomad/jobs/atalaya-collector" }}{{ .password }}{{ end }}
EOH
      }

      resources {
        cpu        = 100
        memory     = 64
        memory_max = 256
      }
    }
  }
}
