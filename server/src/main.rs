// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! atalaya: Cloud Monitoring-style metrics and logs on ClickHouse.
//!
//! Environment (defaults in parentheses):
//!   ATALAYA_LISTEN            listen address (0.0.0.0:8080)
//!   CLICKHOUSE_URL            http://host:8123/
//!   CLICKHOUSE_USER           read-only user (reader)
//!   CLICKHOUSE_PASSWORD
//!   ATALAYA_DB                atalaya database (atalaya)
//!   ATALAYA_DASHBOARDS        directory of YAML dashboards (dashboards)
//!   ATALAYA_OVERVIEW          overview.yaml for the home page (next to the dashboards directory)
//!   ATALAYA_TIMELINE          timeline.yaml for the timeline (same)
//!   ATALAYA_ALERTS            alerts.yaml with alert rules and channels (same)
//!   ATALAYA_PUBLIC_URL        UI URL included in notifications (optional)
//!   ATALAYA_ALLOWED_HOSTS     without sign-in, other names the server answers to besides IP
//!                             addresses, localhost and the host of ATALAYA_PUBLIC_URL (comma-separated)
//!   ATALAYA_DATA              directory where the server keeps notebooks, saved searches and alert state (data)
//!   ATALAYA_OIDC_ISSUER       OpenID Connect issuer: sign-in required for everything (off when unset)
//!   ATALAYA_OIDC_CLIENT_ID    public client with PKCE at the provider (atalaya); its callback is
//!                             <ATALAYA_PUBLIC_URL>/auth/callback
//!   ATALAYA_OIDC_INTERNAL     where the server reaches the provider, if not at the issuer URL
//!   ATALAYA_SESSION_KEY       secret signing the session cookie, 32+ characters (needed with sign-in)
//!   ATALAYA_SESSION_DAYS      session length (30)
//!   ATALAYA_NOW_DELAY         seconds panels, alerts and the home page stay behind now (45; 30 with a 15 s flush)
//!   ATALAYA_REFRESH           seconds between refreshes of the pages that are not dashboards (60)
//!
//! `atalaya-server schema` prints the schema; `atalaya-server schema --apply` applies it
//! (idempotent) as CLICKHOUSE_USER, which must be allowed to create tables.

mod alerts;
mod api;
mod app;
mod auth;
mod events;
mod hosts;
mod logsearch;
mod notebooks;
mod overview;
mod query;
mod schema;

use std::{path::PathBuf, sync::Arc};

use atalaya::{ch::ClickHouse, promql};

use app::{App, config_path, env, ident};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // No colours outside a terminal
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("schema") {
        return schema::schema(args.get(2).map(String::as_str) == Some("--apply")).await;
    }

    let ch = ClickHouse::new(
        env("CLICKHOUSE_URL", "http://127.0.0.1:8123/"),
        env("CLICKHOUSE_USER", "reader"),
        std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_default(),
    );
    let db = ident(&env("ATALAYA_DB", "atalaya"))?;
    let app = Arc::new(App {
        promql: promql::Engine::new(promql::store::Store::new(ch.clone(), db.clone())),
        ch,
        db,
        dashboards: PathBuf::from(env("ATALAYA_DASHBOARDS", "dashboards")),
        overview: config_path("ATALAYA_OVERVIEW", "overview.yaml"),
        timeline: config_path("ATALAYA_TIMELINE", "timeline.yaml"),
        data: PathBuf::from(env("ATALAYA_DATA", "data")),
        alerts: alerts::Alerts::new(config_path("ATALAYA_ALERTS", "alerts.yaml"), &PathBuf::from(env("ATALAYA_DATA", "data"))),
        overview_cache: Default::default(),
    });
    let auth = auth::Auth::from_env()?;
    tokio::spawn(alerts::run(app.clone()));

    let mut router = api::router(app.clone());
    match auth {
        Some(auth) => {
            tracing::info!("sign-in required");
            router = router
                .merge(auth::routes(auth.clone()))
                .layer(axum::middleware::from_fn_with_state(auth, auth::require));
        }
        None => {
            tracing::warn!("sign-in off (no ATALAYA_OIDC_ISSUER): anyone who reaches atalaya can use it");
            // ...but only by a name it knows, so another site cannot rebind its own to it
            router = router.layer(axum::middleware::from_fn_with_state(hosts::AllowedHosts::from_env(), hosts::require));
        }
    }

    let listen = env("ATALAYA_LISTEN", "0.0.0.0:8080");
    tracing::info!("atalaya listening on http://{listen}");
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    axum::serve(listener, router)
        // Ctrl+C locally, SIGTERM from a container runtime or orchestrator
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        })
        .await?;
    Ok(())
}
