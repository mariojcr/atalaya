// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Sign-in with an OpenID Connect provider (optional).
//!
//! With `ATALAYA_OIDC_ISSUER` set, every page and API call needs a session
//! (except `/healthz`, `/readyz` and `/auth/*`): pages send the browser to
//! sign in, API calls get 401. Without it atalaya stays open, for a network
//! that is already private.
//!
//! Authorization code flow with PKCE as a public client: no client secret to
//! keep. Who may sign in is up to the provider (in Pocket ID, the client's
//! allowed groups). The session is a cookie signed with HMAC-SHA256
//! (`ATALAYA_SESSION_KEY`): nothing is stored on the server, and changing the
//! key signs everyone out.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::app::{env, unix_now};

const SESSION_COOKIE: &str = "atalaya_session";
const FLOW_COOKIE: &str = "atalaya_login";

pub struct Auth {
    issuer: String,
    /// Where atalaya itself reaches the provider (the issuer unless set)
    internal: String,
    client_id: String,
    public_url: String,
    key: Vec<u8>,
    session_secs: i64,
    http: reqwest::Client,
}

pub type SharedAuth = Arc<Auth>;

impl Auth {
    /// From the environment; None when sign-in is off (no ATALAYA_OIDC_ISSUER)
    pub fn from_env() -> anyhow::Result<Option<SharedAuth>> {
        let issuer = env("ATALAYA_OIDC_ISSUER", "");
        if issuer.is_empty() {
            return Ok(None);
        }
        let trim = |s: String| s.trim_end_matches('/').to_owned();
        let public_url = trim(env("ATALAYA_PUBLIC_URL", ""));
        anyhow::ensure!(
            public_url.starts_with("https://") || public_url.starts_with("http://"),
            "ATALAYA_PUBLIC_URL is needed for sign-in"
        );
        let key = env("ATALAYA_SESSION_KEY", "");
        anyhow::ensure!(key.len() >= 32, "ATALAYA_SESSION_KEY: at least 32 characters");
        let days: i64 = env("ATALAYA_SESSION_DAYS", "30").parse()?;
        let internal = env("ATALAYA_OIDC_INTERNAL", "");
        Ok(Some(Arc::new(Self {
            internal: trim(if internal.is_empty() { issuer.clone() } else { internal }),
            issuer: trim(issuer),
            client_id: env("ATALAYA_OIDC_CLIENT_ID", "atalaya"),
            public_url,
            key: key.into_bytes(),
            session_secs: days * 86400,
            http: reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?,
        })))
    }

    async fn discovery(&self) -> anyhow::Result<Value> {
        let r = self
            .http
            .get(format!("{}/.well-known/openid-configuration", self.internal))
            .send()
            .await?
            .error_for_status()?;
        Ok(r.json().await?)
    }

    /// An endpoint of the discovery document, at the address atalaya uses
    fn inside(&self, url: &str) -> String {
        url.replacen(&self.issuer, &self.internal, 1)
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC takes any key length");
        mac.update(payload.as_bytes());
        B64.encode(mac.finalize().into_bytes())
    }

    /// `payload.signature`, payload being base64 of {"sub","name","exp"}
    fn session_cookie(&self, sub: &str, name: &str) -> String {
        let payload = B64.encode(json!({"sub": sub, "name": name, "exp": unix_now() + self.session_secs}).to_string());
        format!("{payload}.{}", self.sign(&payload))
    }

    /// The session's user, if the cookie is genuine and not expired
    fn session(&self, headers: &HeaderMap) -> Option<Value> {
        let c = cookie(headers, SESSION_COOKIE)?;
        let (payload, sig) = c.split_once('.')?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).ok()?;
        mac.update(payload.as_bytes());
        // constant time
        mac.verify_slice(&B64.decode(sig).ok()?).ok()?;
        let v: Value = serde_json::from_slice(&B64.decode(payload).ok()?).ok()?;
        (v["exp"].as_i64()? > unix_now()).then_some(v)
    }
}

pub fn routes(auth: SharedAuth) -> Router {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", get(logout))
        .route("/auth/me", get(me))
        .with_state(auth)
}

