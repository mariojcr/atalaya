# SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
# SPDX-License-Identifier: Apache-2.0
# The atalaya server and UI. Dashboards, the home page, the timeline and alert
# rules are YAML files rendered into the task from ../data (paths relative to
# where you run `nomad job run`). Notebooks and alert state go to the host
# volume "atalaya", which the image's user (uid and gid 10001) must be able to
# write (see README.md). It reads ClickHouse as the read-only user reader:
#
#   nomad var put nomad/jobs/atalaya password=<reader's password>
#   nomad job run -var data_dir=../data atalaya.nomad.hcl

variable "version" {
  type    = string
  default = "latest"
}

variable "data_dir" {
  description = "Directory with dashboards/, overview.yaml, timeline.yaml and alerts.yaml"
  type        = string
  default     = "../data"
}

variable "dashboards" {
  description = "Dashboard files inside data_dir/dashboards"
  type        = list(string)
  default     = ["atalaya.yaml", "hardware.yaml", "hosts.yaml", "node-exporter-full.yaml"]
}

locals {
  # Path inside the task → file on the machine that submits the job
  files = merge(
    { for f in ["overview.yaml", "timeline.yaml", "alerts.yaml"] : f => file("${var.data_dir}/${f}") },
    { for f in var.dashboards : "dashboards/${f}" => file("${var.data_dir}/dashboards/${f}") },
  )
}

job "atalaya" {
  type = "service"

  group "atalaya" {
    network {
      port "http" { to = 8080 }
    }

    volume "data" {
      type   = "host"
      source = "atalaya"
    }

    service {
      name     = "atalaya"
      provider = "nomad"
      port     = "http"
      check {
        type     = "http"
        path     = "/readyz"
        interval = "15s"
        timeout  = "5s"
      }
    }

    task "atalaya" {
      driver = "docker"

      config {
        image = "ghcr.io/mariojcr/atalaya-server:${var.version}"
        ports = ["http"]
      }

      template {
        destination = "secrets/clickhouse.env"
        env         = true
        data        = <<EOH
CLICKHOUSE_URL=http://{{ range nomadService "clickhouse" }}{{ .Address }}:{{ .Port }}{{ end }}/
CLICKHOUSE_USER=reader
CLICKHOUSE_PASSWORD={{ with nomadVar "nomad/jobs/atalaya" }}{{ .password }}{{ end }}
EOH
      }

      # The files use {{…}} for legends and alert summaries: other delimiters
      dynamic "template" {
        for_each = local.files
        content {
          destination     = "local/${template.key}"
          data            = template.value
          left_delimiter  = "[%"
          right_delimiter = "%]"
          change_mode     = "noop"
        }
      }

      env {
        ATALAYA_DASHBOARDS = "/local/dashboards"
      }

      # The image's ATALAYA_DATA
      volume_mount {
        volume      = "data"
        destination = "/var/lib/atalaya"
      }

      resources {
        cpu        = 200
        memory     = 256
        memory_max = 1024
      }
    }
  }
}
