# SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
# SPDX-License-Identifier: Apache-2.0
# Creates (or upgrades) the atalaya database and tables, then exits. Safe to run
# again after every upgrade: all statements are idempotent. It runs as the
# ClickHouse admin, whose password goes in the job's own variable:
#
#   nomad var put nomad/jobs/atalaya-schema password=<the admin password>
#   nomad job run atalaya-schema.nomad.hcl

variable "version" {
  type    = string
  default = "latest"
}

job "atalaya-schema" {
  type = "batch"

  group "schema" {
    task "schema" {
      driver = "docker"

      config {
        image = "ghcr.io/mariojcr/atalaya-server:${var.version}"
        args  = ["schema", "--apply"]
      }

      template {
        destination = "secrets/clickhouse.env"
        env         = true
        data        = <<EOH
CLICKHOUSE_URL=http://{{ range nomadService "clickhouse" }}{{ .Address }}:{{ .Port }}{{ end }}/
CLICKHOUSE_USER=admin
CLICKHOUSE_PASSWORD={{ with nomadVar "nomad/jobs/atalaya-schema" }}{{ .password }}{{ end }}
EOH
      }

      resources {
        cpu    = 100
        memory = 64
      }
    }
  }
}
