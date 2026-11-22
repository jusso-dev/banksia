use crate::{auth::AppAuth, error::GraphError};
use bytes::Bytes;
use reqwest::{Method, StatusCode, header};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use url::Url;

#[derive(Debug, Clone)]
pub struct GraphSettings {
    /// For example `https://graph.microsoft.com/v1.0`.
    pub base_url: String,
    pub max_retries: u32,
    pub concurrency: usize,
    /// First backoff delay when no `Retry-After` is given.
    pub base_backoff: Duration,
    /// Upper bound on any single wait, including `Retry-After`.
    pub max_backoff: Duration,
    /// Permit `http://` URLs. Only for local fakes in tests.
    pub allow_http: bool,
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            base_url: "https://graph.microsoft.com/v1.0".into(),
            max_retries: 8,
            concurrency: 4,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(120),
            allow_http: false,
        }
    }
}

/// One page of a Graph collection.
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: DeserializeOwned"))]
pub struct Page<T> {
    #[serde(default)]
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Clone)]
enum Body {
    None,
    Json(Value),
    Bytes(Bytes, &'static str),
}

/// Whether a request may be repeated after an ambiguous failure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    /// Reads and conditional writes: retry on 429, 502, 503, 504 and transport errors.
    Safe,
    /// Creates: retry only when Graph explicitly rejected the request (429, 503).
    OnlyIfRejected,
}

#[derive(Clone)]
pub struct GraphClient {
    http: reqwest::Client,
    /// Client for pre-authenticated URLs. Never carries a bearer token.
    anon: reqwest::Client,
    auth: Arc<AppAuth>,
    base: Url,
    settings: GraphSettings,
    sem: Arc<Semaphore>,
}

