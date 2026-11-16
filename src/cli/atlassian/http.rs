//! HTTP plumbing of the Atlassian connector: credentials, bounded retries and an injectable
//! sleeper.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::base::{Error, Result};

/// Credentials of one site. The `Debug` output never shows the secrets.
#[derive(Clone)]
pub enum Credentials {
    /// Cloud: account e-mail and API token (HTTP basic auth).
    Basic {
        /// Account e-mail.
        email: String,
        /// API token.
        token: String,
    },
    /// Data Center: personal access token (bearer auth).
    Bearer {
        /// Personal access token.
        token: String,
    },
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { .. } => f.write_str("Credentials::Basic(<redacted>)"),
            Self::Bearer { .. } => f.write_str("Credentials::Bearer(<redacted>)"),
        }
    }
}

/// Waits between retries. Injectable so tests need not really sleep.
#[async_trait]
pub trait Sleeper: Send + Sync {
    /// Waits for `duration`.
    async fn sleep(&self, duration: Duration);
}

/// The real clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// How hard to retry. Delays are deterministic (no jitter): `base_delay * 2^attempt`, capped at
/// `max_delay`; a `Retry-After` header (in seconds) replaces the computed delay, also capped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries of 5xx answers and transport failures.
    pub max_server_retries: u32,
    /// Retries of HTTP 429 answers.
    pub max_rate_limit_retries: u32,
    /// First backoff delay.
    pub base_delay: Duration,
    /// Longest single wait.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_server_retries: 3,
            max_rate_limit_retries: 6,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// The wait before retry number `attempt` (0-based).
    pub fn backoff(&self, attempt: u32) -> Duration {
        self.base_delay
            .saturating_mul(2u32.saturating_pow(attempt.min(16)))
            .min(self.max_delay)
    }
}

/// A GET-only JSON client for one site.
pub(crate) struct Api<'a> {
    pub(super) http: reqwest::Client,
    pub(super) base: String,
    pub(super) credentials: &'a Credentials,
    /// Names of the environment variables to mention in authentication errors.
    pub(super) auth_hint: String,
    pub(super) sleeper: &'a dyn Sleeper,
    pub(super) retry: RetryPolicy,
}

impl Api<'_> {
    /// `GET <base><path>?<params>` decoded as JSON, with retries.
    ///
    /// 404 is [`Error::NotFound`]; 401/403 name the environment variables to check. Secrets never
    /// appear in errors.
    pub(super) async fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        let mut url = Url::parse(&format!("{}{path}", self.base)).map_err(|err| {
            Error::invalid(format!("bad Atlassian URL `{}{path}`: {err}", self.base))
        })?;
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params.iter().copied());
        }
        let (mut server_failures, mut rate_limits) = (0u32, 0u32);
        loop {
            let request = self
                .http
                .get(url.clone())
                .header(ACCEPT, "application/json");
            let request = match self.credentials {
                Credentials::Basic { email, token } => request.basic_auth(email, Some(token)),
                Credentials::Bearer { token } => request.bearer_auth(token),
            };
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    if server_failures < self.retry.max_server_retries {
                        self.sleeper
                            .sleep(self.retry.backoff(server_failures))
                            .await;
                        server_failures += 1;
                        continue;
                    }
                    return Err(Error::other(format!(
                        "request to {path} failed: {}",
                        err.without_url()
                    )));
                }
            };
            let status = response.status();
            if status.is_success() {
                return response.json::<Value>().await.map_err(|err| {
                    Error::Parse(format!("{path}: not JSON: {}", err.without_url()))
                });
            }
            match status {
                StatusCode::TOO_MANY_REQUESTS => {
                    if rate_limits >= self.retry.max_rate_limit_retries {
                        return Err(Error::other(format!(
                            "{path}: still rate limited (HTTP 429) after {rate_limits} retries; try again later"
                        )));
                    }
                    let advised = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let wait = advised
                        .unwrap_or_else(|| self.retry.backoff(rate_limits))
                        .min(self.retry.max_delay);
                    self.sleeper.sleep(wait).await;
                    rate_limits += 1;
                }
                status if status.is_server_error() => {
                    if server_failures >= self.retry.max_server_retries {
                        return Err(Error::other(format!(
                            "{path}: server error {status} after {server_failures} retries"
                        )));
                    }
                    self.sleeper
                        .sleep(self.retry.backoff(server_failures))
                        .await;
                    server_failures += 1;
                }
                StatusCode::UNAUTHORIZED => {
                    return Err(Error::other(format!(
                        "{} rejected the credentials (HTTP 401) for {path}: check {}",
                        self.base, self.auth_hint
                    )));
                }
                StatusCode::FORBIDDEN => {
                    return Err(Error::other(format!(
                        "{} refused access (HTTP 403) to {path}: the account may lack permission, or the credentials are wrong; check {}",
                        self.base, self.auth_hint
                    )));
                }
                StatusCode::NOT_FOUND => {
                    return Err(Error::not_found(format!("{path} (HTTP 404)")));
                }
                status => {
                    let body = response.text().await.unwrap_or_default();
                    let body: String = body.chars().take(200).collect();
                    return Err(Error::other(format!("{path}: HTTP {status}: {body}")));
                }
            }
        }
    }
}
