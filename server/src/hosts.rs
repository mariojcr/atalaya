// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The names the server answers to when sign-in is off. Without a session, a
//! page on another site could point its own name at this server (DNS
//! rebinding) and read everything through its visitor's browser, which sends
//! that name as the Host. Requests to an IP address, to localhost, to the host
//! of ATALAYA_PUBLIC_URL or to one of ATALAYA_ALLOWED_HOSTS pass; the rest get
//! 403. With sign-in on the check is not needed: the session cookie is only
//! sent to the name that set it.

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::app::env;

pub struct AllowedHosts {
    names: HashSet<String>,
    /// Hosts already refused, to log each one once (at most a few dozen)
    logged: Mutex<HashSet<String>>,
}

impl AllowedHosts {
    pub fn from_env() -> Arc<Self> {
        Arc::new(Self::new(&env("ATALAYA_PUBLIC_URL", ""), &env("ATALAYA_ALLOWED_HOSTS", "")))
    }

    fn new(public_url: &str, allowed: &str) -> Self {
        let public = public_url
            .split_once("://")
            .map_or("", |(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""));
        let names = std::iter::once(public)
            .chain(allowed.split(','))
            .map(|h| name(h.trim()))
            .filter(|h| !h.is_empty())
            .chain(["localhost".to_owned()])
            .collect();
        Self {
            names,
            logged: Mutex::new(HashSet::new()),
        }
    }

    /// Whether a Host header (name or address, with or without a port) is one this server answers to
    pub fn allows(&self, host: &str) -> bool {
        let host = host.trim();
        if let Some(inner) = host.strip_prefix('[') {
            return inner.split_once(']').is_some_and(|(ip, _)| ip.parse::<Ipv6Addr>().is_ok());
        }
        let name = name(host);
        name.parse::<Ipv4Addr>().is_ok() || self.names.contains(&name)
    }
}

/// The name of a host, without its port or trailing dot, in lowercase
fn name(host: &str) -> String {
    let host = match host.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Refuses requests to other names; health checks pass whatever their Host
pub async fn require(State(hosts): State<Arc<AllowedHosts>>, req: Request, next: Next) -> Response {
    if matches!(req.uri().path(), "/healthz" | "/readyz") {
        return next.run(req).await;
    }
    // No Host at all is not a browser, so not a rebinding; one that is not text is refused
    let host = match req.headers().get(header::HOST) {
        Some(h) => String::from_utf8_lossy(h.as_bytes()).into_owned(),
        None => req.uri().authority().map_or_else(String::new, |a| a.as_str().to_owned()),
    };
    if host.is_empty() || hosts.allows(&host) {
        return next.run(req).await;
    }
    let mut logged = hosts.logged.lock().unwrap_or_else(|e| e.into_inner());
    if logged.len() < 64 && logged.insert(host.clone()) {
        tracing::warn!("refused a request for host {host:?}: not in ATALAYA_ALLOWED_HOSTS nor ATALAYA_PUBLIC_URL");
    }
    drop(logged);
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "atalaya does not answer to this host name. Without sign-in it only answers to IP addresses, localhost, \
         the host of ATALAYA_PUBLIC_URL and the names in ATALAYA_ALLOWED_HOSTS (comma-separated): add the name \
         you reach it by to one of them.\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_localhost_and_configured_names_pass() {
        let h = AllowedHosts::new("https://Atalaya.Example.com/ui/", "mon.lan, other.example:8443 ,");
        for ok in [
            "127.0.0.1",
            "10.0.0.5:8080",
            "[::1]",
            "[fe80::1]:8080",
            "localhost",
            "localhost:8080",
            "atalaya.example.com",
            "ATALAYA.example.com.",
            "atalaya.example.com:443",
            "mon.lan:8080",
            "other.example",
        ] {
            assert!(h.allows(ok), "{ok} should pass");
        }
    }

    #[test]
    fn other_names_are_refused() {
        let h = AllowedHosts::new("", "");
        for bad in [
            "evil.example",
            "evil.example:8080",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "[not-an-ip]",
            "[::1",
            "atalaya.example.com",
            "",
        ] {
            assert!(!h.allows(bad), "{bad} should be refused");
        }
    }
}
