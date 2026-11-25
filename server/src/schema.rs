// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

use atalaya::ch::ClickHouse;

use crate::app::{env, ident};

const LOGS: &str = include_str!("../schema/logs.sql");
const METRICS: &str = include_str!("../schema/metrics.sql");

/// The database and the logs table always; the metric tables only while metrics are stored in
/// ClickHouse. With them on VictoriaMetrics the server never reads those tables, and creating them
/// on every deploy would bring back the ones dropped after the move.
fn schema_text(metrics_in_clickhouse: bool) -> String {
    if metrics_in_clickhouse {
        format!("{LOGS}\n{METRICS}")
    } else {
        LOGS.to_owned()
    }
}

/// The schema for the ATALAYA_DB database: the files say `atalaya`, and the same
/// variable decides where the server and the alerts query
fn schema_for(db: &str, schema: &str) -> String {
    let re = regex::Regex::new(r"\batalaya\.").expect("regex");
    re.replace_all(schema, format!("{db}.").as_str())
        .replace("CREATE DATABASE IF NOT EXISTS atalaya;", &format!("CREATE DATABASE IF NOT EXISTS {db};"))
}

/// The schema for a replicated ClickHouse (ATALAYA_REPLICATED=1): every `*MergeTree` engine
/// becomes its `Replicated*MergeTree` without arguments, so the server's `default_replica_path`
/// and `default_replica_name` place it in Keeper, and the database is not created here. A
/// `Replicated` database replicates the DDL but does not convert engines, and it has to be created
/// on each replica against itself (`ENGINE = Replicated(path, '{shard}', '{replica}')`), which one
/// connection to one replica cannot do: whoever runs the cluster creates it before the schema.
fn replicated(schema: &str) -> String {
    let engine = regex::Regex::new(r"ENGINE = (\w*MergeTree)\b").expect("regex");
    engine
        .replace_all(schema, "ENGINE = Replicated$1")
        .lines()
        .filter(|l| !l.starts_with("CREATE DATABASE "))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn schema_statements(schema: &str) -> Vec<String> {
    schema
        .split(";\n")
        .map(|s| s.lines().filter(|l| !l.trim_start().starts_with("--")).collect::<Vec<_>>().join("\n"))
        .map(|s| s.trim().trim_end_matches(';').to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// How many times (a second apart) a replicated schema retries a statement on a
/// table another replica has created and this one has not yet
const REPLICA_RETRIES: u32 = 30;

/// ClickHouse's UNKNOWN_TABLE
fn unknown_table(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("(UNKNOWN_TABLE)")
}

pub async fn schema(apply: bool) -> anyhow::Result<()> {
    let db = ident(&env("ATALAYA_DB", "atalaya"))?;
    let is_replicated = env("ATALAYA_REPLICATED", "") == "1";
    // The same variable that sends the server's PromQL to VictoriaMetrics
    let metrics_in_clickhouse = env("ATALAYA_VICTORIA_METRICS_URL", "").is_empty();
    let schema = schema_for(&db, &schema_text(metrics_in_clickhouse));
    let schema = if is_replicated { replicated(&schema) } else { schema };
    if !apply {
        if !metrics_in_clickhouse {
            println!("-- ATALAYA_VICTORIA_METRICS_URL is set: metrics are on VictoriaMetrics, so their tables are left out");
        }
        print!("{schema}");
        return Ok(());
    }
    let ch = ClickHouse::new(
        env("CLICKHOUSE_URL", "http://127.0.0.1:8123/"),
        env("CLICKHOUSE_USER", "default"),
        std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_default(),
    );
    if is_replicated {
        let engine = ch
            .text(
                "SELECT engine FROM system.databases WHERE name = {db:String}",
                &[("db".to_owned(), db.clone())],
                &[],
            )
            .await?;
        if engine.trim() != "Replicated" {
            anyhow::bail!(
                "ATALAYA_REPLICATED=1 needs the database {db} to exist on every replica with ENGINE = Replicated (found: {:?})",
                engine.trim()
            );
        }
    }
    let statements = schema_statements(&schema);
    for (i, st) in statements.iter().enumerate() {
        let first = st.lines().next().unwrap_or_default();
        let mut tries = 0;
        loop {
            match ch.exec(st).await {
                Ok(()) => break,
                // The replica that runs a statement answers before the others have
                // applied it, and behind a load balancer the next one may reach a
                // replica that has not created the table yet: it will, give it time.
                Err(e) if is_replicated && tries < REPLICA_RETRIES && unknown_table(&e) => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Err(e) => anyhow::bail!("statement {} ({first}): {e:#}", i + 1),
            }
        }
    }
    if metrics_in_clickhouse {
        println!("schema applied: {} statements", statements.len());
    } else {
        println!(
            "schema applied: {} statements (logs only: ATALAYA_VICTORIA_METRICS_URL is set, metric tables are not created)",
            statements.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tables and views a schema creates, by name
    fn created(schema: &str) -> Vec<String> {
        let re = regex::Regex::new(r"CREATE (?:TABLE|MATERIALIZED VIEW) IF NOT EXISTS \w+\.(\w+)").expect("regex");
        re.captures_iter(schema).map(|c| c[1].to_owned()).collect()
    }

    #[test]
    fn schema_uses_the_configured_database() {
        for metrics in [true, false] {
            let s = schema_for("other", &schema_text(metrics));
            assert!(s.contains("CREATE DATABASE IF NOT EXISTS other;"));
            assert!(s.contains("CREATE TABLE IF NOT EXISTS other.logs"));
            assert!(!s.contains("atalaya."), "metrics: {metrics}");
            assert_eq!(schema_statements(&s).len(), schema_statements(&schema_text(metrics)).len());
        }
        assert!(schema_for("other", &schema_text(true)).contains("CREATE TABLE IF NOT EXISTS other.points"));
    }

    #[test]
    fn metrics_on_victoria_leave_every_metric_table_out() {
        let logs_only = schema_for("other", &schema_text(false));
        assert_eq!(created(&logs_only), ["logs"]);
        // Nothing else either: no ALTER of a metric table or view, which would fail once they are dropped
        for st in schema_statements(&logs_only) {
            assert!(
                st.starts_with("CREATE DATABASE IF NOT EXISTS other") || st.starts_with("CREATE TABLE IF NOT EXISTS other.logs"),
                "{st}"
            );
        }
        let full = schema_for("other", &schema_text(true));
        let metrics = created(&full);
        for t in [
            "metric_descriptors",
            "series",
            "points",
            "points_1m_mv",
            "dist_points_10m",
            "stale",
            "dist_import_10m_only_mv",
        ] {
            assert!(metrics.iter().any(|m| m == t), "{t} is in the full schema");
        }
        // The metrics part adds only metric objects: the database and logs live in the logs part
        assert_eq!(metrics.iter().filter(|m| *m == "logs").count(), 1);
        assert_eq!(
            schema_statements(&full).len(),
            schema_statements(&logs_only).len() + schema_statements(&schema_for("other", METRICS)).len()
        );
        assert!(!METRICS.contains("CREATE DATABASE") && !METRICS.contains("atalaya.logs"));
    }

    #[test]
    fn replicated_schema_replicates_every_merge_tree_and_leaves_the_database_alone() {
        for metrics in [true, false] {
            let plain = schema_for("other", &schema_text(metrics));
            let s = replicated(&plain);
            assert!(!s.contains("CREATE DATABASE"), "the database is the cluster's to create");
            let engines: Vec<&str> = s.lines().filter_map(|l| l.split("ENGINE = ").nth(1)).collect();
            assert!(engines.iter().any(|e| e.starts_with("ReplicatedMergeTree")), "the logs table");
            assert_eq!(engines.iter().any(|e| e.starts_with("ReplicatedReplacingMergeTree(Updated)")), metrics);
            assert_eq!(engines.iter().any(|e| e.starts_with("ReplicatedAggregatingMergeTree")), metrics);
            for e in &engines {
                assert!(e.starts_with("Replicated") || e.starts_with("Null"), "not replicated: {e}");
                assert!(!e.starts_with("ReplicatedReplicated"), "replicated twice: {e}");
            }
            // Same statements as the plain schema, minus CREATE DATABASE.
            assert_eq!(schema_statements(&s).len() + 1, schema_statements(&plain).len());
        }
    }

    #[test]
    fn only_a_missing_table_is_waited_for() {
        let missing = anyhow::anyhow!(
            "ClickHouse: Code: 60. DB::Exception: Table atalaya.points_1m does not exist. Maybe you meant atalaya.points?. (UNKNOWN_TABLE) (version 26.8.15.10 (official build))"
        );
        assert!(unknown_table(&missing));
        let denied = anyhow::anyhow!("ClickHouse: Code: 497. DB::Exception: atalaya_schema: Not enough privileges. (ACCESS_DENIED)");
        assert!(!unknown_table(&denied));
    }
}
