-- SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
-- SPDX-License-Identifier: Apache-2.0
-- atalaya schema. Idempotent (IF NOT EXISTS): `atalaya-server schema --apply`.
--
-- Cloud Monitoring model:
--   metric_descriptors  one row per metric: kind, unit, description
--   series              one row per series: metric, service, resource and
--                       metric labels, bucket bounds for distributions
--   points              scalar points: series, time, value (7 days)
--   points_1m/_10m      per bucket: min, max, sum, count and DDSketch
--                       (6 weeks / forever), written on insert
--   dist_points         distributions (Prometheus histograms): count, sum and
--                       cumulative buckets (7 days), with their tiers
--   logs                logs, kept forever, with their service and resource
--
-- The collector registers each series once (refreshing LastSeen hourly) and
-- then only sends points. Materialized views compute the tiers on insert.

CREATE DATABASE IF NOT EXISTS atalaya;

CREATE TABLE IF NOT EXISTS atalaya.metric_descriptors
(
    Metric      LowCardinality(String),
    Kind        Enum8('gauge' = 1, 'cumulative' = 2, 'delta' = 3, 'distribution' = 4),
    Unit        LowCardinality(String),
    Description String,
    Updated     DateTime
)
ENGINE = ReplacingMergeTree(Updated)
ORDER BY Metric;

-- Fine index granularity (series and raw points 1024 rows, 1-minute tier 2048,
-- 10-minute tier 512): a query reads a few series out of thousands, and with
-- the default 8192 every lookup read whole granules of others. More marks in
-- memory: give ClickHouse a mark cache of about 1 GiB (mark_cache_size).
CREATE TABLE IF NOT EXISTS atalaya.series
(
    SeriesId  UInt64,
    Metric    LowCardinality(String),
    Service   LowCardinality(String),
    Resource  Map(LowCardinality(String), String),
    Labels    Map(LowCardinality(String), String),
    -- Bucket upper bounds (distributions only)
    Bounds    Array(Float64),
    LastSeen  DateTime
)
ENGINE = ReplacingMergeTree(LastSeen)
ORDER BY (Metric, SeriesId)
SETTINGS index_granularity = 1024;

