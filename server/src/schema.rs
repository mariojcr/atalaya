// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! ClickHouse schema: `atalaya-server schema` prints it and `--apply` applies it.

use atalaya::ch::ClickHouse;

use crate::app::{env, ident};

const SCHEMA: &str = include_str!("../schema/atalaya.sql");

/// The schema for the ATALAYA_DB database: the file says `atalaya`, and the same
/// variable decides where the server and the alerts query
fn schema_for(db: &str) -> String {
    let re = regex::Regex::new(r"\batalaya\.").expect("regex");
    re.replace_all(SCHEMA, format!("{db}.").as_str())
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

/// Schema statements, without comments
fn schema_statements(schema: &str) -> Vec<String> {
    schema
        .split(";\n")
        .map(|s| s.lines().filter(|l| !l.trim_start().starts_with("--")).collect::<Vec<_>>().join("\n"))
        .map(|s| s.trim().trim_end_matches(';').to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

pub async fn schema(apply: bool) -> anyhow::Result<()> {
    let db = ident(&env("ATALAYA_DB", "atalaya"))?;
    let is_replicated = env("ATALAYA_REPLICATED", "") == "1";
    let schema = if is_replicated { replicated(&schema_for(&db)) } else { schema_for(&db) };
    if !apply {
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
        ch.exec(st).await.map_err(|e| anyhow::anyhow!("statement {} ({first}): {e:#}", i + 1))?;
    }
    println!("schema applied: {} statements", statements.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_uses_the_configured_database() {
        let s = schema_for("other");
        assert!(s.contains("CREATE DATABASE IF NOT EXISTS other;"));
        assert!(s.contains("CREATE TABLE IF NOT EXISTS other.points"));
        assert!(!s.contains("atalaya.points"));
        assert_eq!(schema_statements(&s).len(), schema_statements(SCHEMA).len());
    }

    #[test]
    fn replicated_schema_replicates_every_merge_tree_and_leaves_the_database_alone() {
        let s = replicated(&schema_for("other"));
        assert!(!s.contains("CREATE DATABASE"), "the database is the cluster's to create");
        let engines: Vec<&str> = s.lines().filter_map(|l| l.split("ENGINE = ").nth(1)).collect();
        assert!(engines.iter().any(|e| e.starts_with("ReplicatedReplacingMergeTree(Updated)")));
        assert!(engines.iter().any(|e| e.starts_with("ReplicatedAggregatingMergeTree")));
        for e in &engines {
            assert!(e.starts_with("Replicated") || e.starts_with("Null"), "not replicated: {e}");
            assert!(!e.starts_with("ReplicatedReplicated"), "replicated twice: {e}");
        }
        // Same statements as the plain schema, minus CREATE DATABASE.
        assert_eq!(schema_statements(&s).len() + 1, schema_statements(&schema_for("other")).len());
    }
}
