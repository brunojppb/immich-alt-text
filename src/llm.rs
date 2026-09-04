//! OpenAI-compatible chat completions client. One vision call per photo.

use std::time::Duration;

use base64::Engine as _;
use reqwest::StatusCode;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Network or server trouble that may pass. Worth a retry.
    #[error("llm: {0}")]
    Transient(String),
    /// Wrong for this one photo. Skip it.
    #[error("llm: {0}")]
    Permanent(String),
    /// Wrong for the whole run: bad key, unknown model, wrong URL.
    #[error("llm: {0}")]
    Fatal(String),
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl LlmClient {
    /// `base_url` ends at the API root, for example `http://localhost:1234/v1`.
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| LlmError::Fatal(format!("http client: {error}")))?;

        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_tokens,
        })
    }

    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            req
        } else {
            req.bearer_auth(&self.api_key)
        }
    }

    /// Adds auth, sends, logs method, path, status, and duration, then maps the status.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, LlmError> {
        let req = self
            .authorize(req)
            .build()
            .map_err(|error| LlmError::Permanent(format!("bad request: {error}")))?;
        let method = req.method().clone();
        let path = req.url().path().to_string();
        let started = std::time::Instant::now();
        let resp = self.http.execute(req).await.map_err(transport)?;

        tracing::debug!(
            %method,
            %path,
            status = %resp.status(),
            ms = started.elapsed().as_millis() as u64,
            "llm"
        );

        check_status(resp).await
    }

    /// Lists models. Returns the HTTP status line. Proves URL and key.
    pub async fn ping(&self) -> Result<String, LlmError> {
        let resp = self
            .send(self.http.get(format!("{}/models", self.base_url)))
            .await?;
        Ok(resp.status().to_string())
    }

    /// Asks the model for a description of one JPEG.
    pub async fn describe(&self, jpeg: &[u8], prompt: &str) -> Result<String, LlmError> {
        let data_uri = format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        );
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": data_uri } }
                ]
            }]
        });
        let resp = self
            .send(
                self.http
                    .post(format!("{}/chat/completions", self.base_url))
                    .json(&body),
            )
            .await?;
        let parsed: Completion = resp
            .json()
            .await
            .map_err(|error| LlmError::Permanent(format!("bad response body: {error}")))?;
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            return Err(LlmError::Permanent("model returned no text".into()));
        }

        Ok(text.to_string())
    }
}

#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

fn transport(error: reqwest::Error) -> LlmError {
    LlmError::Transient(error.to_string())
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, LlmError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    let message = format!("HTTP {status}");
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Err(LlmError::Fatal(format!("{message}: check the API key")))
        }
        StatusCode::NOT_FOUND => Err(LlmError::Fatal(format!(
            "{message}: check the base URL and model name"
        ))),
        StatusCode::TOO_MANY_REQUESTS => Err(LlmError::Transient(message)),
        server if server.is_server_error() => Err(LlmError::Transient(message)),
        _ => Err(LlmError::Permanent(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{body_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0];

    fn ok_body(text: &str) -> serde_json::Value {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "m",
            "choices": [ { "index": 0, "finish_reason": "stop",
                "message": { "role": "assistant", "content": text } } ]
        })
    }

    async fn client(server: &MockServer, key: &str) -> LlmClient {
        LlmClient::new(
            &format!("{}/v1", server.uri()),
            key,
            "gemma",
            200,
            Duration::from_secs(5),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn describe_sends_openai_vision_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({
                "model": "gemma",
                "max_tokens": 200,
                "messages": [ { "role": "user", "content": [
                    { "type": "text", "text": "describe it" },
                    {
                        "type": "image_url",
                        "image_url": { "url": "data:image/jpeg;base64,/9j/4A==" }
                    }
                ] } ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("  A red bike.  \n")))
            .expect(1)
            .mount(&server)
            .await;

        let text = client(&server, "sk-test")
            .await
            .describe(JPEG, "describe it")
            .await
            .unwrap();
        assert_eq!(text, "A red bike.");

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let url = body["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
        assert!(url.starts_with("data:image/jpeg;base64,/9j/"), "{url}");
    }

    #[tokio::test]
    async fn empty_key_sends_no_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("x")))
            .mount(&server)
            .await;
        let text = client(&server, "").await.describe(JPEG, "p").await.unwrap();
        assert_eq!(text, "x");
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_tolerated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("ok")))
            .mount(&server)
            .await;
        let c = LlmClient::new(
            &format!("{}/v1/", server.uri()),
            "",
            "m",
            10,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(c.describe(JPEG, "p").await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn empty_content_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("   ")))
            .mount(&server)
            .await;
        let err = client(&server, "")
            .await
            .describe(JPEG, "p")
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn missing_choices_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "choices": [] })))
            .mount(&server)
            .await;
        let err = client(&server, "")
            .await
            .describe(JPEG, "p")
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn unauthorized_and_not_found_are_fatal() {
        for status in [401u16, 403, 404] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let err = client(&server, "")
                .await
                .describe(JPEG, "p")
                .await
                .unwrap_err();
            assert!(matches!(err, LlmError::Fatal(_)), "{status}: {err}");
        }
    }

    #[tokio::test]
    async fn server_errors_and_rate_limits_are_transient() {
        for status in [500u16, 503, 429] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let err = client(&server, "")
                .await
                .describe(JPEG, "p")
                .await
                .unwrap_err();
            assert!(matches!(err, LlmError::Transient(_)), "{status}: {err}");
        }
    }

    #[tokio::test]
    async fn bad_request_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let err = client(&server, "")
            .await
            .describe(JPEG, "p")
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn ping_hits_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(client(&server, "k").await.ping().await.unwrap(), "200 OK");
    }

    #[tokio::test]
    async fn ping_reports_fatal_on_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(&server, "k").await.ping().await.unwrap_err();
        assert!(matches!(err, LlmError::Fatal(_)), "{err}");
    }

    #[tokio::test]
    async fn request_timeout_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .expect(1)
            .mount(&server)
            .await;
        let client = LlmClient::new(
            &format!("{}/v1", server.uri()),
            "k",
            "gemma",
            200,
            Duration::from_millis(20),
        )
        .unwrap();
        let err = client.ping().await.unwrap_err();
        assert!(matches!(err, LlmError::Transient(_)), "{err}");
    }
}