impl std::fmt::Debug for GraphClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphClient")
            .field("base", &self.base.as_str())
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl GraphClient {
    pub fn new(settings: GraphSettings, auth: AppAuth) -> Result<Self, GraphError> {
        let base = Url::parse(settings.base_url.trim_end_matches('/'))
            .map_err(|e| GraphError::Refused(format!("invalid Graph base URL: {e}")))?;
        if base.scheme() != "https" && !settings.allow_http {
            return Err(GraphError::Refused("Graph base URL must use https".into()));
        }
        let ua = concat!("banksia/", env!("CARGO_PKG_VERSION"));
        let build = |redirect| {
            reqwest::Client::builder()
                .user_agent(ua)
                .https_only(!settings.allow_http)
                .redirect(redirect)
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(600))
                .build()
                .map_err(|e| GraphError::Transport(e.to_string()))
        };
        Ok(Self {
            // Redirects are followed by hand so the token never leaves the Graph origin.
            http: build(reqwest::redirect::Policy::none())?,
            anon: build(reqwest::redirect::Policy::limited(5))?,
            auth: Arc::new(auth),
            sem: Arc::new(Semaphore::new(settings.concurrency.max(1))),
            base,
            settings,
        })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// Resolve a relative path (`/users/x`) or an absolute Graph link
    /// (`@odata.nextLink`). Absolute links must share the Graph origin.
    pub fn resolve(&self, path_or_url: &str) -> Result<Url, GraphError> {
        if path_or_url.starts_with("https://") || path_or_url.starts_with("http://") {
            let u = Url::parse(path_or_url)
                .map_err(|e| GraphError::Refused(format!("invalid link: {e}")))?;
            if !same_origin(&u, &self.base) {
                return Err(GraphError::Refused(format!(
                    "link points outside the Graph origin ({})",
                    u.host_str().unwrap_or("?")
                )));
            }
            Ok(u)
        } else {
            let p = if path_or_url.starts_with('/') {
                path_or_url.to_string()
            } else {
                format!("/{path_or_url}")
            };
            Url::parse(&format!(
                "{}{}",
                self.base.as_str().trim_end_matches('/'),
                p
            ))
            .map_err(|e| GraphError::Refused(format!("invalid path: {e}")))
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let exp = self
            .settings
            .base_backoff
            .saturating_mul(2u32.saturating_pow(attempt.min(16)));
        let mut j = [0u8; 2];
        let _ = getrandom::fill(&mut j);
        let jitter = self
            .settings
            .base_backoff
            .mul_f64(f64::from(u16::from_le_bytes(j)) / 65535.0);
        (exp + jitter).min(self.settings.max_backoff)
    }

    fn retry_after(&self, resp: &reqwest::Response) -> Option<Duration> {
        let v = resp.headers().get(header::RETRY_AFTER)?.to_str().ok()?;
        let secs: u64 = v.trim().parse().ok()?;
        Some(Duration::from_secs(secs).min(self.settings.max_backoff))
    }

    async fn execute(
        &self,
        method: Method,
        url: Url,
        body: Body,
        extra_headers: &[(&'static str, String)],
        repeat: Repeat,
    ) -> Result<reqwest::Response, GraphError> {
        let _permit = self.sem.acquire().await.expect("semaphore is never closed");
        let mut attempt = 0u32;
        let mut refreshed = false;
        loop {
            let token = self.auth.token(&self.http).await?;
            let mut rb = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(token.as_str());
            for (k, v) in extra_headers {
                rb = rb.header(*k, v);
            }
            rb = match &body {
                Body::None => rb,
                Body::Json(v) => rb.json(v),
                Body::Bytes(b, ct) => rb.header(header::CONTENT_TYPE, *ct).body(b.clone()),
            };
            match rb.send().await {
                Ok(resp) => {
                    let st = resp.status();
                    if st == StatusCode::UNAUTHORIZED && !refreshed {
                        self.auth.invalidate().await;
                        refreshed = true;
                        continue;
                    }
                    let retryable = match repeat {
                        Repeat::Safe => matches!(st.as_u16(), 429 | 502 | 503 | 504),
                        Repeat::OnlyIfRejected => matches!(st.as_u16(), 429 | 503),
                    };
                    if retryable && attempt < self.settings.max_retries {
                        let wait = self
                            .retry_after(&resp)
                            .unwrap_or_else(|| self.backoff(attempt));
                        attempt += 1;
                        tracing::warn!(
                            status = st.as_u16(),
                            attempt,
                            wait_ms = wait.as_millis() as u64,
                            "Graph throttled or unavailable, retrying"
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    let ambiguous = !(e.is_connect() || e.is_builder());
                    let may_retry = repeat == Repeat::Safe || !ambiguous;
                    if may_retry && attempt < self.settings.max_retries {
                        let wait = self.backoff(attempt);
                        attempt += 1;
                        tracing::warn!(
                            attempt,
                            wait_ms = wait.as_millis() as u64,
                            "Graph transport error, retrying"
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(GraphError::Transport(e.without_url().to_string()));
                }
            }
        }
    }

    async fn checked(
        &self,
        method: Method,
        path: &str,
        body: Body,
        headers: &[(&'static str, String)],
        repeat: Repeat,
    ) -> Result<reqwest::Response, GraphError> {
        let url = self.resolve(path)?;
        let resp = self.execute(method, url, body, headers, repeat).await?;
        error_for_status(resp).await
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, GraphError> {
        let resp = self
            .checked(Method::GET, path, Body::None, &[], Repeat::Safe)
            .await?;
        decode(resp).await
    }

    /// GET with extra request headers (for example `Prefer`).
    pub async fn get_json_with<T: DeserializeOwned>(
        &self,
        path: &str,
        headers: &[(&'static str, String)],
    ) -> Result<T, GraphError> {
        let resp = self
            .checked(Method::GET, path, Body::None, headers, Repeat::Safe)
            .await?;
        decode(resp).await
    }

    /// Fetch every item in a collection, following `@odata.nextLink`.
    pub async fn get_all<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, GraphError> {
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        while let Some(link) = next {
            let page: Page<T> = self.get_json(&link).await?;
            out.extend(page.value);
            next = page.next_link;
        }
        Ok(out)
    }

    /// Run a delta query to completion. Returns every change and the new delta link.
    pub async fn delta_all<T: DeserializeOwned>(
        &self,
        start: &str,
        headers: &[(&'static str, String)],
    ) -> Result<(Vec<T>, String), GraphError> {
        let mut out = Vec::new();
        let mut link = start.to_string();
        loop {
            let page: Page<T> = self.get_json_with(&link, headers).await?;
            out.extend(page.value);
            match (page.next_link, page.delta_link) {
                (Some(n), _) => link = n,
                (None, Some(d)) => return Ok((out, d)),
                (None, None) => {
                    return Err(GraphError::Decode(
                        "delta response had neither nextLink nor deltaLink".into(),
                    ));
                }
            }
        }
    }

    /// POST that creates something. Not repeated after ambiguous failures.
    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, GraphError> {
        let resp = self
            .checked(
                Method::POST,
                path,
                Body::Json(body.clone()),
                &[],
                Repeat::OnlyIfRejected,
            )
            .await?;
        decode(resp).await
    }

    /// POST raw bytes (for example base64 MIME with `text/plain`).
    pub async fn post_bytes<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Bytes,
        content_type: &'static str,
    ) -> Result<T, GraphError> {
        let resp = self
            .checked(
                Method::POST,
                path,
                Body::Bytes(body, content_type),
                &[],
                Repeat::OnlyIfRejected,
            )
            .await?;
        decode(resp).await
    }

    pub async fn patch_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, GraphError> {
        let resp = self
            .checked(
                Method::PATCH,
                path,
                Body::Json(body.clone()),
                &[],
                Repeat::Safe,
            )
            .await?;
        decode(resp).await
    }

    /// PUT content. With conflict behaviour `fail` or `replace` a PUT is
    /// idempotent; with `rename` it is a create, so it is not repeated after
    /// ambiguous failures.
    pub async fn put_bytes<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Bytes,
        content_type: &'static str,
        creates: bool,
    ) -> Result<T, GraphError> {
        let repeat = if creates {
            Repeat::OnlyIfRejected
        } else {
            Repeat::Safe
        };
        let resp = self
            .checked(
                Method::PUT,
                path,
                Body::Bytes(body, content_type),
                &[],
                repeat,
            )
            .await?;
        decode(resp).await
    }

    /// Start a streaming download. Follows a redirect to a pre-authenticated
    /// URL without the bearer token.
    pub async fn download(&self, path: &str) -> Result<Download, GraphError> {
        let url = self.resolve(path)?;
        let resp = self
            .execute(Method::GET, url.clone(), Body::None, &[], Repeat::Safe)
            .await?;
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| GraphError::Decode("redirect without Location".into()))?;
            let target = url
                .join(loc)
                .map_err(|e| GraphError::Decode(format!("invalid redirect: {e}")))?;
            let resp = self.anon_request(Method::GET, target, None).await?;
            return Ok(Download {
                resp: error_for_status(resp).await?,
            });
        }
        Ok(Download {
            resp: error_for_status(resp).await?,
        })
    }

    /// Request against a pre-authenticated URL (download or upload session).
    /// No bearer token is ever attached.
    async fn anon_request(
        &self,
        method: Method,
        url: Url,
        body: Option<(Bytes, String)>,
    ) -> Result<reqwest::Response, GraphError> {
        if url.scheme() != "https" && !self.settings.allow_http {
            return Err(GraphError::Refused(
                "pre-authenticated URL must use https".into(),
            ));
        }
        let _permit = self.sem.acquire().await.expect("semaphore is never closed");
        let mut attempt = 0u32;
        loop {
            let mut rb = self.anon.request(method.clone(), url.clone());
            if let Some((b, range)) = &body {
                rb = rb
                    .header(header::CONTENT_RANGE, range)
                    .header(header::CONTENT_LENGTH, b.len())
                    .body(b.clone());
            }
            match rb.send().await {
                Ok(resp) => {
                    let st = resp.status().as_u16();
                    if matches!(st, 429 | 500 | 502 | 503 | 504)
                        && attempt < self.settings.max_retries
                    {
                        let wait = self
                            .retry_after(&resp)
                            .unwrap_or_else(|| self.backoff(attempt));
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) if attempt < self.settings.max_retries => {
                    let _ = e;
                    let wait = self.backoff(attempt);
                    attempt += 1;
                    tokio::time::sleep(wait).await;
                }
                Err(e) => return Err(GraphError::Transport(e.without_url().to_string())),
            }
        }
    }

    /// Upload one fragment to an upload session. Fragment uploads are
    /// idempotent for the same byte range. Returns the drive item JSON when
    /// the final fragment completes the upload.
    pub async fn upload_fragment(
        &self,
        upload_url: &str,
        data: Bytes,
        start: u64,
        total: u64,
    ) -> Result<Option<Value>, GraphError> {
        let url = Url::parse(upload_url)
            .map_err(|e| GraphError::Decode(format!("invalid upload URL: {e}")))?;
        let end = start + data.len() as u64 - 1;
        let range = format!("bytes {start}-{end}/{total}");
        let resp = self
            .anon_request(Method::PUT, url, Some((data, range)))
            .await?;
        let resp = error_for_status(resp).await?;
        let st = resp.status();
        if st == StatusCode::OK || st == StatusCode::CREATED {
            return Ok(Some(decode(resp).await?));
        }
        Ok(None)
    }
}

/// A streaming response body.
pub struct Download {
    resp: reqwest::Response,
}

impl Download {
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, GraphError> {
        self.resp
            .chunk()
            .await
            .map_err(|e| GraphError::Transport(e.without_url().to_string()))
    }

    pub async fn bytes(self) -> Result<Bytes, GraphError> {
        self.resp
            .bytes()
            .await
            .map_err(|e| GraphError::Transport(e.without_url().to_string()))
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: Option<String>,
    message: Option<String>,
}

async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response, GraphError> {
    let st = resp.status();
    if st.is_success() || st.is_redirection() {
        return Ok(resp);
    }
    let body = resp.bytes().await.unwrap_or_default();
    let (code, message) = serde_json::from_slice::<ErrorEnvelope>(&body)
        .ok()
        .and_then(|e| e.error)
        .map(|e| (e.code.unwrap_or_default(), e.message.unwrap_or_default()))
        .unwrap_or_default();
    Err(GraphError::Api {
        status: st.as_u16(),
        code,
        message: message.chars().take(500).collect(),
    })
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, GraphError> {
    let body = resp
        .bytes()
        .await
        .map_err(|e| GraphError::Transport(e.without_url().to_string()))?;
    let body: &[u8] = if body.is_empty() { b"null" } else { &body };
    serde_json::from_slice(body).map_err(|e| GraphError::Decode(e.to_string()))
}