-- Scalar points. Counter flags cumulative metrics (rates come from each
-- bucket's maximum and percentiles make no sense for them); sorted by metric,
-- it is constant per block and takes no space.
CREATE TABLE IF NOT EXISTS atalaya.points
(
    Metric   LowCardinality(String),
    SeriesId UInt64,
    Time     DateTime CODEC(DoubleDelta, ZSTD(1)),
    Value    Float64 CODEC(Gorilla, ZSTD(1)),
    Counter  Bool DEFAULT false
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Metric, SeriesId, Time)
TTL Time + INTERVAL 7 DAY
SETTINGS index_granularity = 1024, merge_with_ttl_timeout = 86400;
-- The TTL rewrites the whole monthly partition to drop what expired: every
-- 4 hours by default (33 GiB a day across the four tables with a TTL). Once a
-- day is enough; data stays up to a day longer. Also for existing databases:
ALTER TABLE atalaya.points MODIFY SETTING merge_with_ttl_timeout = 86400;

CREATE TABLE IF NOT EXISTS atalaya.points_1m
(
    Metric   LowCardinality(String),
    SeriesId UInt64,
    Time     DateTime CODEC(DoubleDelta, ZSTD(1)),
    Min      SimpleAggregateFunction(min, Float64) CODEC(Gorilla, ZSTD(1)),
    Max      SimpleAggregateFunction(max, Float64) CODEC(Gorilla, ZSTD(1)),
    Sum      SimpleAggregateFunction(sum, Float64) CODEC(Gorilla, ZSTD(1)),
    Count    SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    -- Time of the bucket's last sample, so rates over the tiers do not spike
    -- on partial buckets (restarts, gaps). Rows written before the column
    -- existed default to the bucket start, so any real value wins in max()
    LastTime SimpleAggregateFunction(max, DateTime) DEFAULT Time CODEC(DoubleDelta, ZSTD(1)),
    Sketch   AggregateFunction(quantilesDD(0.01, 0.5, 0.95, 0.99), Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Metric, SeriesId, Time)
TTL Time + INTERVAL 42 DAY
SETTINGS index_granularity = 2048, merge_with_ttl_timeout = 86400;
ALTER TABLE atalaya.points_1m MODIFY SETTING merge_with_ttl_timeout = 86400;
-- Databases created before LastTime existed
ALTER TABLE atalaya.points_1m ADD COLUMN IF NOT EXISTS LastTime SimpleAggregateFunction(max, DateTime) DEFAULT Time CODEC(DoubleDelta, ZSTD(1)) AFTER Count;

CREATE TABLE IF NOT EXISTS atalaya.points_10m
(
    Metric   LowCardinality(String),
    SeriesId UInt64,
    Time     DateTime CODEC(DoubleDelta, ZSTD(1)),
    Min      SimpleAggregateFunction(min, Float64) CODEC(Gorilla, ZSTD(1)),
    Max      SimpleAggregateFunction(max, Float64) CODEC(Gorilla, ZSTD(1)),
    Sum      SimpleAggregateFunction(sum, Float64) CODEC(Gorilla, ZSTD(1)),
    Count    SimpleAggregateFunction(sum, UInt64) CODEC(T64, ZSTD(1)),
    -- Time of the bucket's last sample, so rates over the tiers do not spike
    -- on partial buckets (restarts, gaps). Rows written before the column
    -- existed default to the bucket start, so any real value wins in max()
    LastTime SimpleAggregateFunction(max, DateTime) DEFAULT Time CODEC(DoubleDelta, ZSTD(1)),
    Sketch   AggregateFunction(quantilesDD(0.01, 0.5, 0.95, 0.99), Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Metric, SeriesId, Time)
SETTINGS index_granularity = 512;
-- Databases created before LastTime existed
ALTER TABLE atalaya.points_10m ADD COLUMN IF NOT EXISTS LastTime SimpleAggregateFunction(max, DateTime) DEFAULT Time CODEC(DoubleDelta, ZSTD(1)) AFTER Count;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.points_1m_mv TO atalaya.points_1m AS
SELECT Metric, SeriesId, toStartOfMinute(T0) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points)
GROUP BY Metric, SeriesId, Time;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.points_10m_mv TO atalaya.points_10m AS
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points)
GROUP BY Metric, SeriesId, Time;

-- Distributions (Prometheus histograms): values cumulative over time. Each
-- bucket keeps the last state; percentiles over a window come from the bucket
-- difference between its start and its end.
CREATE TABLE IF NOT EXISTS atalaya.dist_points
(
    Metric   LowCardinality(String),
    SeriesId UInt64,
    Time     DateTime CODEC(DoubleDelta, ZSTD(1)),
    Count    UInt64 CODEC(DoubleDelta, ZSTD(1)),
    Sum      Float64 CODEC(Gorilla, ZSTD(1)),
    Buckets  Array(UInt64) CODEC(ZSTD(1))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Metric, SeriesId, Time)
TTL Time + INTERVAL 7 DAY
SETTINGS index_granularity = 1024, merge_with_ttl_timeout = 86400;
ALTER TABLE atalaya.dist_points MODIFY SETTING merge_with_ttl_timeout = 86400;

-- Staleness markers: when a series stopped being written (missing from its
-- target's scrape, a failed scrape, a target that is gone). PromQL does not
-- take its last value past that time, as in Prometheus. Series that come and
-- go with activity (Nomad's own hook timings) make hundreds of thousands a
-- day, so they are kept like the 1-minute tier: past that, queries read the
-- 10-minute tier, whose lookback is longer than what a marker saves. The
-- collectors send them best effort: without this table they are dropped and
-- nothing else is held up.
CREATE TABLE IF NOT EXISTS atalaya.stale
(
    Metric   LowCardinality(String),
    SeriesId UInt64,
    Time     DateTime CODEC(DoubleDelta, ZSTD(1))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Metric, SeriesId, Time)
TTL Time + INTERVAL 42 DAY
SETTINGS merge_with_ttl_timeout = 86400;
-- For tables created without the TTL:
ALTER TABLE atalaya.stale MODIFY TTL Time + INTERVAL 42 DAY;
ALTER TABLE atalaya.stale MODIFY SETTING merge_with_ttl_timeout = 86400;

CREATE TABLE IF NOT EXISTS atalaya.dist_points_1m
(
    Metric   LowCardinality(String),
    SeriesId UInt64,
    Time     DateTime CODEC(DoubleDelta, ZSTD(1)),
    Last     AggregateFunction(argMax, Tuple(UInt64, Float64, Array(UInt64)), DateTime)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Metric, SeriesId, Time)
TTL Time + INTERVAL 42 DAY
SETTINGS index_granularity = 2048, merge_with_ttl_timeout = 86400;
ALTER TABLE atalaya.dist_points_1m MODIFY SETTING merge_with_ttl_timeout = 86400;

CREATE TABLE IF NOT EXISTS atalaya.dist_points_10m
(
    Metric   LowCardinality(String),
    SeriesId UInt64,
    Time     DateTime CODEC(DoubleDelta, ZSTD(1)),
    Last     AggregateFunction(argMax, Tuple(UInt64, Float64, Array(UInt64)), DateTime)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(Time)
ORDER BY (Metric, SeriesId, Time)
SETTINGS index_granularity = 512;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.dist_points_1m_mv TO atalaya.dist_points_1m AS
SELECT Metric, SeriesId, toStartOfMinute(T0) AS Time,
       argMaxState(tuple(Count, Sum, Buckets), T0) AS Last
FROM (SELECT Metric, SeriesId, Time AS T0, Count, Sum, Buckets FROM atalaya.dist_points)
GROUP BY Metric, SeriesId, Time;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.dist_points_10m_mv TO atalaya.dist_points_10m AS
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time,
       argMaxState(tuple(Count, Sum, Buckets), T0) AS Last
FROM (SELECT Metric, SeriesId, Time AS T0, Count, Sum, Buckets FROM atalaya.dist_points)
GROUP BY Metric, SeriesId, Time;

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

-- History import (e.g. from VictoriaMetrics). Null input tables store nothing,
-- and their views write straight into the tiers, so old data skips the raw
-- points (which only last 7 days):
--   points_import       → 1 and 10 minute tiers (data 7 to 42 days old)
--   points_import_10m   → 10 minute tier only (older than 42 days)
--   dist_import, dist_import_10m: the same for distributions
CREATE TABLE IF NOT EXISTS atalaya.points_import AS atalaya.points ENGINE = Null;
CREATE TABLE IF NOT EXISTS atalaya.points_import_10m AS atalaya.points ENGINE = Null;
CREATE TABLE IF NOT EXISTS atalaya.dist_import AS atalaya.dist_points ENGINE = Null;
CREATE TABLE IF NOT EXISTS atalaya.dist_import_10m AS atalaya.dist_points ENGINE = Null;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.points_import_1m_mv TO atalaya.points_1m AS
SELECT Metric, SeriesId, toStartOfMinute(T0) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points_import)
GROUP BY Metric, SeriesId, Time;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.points_import_10m_mv TO atalaya.points_10m AS
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points_import)
GROUP BY Metric, SeriesId, Time;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.points_import_10m_only_mv TO atalaya.points_10m AS
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points_import_10m)
GROUP BY Metric, SeriesId, Time;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.dist_import_1m_mv TO atalaya.dist_points_1m AS
SELECT Metric, SeriesId, toStartOfMinute(T0) AS Time, argMaxState(tuple(Count, Sum, Buckets), T0) AS Last
FROM (SELECT Metric, SeriesId, Time AS T0, Count, Sum, Buckets FROM atalaya.dist_import)
GROUP BY Metric, SeriesId, Time;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.dist_import_10m_mv TO atalaya.dist_points_10m AS
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time, argMaxState(tuple(Count, Sum, Buckets), T0) AS Last
FROM (SELECT Metric, SeriesId, Time AS T0, Count, Sum, Buckets FROM atalaya.dist_import)
GROUP BY Metric, SeriesId, Time;

CREATE MATERIALIZED VIEW IF NOT EXISTS atalaya.dist_import_10m_only_mv TO atalaya.dist_points_10m AS
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time, argMaxState(tuple(Count, Sum, Buckets), T0) AS Last
FROM (SELECT Metric, SeriesId, Time AS T0, Count, Sum, Buckets FROM atalaya.dist_import_10m)
GROUP BY Metric, SeriesId, Time;

-- Databases created before LastTime existed: their views start filling it
ALTER TABLE atalaya.points_1m_mv MODIFY QUERY
SELECT Metric, SeriesId, toStartOfMinute(T0) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points)
GROUP BY Metric, SeriesId, Time;
ALTER TABLE atalaya.points_10m_mv MODIFY QUERY
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points)
GROUP BY Metric, SeriesId, Time;
ALTER TABLE atalaya.points_import_1m_mv MODIFY QUERY
SELECT Metric, SeriesId, toStartOfMinute(T0) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points_import)
GROUP BY Metric, SeriesId, Time;
ALTER TABLE atalaya.points_import_10m_mv MODIFY QUERY
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points_import)
GROUP BY Metric, SeriesId, Time;
ALTER TABLE atalaya.points_import_10m_only_mv MODIFY QUERY
SELECT Metric, SeriesId, toStartOfInterval(T0, INTERVAL 10 MINUTE) AS Time,
       min(Value) AS Min, max(Value) AS Max, sum(Value) AS Sum, count() AS Count, max(T0) AS LastTime,
       quantilesDDStateIf(0.01, 0.5, 0.95, 0.99)(Value, NOT Counter) AS Sketch
FROM (SELECT Metric, SeriesId, Time AS T0, Value, Counter FROM atalaya.points_import_10m)
GROUP BY Metric, SeriesId, Time;
