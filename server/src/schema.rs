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
    let schema = schema_for(&db);
    if !apply {
        print!("{schema}");
        return Ok(());
    }
    let ch = ClickHouse::new(
        env("CLICKHOUSE_URL", "http://127.0.0.1:8123/"),
        env("CLICKHOUSE_USER", "default"),
        std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_default(),
    );
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
}
