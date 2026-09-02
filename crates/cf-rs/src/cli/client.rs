//! Thin HTTP client for the admin API (`contracts/admin-api.md`), shared by
//! the `fn` subcommands.

use std::time::Duration;

use serde_json::Value;

pub const EXIT_ADMIN_UNREACHABLE: u8 = 3;
pub const EXIT_AUTH_FAILED: u8 = 4;

pub struct AdminClient {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not reach admin API at {url}: {source}")]
    Unreachable {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("authentication failed (401) against the admin API")]
    Unauthorized,
    #[error("admin API returned {status}: {body}")]
    ApiError { status: u16, body: String },
}

impl ClientError {
    /// Exit code per `admin-api.md`'s CLI section: 3 = admin API unreachable,
    /// 4 = authentication failed. Other API errors are surfaced as exit 1
    /// (a failed operation) by the caller, not by this mapping.
    pub fn suggested_exit_code(&self) -> u8 {
        match self {
            ClientError::Unreachable { .. } => EXIT_ADMIN_UNREACHABLE,
            ClientError::Unauthorized => EXIT_AUTH_FAILED,
            ClientError::ApiError { .. } => 1,
        }
    }
}

impl AdminClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            base_url,
            token,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<Value, ClientError> {
        let url_for_err = self.base_url.clone();
        let resp = self
            .authorize(builder)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| ClientError::Unreachable {
                url: url_for_err,
                source,
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(ClientError::ApiError {
                status: status.as_u16(),
                body: body.to_string(),
            });
        }
        Ok(body)
    }

    pub async fn deploy(&self, name: &str, request_body: Value) -> Result<Value, ClientError> {
        let builder = self.http.put(self.url(&format!("/v1/functions/{name}")));
        self.send(builder.json(&request_body)).await
    }

    pub async fn describe(&self, name: &str) -> Result<Value, ClientError> {
        let builder = self.http.get(self.url(&format!("/v1/functions/{name}")));
        self.send(builder).await
    }

    pub async fn get_build(&self, name: &str, build_id: &str) -> Result<Value, ClientError> {
        let builder = self
            .http
            .get(self.url(&format!("/v1/functions/{name}/builds/{build_id}")));
        self.send(builder).await
    }

    pub async fn get_build_log(&self, name: &str, build_id: &str) -> Result<String, ClientError> {
        let url_for_err = self.base_url.clone();
        let resp = self
            .authorize(
                self.http
                    .get(self.url(&format!("/v1/functions/{name}/builds/{build_id}/log"))),
            )
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| ClientError::Unreachable {
                url: url_for_err,
                source,
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ClientError::ApiError {
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(text)
    }
}
