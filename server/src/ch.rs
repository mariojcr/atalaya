// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Minimal ClickHouse HTTP client. Queries use typed parameters ({name:Type}
//! in the SQL, param_name in the URL): no user-supplied value is ever pasted
//! into the query text.

use anyhow::{Result, anyhow};
use serde_json::Value;

#[derive(Clone)]
pub struct ClickHouse {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
}

/// ClickHouse failure (unreachable or returned an error): the API tells it
/// apart from a bad request and answers 502
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClickHouse: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

fn down(e: reqwest::Error) -> anyhow::Error {
    anyhow!(Unavailable(format!("unreachable: {e}")))
}

fn failed(body: &str) -> anyhow::Error {
    anyhow!(Unavailable(body.trim().to_owned()))
}

/// Value of an Array(String) parameter: ['a','b'] with quotes and backslashes escaped
pub fn array_param<S: AsRef<str>>(values: &[S]) -> String {
    let items: Vec<String> = values
        .iter()
        .map(|v| format!("'{}'", v.as_ref().replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", items.join(","))
}

/// A query parameter: name (without the param_ prefix) and value as text.
pub type Param = (String, String);

impl ClickHouse {
    pub fn new(url: String, user: String, password: String) -> Self {
        Self {
            // ClickHouse closes idle HTTP connections (keep_alive_timeout, 10 s
            // by default); if the pool reuses one afterwards the request fails
            // with "connection closed before message completed", so the client
            // drops them sooner.
            // Total per-request deadline (connect, query and transfer): a bit
            // more than max_execution_time, which only limits server-side execution
            http: reqwest::Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("HTTP client"),
            url,
            user,
            password,
        }
    }

    /// Runs a statement without a result (DDL, INSERT ... SELECT).
    pub async fn exec(&self, sql: &str) -> Result<()> {
        let resp = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.password))
            .body(sql.to_owned())
            .send()
            .await
            .map_err(down)?;
        if !resp.status().is_success() {
            return Err(failed(&resp.text().await.unwrap_or_default()));
        }
        Ok(())
    }

    /// Runs a query and returns the raw body (the SQL sets the format:
    /// RowBinary for large results, which would be costly to parse as text).
    pub async fn bytes(&self, sql: &str, params: &[Param], settings: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut query: Vec<(String, String)> = vec![("max_execution_time".into(), "60".into())];
        query.extend(settings.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        query.extend(params.iter().map(|(k, v)| (format!("param_{k}"), v.clone())));
        let resp = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.password))
            .query(&query)
            .body(sql.to_owned())
            .send()
            .await
            .map_err(down)?;
        let status = resp.status();
        if !status.is_success() || exception_header(&resp) {
            return Err(failed(&resp.text().await.unwrap_or_default()));
        }
        Ok(resp.bytes().await.map_err(down)?.to_vec())
    }

    /// Like `bytes`, as text
    pub async fn text(&self, sql: &str, params: &[Param], settings: &[(&str, &str)]) -> Result<String> {
        let mut query: Vec<(String, String)> = vec![("max_execution_time".into(), "60".into())];
        query.extend(settings.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        query.extend(params.iter().map(|(k, v)| (format!("param_{k}"), v.clone())));
        let resp = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.password))
            .query(&query)
            .body(sql.to_owned())
            .send()
            .await
            .map_err(down)?;
        let status = resp.status();
        let exception = exception_header(&resp);
        let body = resp.text().await.map_err(down)?;
        if !status.is_success() || exception || body.contains("DB::Exception") {
            return Err(failed(&body));
        }
        Ok(body)
    }

    /// Runs a query and returns its rows as JSON objects.
    pub async fn rows(&self, sql: &str, params: &[Param]) -> Result<Vec<Value>> {
        let mut query: Vec<(String, String)> = vec![
            ("default_format".into(), "JSON".into()),
            ("output_format_json_quote_64bit_integers".into(), "0".into()),
            ("max_execution_time".into(), "60".into()),
        ];
        query.extend(params.iter().map(|(k, v)| (format!("param_{k}"), v.clone())));

        let resp = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.password))
            .query(&query)
            .body(sql.to_owned())
            .send()
            .await
            .map_err(down)?;

        let status = resp.status();
        let exception = exception_header(&resp);
        let body = resp.text().await.map_err(down)?;
        if !status.is_success() || exception {
            return Err(failed(&body));
        }
        let json: Value = serde_json::from_str(&body).map_err(|e| failed(&format!("response is not JSON: {e}")))?;
        Ok(json["data"].as_array().cloned().unwrap_or_default())
    }
}

/// ClickHouse sets this header on responses that start with an exception even
/// when the HTTP status is 200
fn exception_header(resp: &reqwest::Response) -> bool {
    resp.headers().contains_key("x-clickhouse-exception-code")
}

#[cfg(test)]
mod tests {
    #[test]
    fn array_param_escapes() {
        assert_eq!(super::array_param::<&str>(&[]), "[]");
        assert_eq!(super::array_param(&["a".to_owned(), "o'b".to_owned()]), "['a','o\\'b']");
        // Metric names come from what exporters expose: a quote or a backslash in one stays inside its item
        assert_eq!(super::array_param(&["x'y", "a\\'b"]), "['x\\'y','a\\\\\\'b']");
    }
}
