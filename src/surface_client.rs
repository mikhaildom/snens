use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,     // No timeout: SSE connections are long-lived.
    commands: reqwest::Client, // Bounded request duration for all non-stream API calls.
    base: String,
    token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    pub surface: String,
    pub state: String,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub session_id: String,
    pub run_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub fields: Value,
}

#[derive(Debug, Deserialize)]
pub struct History {
    #[serde(default)]
    pub events: Vec<Event>,
    pub cursor: u64,
    #[serde(default)]
    pub messages: Option<Vec<TranscriptMessage>>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // Protocol metadata is decoded even when the terminal renderer does not use it.
pub struct TranscriptMessage {
    pub seq: u64,
    pub ts: u64,
    pub role: String,
    pub content: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub exit: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct Sessions {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
pub struct Accepted {
    pub message_id: String,
    pub run_id: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: ErrorBody,
}
#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

impl Client {
    pub fn new(endpoint: String, token: String) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| e.to_string())?;
        let commands = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            commands,
            base: endpoint.trim_end_matches('/').to_owned(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, String> {
        let mut req = self
            .commands
            .request(method, self.url(path))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.map_err(|e| format!("HTTP: {e}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("HTTP body: {e}"))?;
        if !status.is_success() {
            if let Ok(err) = serde_json::from_slice::<ApiError>(&bytes) {
                return Err(format!("{}: {}", err.error.code, err.error.message));
            }
            return Err(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid API response: {e}"))
    }

    pub async fn sessions(&self) -> Result<Vec<Session>, String> {
        Ok(self
            .json::<Sessions>(reqwest::Method::GET, "/v1/sessions", None)
            .await?
            .sessions)
    }

    pub async fn create(&self, command_id: &str) -> Result<Session, String> {
        self.post_retry(
            "/v1/sessions",
            serde_json::json!({"command_id":command_id,"surface":"tui"}),
        )
        .await
    }

    pub async fn get_session(&self, id: &str) -> Result<Session, String> {
        self.json(
            reqwest::Method::GET,
            &format!("/v1/sessions/{}", segment(id)),
            None,
        )
        .await
    }

    pub async fn history(&self, id: &str) -> Result<History, String> {
        self.json(
            reqwest::Method::GET,
            &format!("/v1/sessions/{}/history", segment(id)),
            None,
        )
        .await
    }

    pub async fn message(
        &self,
        id: &str,
        command_id: &str,
        text: &str,
    ) -> Result<Accepted, String> {
        self.post_retry(
            &format!("/v1/sessions/{}/messages", segment(id)),
            serde_json::json!({"command_id":command_id,"text":text}),
        )
        .await
    }

    pub async fn stop(&self, id: &str, command_id: &str, run_id: &str) -> Result<Session, String> {
        self.post_retry(
            &format!("/v1/sessions/{}/stop", segment(id)),
            serde_json::json!({"command_id":command_id,"run_id":run_id}),
        )
        .await
    }

    async fn post_retry<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: Value,
    ) -> Result<T, String> {
        // Retry identical body and command_id after transient failures; API idempotency
        // makes a lost response safe to retry without double execution.
        let mut last = String::new();
        for attempt in 0..3 {
            match self
                .json(reqwest::Method::POST, path, Some(body.clone()))
                .await
            {
                Ok(value) => return Ok(value),
                Err(error) if error.starts_with("HTTP:") || error.starts_with("HTTP body:") => {
                    last = error
                }
                Err(error) => return Err(error),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
            }
        }
        Err(format!("{last}; command_id unchanged on retries"))
    }

    /// Consume one SSE connection. A return means reconnect from last delivered ID.
    pub async fn events<F>(&self, id: &str, after: u64, mut event: F) -> Result<(), String>
    where
        F: FnMut(Event, u64),
    {
        let response = self
            .http
            .get(self.url(&format!(
                "/v1/sessions/{}/events?after={after}",
                segment(id)
            )))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("SSE connect: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("SSE HTTP {status}: {body}"));
        }
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::<u8>::new();
        let mut first_line = true;
        let mut data = Vec::<String>::new();
        let mut event_id: Option<u64> = None;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("SSE read: {e}"))?;
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = buffer.drain(..=pos).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let line =
                    std::str::from_utf8(&line).map_err(|e| format!("SSE invalid UTF-8: {e}"))?;
                let line = if first_line {
                    first_line = false;
                    line.strip_prefix('\u{feff}').unwrap_or(line)
                } else {
                    line
                };
                if line.is_empty() {
                    if !data.is_empty() {
                        let joined = data.join("\n");
                        let parsed: Event = serde_json::from_str(&joined)
                            .map_err(|e| format!("SSE invalid event: {e}"))?;
                        let seq = event_id.unwrap_or(parsed.seq);
                        event(parsed, seq);
                    }
                    data.clear();
                    event_id = None;
                } else if line.starts_with(':') {
                    continue;
                } else if let Some((field, value)) = line.split_once(':') {
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "data" => data.push(value.to_owned()),
                        "id" => {
                            if let Ok(n) = value.parse::<u64>() {
                                event_id = Some(n);
                            } else {
                                event_id = None; // SSE permits an empty id to reset the cursor.
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Process a final unterminated line/frame too; UTF-8 was only decoded once complete.
        if !buffer.is_empty() {
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
            let line =
                std::str::from_utf8(&buffer).map_err(|e| format!("SSE invalid UTF-8: {e}"))?;
            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
            } else if let Some(value) = line.strip_prefix("id:") {
                event_id = value.trim().parse().ok();
            }
        }
        if !data.is_empty() {
            let parsed: Event = serde_json::from_str(&data.join("\n"))
                .map_err(|e| format!("SSE invalid event: {e}"))?;
            let seq = event_id.unwrap_or(parsed.seq);
            event(parsed, seq);
        }
        Ok(())
    }
}

fn segment(value: &str) -> String {
    urlencoding_encode(value)
}
fn urlencoding_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_segment_is_encoded() {
        assert_eq!(segment("a/b ?"), "a%2Fb%20%3F");
    }
    #[test]
    fn event_supports_unknown_fields() {
        let event: Event = serde_json::from_str(
            r#"{"seq":3,"session_id":"s","run_id":null,"type":"notice","text":"ok"}"#,
        )
        .unwrap();
        assert_eq!(event.kind, "notice");
        assert_eq!(event.fields["text"], "ok");
    }
}
