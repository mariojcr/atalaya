# SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
# SPDX-License-Identifier: Apache-2.0
# ClickHouse for atalaya (single node). Data in the host volume "clickhouse".
# Three users, each with its own password in the Nomad variable
# nomad/jobs/clickhouse (see README.md):
#
#   admin      created by the image; applies the schema (atalaya-schema)
#   reader     the server: read-only (readonly 2: it may change query settings)
#   collector  the collectors: inserts into atalaya.*, plus SELECT, which the
#              materialized views behind the 1-minute and 10-minute tiers run with
#
#   nomad job run clickhouse.nomad.hcl

job "clickhouse" {
  type = "service"

  group "clickhouse" {
    network {
      port "http" { static = 8123 }
    }

    volume "data" {
      type   = "host"
      source = "clickhouse"
    }

    service {
      name     = "clickhouse"
      provider = "nomad"
      port     = "http"
      check {
        type     = "http"
        path     = "/ping"
        interval = "10s"
        timeout  = "2s"
      }
    }

    task "clickhouse" {
      driver = "docker"

      config {
        image = "docker.io/clickhouse/clickhouse-server:26.8"
        ports = ["http"]
        ulimit { nofile = "262144:262144" }
        # reader and collector, rendered below (a path inside the task directory)
        volumes = ["secrets/atalaya-users.xml:/etc/clickhouse-server/users.d/atalaya.xml:ro"]
      }

      template {
        destination = "secrets/clickhouse.env"
        env         = true
        data        = <<EOH
CLICKHOUSE_USER=admin
CLICKHOUSE_PASSWORD={{ with nomadVar "nomad/jobs/clickhouse" }}{{ .admin_password }}{{ end }}
CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1
EOH
      }

      # Only password hashes in the file; it is read at startup, so a new
      # password restarts the task
      template {
        destination = "secrets/atalaya-users.xml"
        data        = <<EOH
{{- with nomadVar "nomad/jobs/clickhouse" -}}
<clickhouse>
  <profiles>
    <reader>
      <readonly>2</readonly>
      <max_execution_time>120</max_execution_time>
    </reader>
  </profiles>
  <users>
    <reader>
      <password_sha256_hex>{{ printf "%s" .reader_password | sha256Hex }}</password_sha256_hex>
      <networks><ip>::/0</ip></networks>
      <profile>reader</profile>
      <quota>default</quota>
      <grants><query>GRANT SELECT ON atalaya.*</query></grants>
    </reader>
    <collector>
      <password_sha256_hex>{{ printf "%s" .collector_password | sha256Hex }}</password_sha256_hex>
      <networks><ip>::/0</ip></networks>
      <profile>default</profile>
      <quota>default</quota>
      <grants><query>GRANT SELECT, INSERT ON atalaya.*</query></grants>
    </collector>
  </users>
</clickhouse>
{{- end }}
EOH
      }

      volume_mount {
        volume      = "data"
        destination = "/var/lib/clickhouse"
      }

      resources {
        cpu    = 1000
        memory = 2048
      }
    }
  }
}
