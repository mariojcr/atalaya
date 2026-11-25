-- SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
-- SPDX-License-Identifier: Apache-2.0
-- atalaya schema, logs part: the database and the logs table. Idempotent (IF NOT EXISTS):
-- `atalaya-server schema --apply` always applies it, and then metrics.sql unless the metrics are
-- on VictoriaMetrics (ATALAYA_VICTORIA_METRICS_URL).

CREATE DATABASE IF NOT EXISTS atalaya;

-- Logs, kept forever. Resource holds the Nomad job, group, task and alloc
-- (when collected from Nomad); Attributes holds fields of JSON lines (nginx…).
CREATE TABLE IF NOT EXISTS atalaya.logs
(
    Time       DateTime64(3) CODEC(Delta(8), ZSTD(1)),
    Service    LowCardinality(String),
    Host       LowCardinality(String),
    Stream     LowCardinality(String),
    Level      LowCardinality(String),
    Body       String CODEC(ZSTD(3)),
    Resource   Map(LowCardinality(String), String) CODEC(ZSTD(1)),
    Attributes Map(LowCardinality(String), String) CODEC(ZSTD(1)),
    INDEX idx_body lower(Body) TYPE text(tokenizer = 'splitByNonAlpha'),
    INDEX idx_level Level TYPE set(16) GRANULARITY 4
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Service, Time);