/// Lets through requests with a session; pages without one go to sign in,
/// API calls get 401 (the UI then reloads into the sign-in)
pub async fn require(State(auth): State<SharedAuth>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if matches!(path, "/healthz" | "/readyz") || path.starts_with("/auth/") || auth.session(req.headers()).is_some() {
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({"error": "login"})),
        )
            .into_response();
    }
    let to = req.uri().path_and_query().map_or("/", |p| p.as_str());
    Redirect::to(&format!("/auth/login?to={}", urlencode(to))).into_response()
}

fn random(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system randomness");
    B64.encode(buf)
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('=').map(str::to_owned))
}

fn set_cookie(name: &str, value: &str, path: &str, max_age: i64) -> String {
    format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax")
}

/// Where to go back after signing in: only paths of this site. Browsers drop
/// tabs and newlines from a URL and read `\` as `/`, so `/\t/evil.example`
/// would become `//evil.example`: none of them is allowed anywhere.
fn safe_return(to: Option<&str>) -> String {
    match to {
        Some(t) if t.starts_with('/') && !t.starts_with("//") && !t.chars().any(|c| c == '\\' || c.is_whitespace() || c.is_control()) => t.to_owned(),
        _ => "/".to_owned(),
    }
}

fn urlencode(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn page(status: StatusCode, msg: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'><title>Atalaya</title>\
         <p style='font:16px system-ui;margin:3rem auto;max-width:28rem'>{msg} <a href='/auth/login'>Sign in</a></p>"
    );
    (status, [(header::CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

#[derive(Deserialize)]
struct LoginParams {
    to: Option<String>,
}

/// Sends the browser to the provider
async fn login(State(auth): State<SharedAuth>, Query(p): Query<LoginParams>) -> Response {
    let Ok(d) = auth.discovery().await else {
        return page(StatusCode::BAD_GATEWAY, "The identity provider can't be reached right now.");
    };
    let (state, verifier) = (random(16), random(32));
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let to = B64.encode(safe_return(p.to.as_deref()));
    let redirect = format!("{}/auth/callback", auth.public_url);
    let qs = [
        ("response_type", "code"),
        ("client_id", auth.client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("scope", "openid profile email"),
        ("state", state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencode(v)))
    .collect::<Vec<_>>()
    .join("&");
    let url = format!("{}?{qs}", d["authorization_endpoint"].as_str().unwrap_or_default());
    (
        [(header::SET_COOKIE, set_cookie(FLOW_COOKIE, &format!("{state}.{verifier}.{to}"), "/auth", 600))],
        Redirect::to(&url),
    )
        .into_response()
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Back from the provider: the code for a token, who it is, a session
async fn callback(State(auth): State<SharedAuth>, headers: HeaderMap, Query(p): Query<CallbackParams>) -> Response {
    if let Some(e) = p.error {
        tracing::warn!("sign-in refused by the provider: {e}");
        return page(StatusCode::FORBIDDEN, "Sign-in was not completed.");
    }
    let flow = cookie(&headers, FLOW_COOKIE).unwrap_or_default();
    let mut parts = flow.splitn(3, '.');
    let (Some(state), Some(verifier), Some(to)) = (parts.next(), parts.next(), parts.next()) else {
        return page(StatusCode::BAD_REQUEST, "Sign-in expired.");
    };
    let (Some(code), Some(got)) = (p.code, p.state) else {
        return page(StatusCode::BAD_REQUEST, "Incomplete answer from the provider.");
    };
    if got != state {
        return page(StatusCode::BAD_REQUEST, "Sign-in expired.");
    }
    let to = B64
        .decode(to)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map_or_else(|| "/".to_owned(), |t| safe_return(Some(&t)));
    match sign_in(&auth, &code, verifier).await {
        Ok((sub, name)) => {
            tracing::info!(sub, name, "signed in");
            let mut res = Redirect::to(&to).into_response();
            let h = res.headers_mut();
            h.append(
                header::SET_COOKIE,
                set_cookie(SESSION_COOKIE, &auth.session_cookie(&sub, &name), "/", auth.session_secs)
                    .parse()
                    .unwrap(),
            );
            h.append(header::SET_COOKIE, set_cookie(FLOW_COOKIE, "", "/auth", 0).parse().unwrap());
            res
        }
        Err(e) => {
            tracing::warn!("sign-in failed: {e:#}");
            page(StatusCode::BAD_GATEWAY, "Sign-in with the identity provider failed.")
        }
    }
}

async fn sign_in(auth: &Auth, code: &str, verifier: &str) -> anyhow::Result<(String, String)> {
    let d = auth.discovery().await?;
    let redirect = format!("{}/auth/callback", auth.public_url);
    let res = auth
        .http
        .post(auth.inside(d["token_endpoint"].as_str().unwrap_or_default()))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect),
            ("client_id", &auth.client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;
    let status = res.status();
    let token: Value = res.json().await?;
    anyhow::ensure!(status.is_success(), "token endpoint {status}: {}", token["error"]);
    let access = token["access_token"].as_str().unwrap_or_default();
    let info: Value = auth
        .http
        .get(auth.inside(d["userinfo_endpoint"].as_str().unwrap_or_default()))
        .bearer_auth(access)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let sub = info["sub"].as_str().unwrap_or_default().to_owned();
    anyhow::ensure!(!sub.is_empty(), "no subject in userinfo");
    let name = info["name"]
        .as_str()
        .or(info["preferred_username"].as_str())
        .or(info["email"].as_str())
        .unwrap_or("")
        .to_owned();
    Ok((sub, name))
}

/// Ends the session here and at the provider
async fn logout(State(auth): State<SharedAuth>) -> Response {
    let back = match auth.discovery().await.ok().and_then(|d| d["end_session_endpoint"].as_str().map(str::to_owned)) {
        Some(end) => format!(
            "{end}?client_id={}&post_logout_redirect_uri={}",
            urlencode(&auth.client_id),
            urlencode(&format!("{}/", auth.public_url))
        ),
        None => "/".to_owned(),
    };
    ([(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", "/", 0))], Redirect::to(&back)).into_response()
}

/// Who is signed in (401 without a session)
async fn me(State(auth): State<SharedAuth>, headers: HeaderMap) -> Response {
    match auth.session(&headers) {
        Some(v) => axum::Json(json!({"sub": v["sub"], "name": v["name"]})).into_response(),
        None => (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "login"}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth {
            issuer: "https://id.example".into(),
            internal: "https://id.example".into(),
            client_id: "atalaya".into(),
            public_url: "https://atalaya.example".into(),
            key: b"0123456789abcdef0123456789abcdef".to_vec(),
            session_secs: 3600,
            http: reqwest::Client::new(),
        }
    }

    fn headers(c: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("x=1; {SESSION_COOKIE}={c}").parse().unwrap());
        h
    }

    #[test]
    fn session_round_trip_and_tampering() {
        let a = auth();
        let c = a.session_cookie("user-1", "Mario");
        assert_eq!(a.session(&headers(&c)).unwrap()["sub"], "user-1");
        // another payload with the old signature
        let (_, sig) = c.split_once('.').unwrap();
        let forged = format!("{}.{sig}", B64.encode(r#"{"sub":"admin","name":"x","exp":9999999999}"#));
        assert!(a.session(&headers(&forged)).is_none());
        // another key
        let mut b = auth();
        b.key = b"another key of thirty-two bytes!!".to_vec();
        assert!(b.session(&headers(&c)).is_none());
        assert!(a.session(&HeaderMap::new()).is_none());
    }

    #[test]
    fn expired_sessions_are_refused() {
        let mut a = auth();
        a.session_secs = -1;
        let c = a.session_cookie("user-1", "Mario");
        assert!(a.session(&headers(&c)).is_none());
    }

    #[test]
    fn only_local_paths_to_return_to() {
        assert_eq!(safe_return(Some("/logs?q=a")), "/logs?q=a");
        assert_eq!(safe_return(Some("//evil.example/")), "/");
        assert_eq!(safe_return(Some("/\\evil.example")), "/");
        assert_eq!(safe_return(Some("/\t/evil.example")), "/");
        assert_eq!(safe_return(Some("/\n/evil.example")), "/");
        assert_eq!(safe_return(Some("/ /evil.example")), "/");
        assert_eq!(safe_return(Some("https://evil.example/")), "/");
        assert_eq!(safe_return(None), "/");
    }
}
