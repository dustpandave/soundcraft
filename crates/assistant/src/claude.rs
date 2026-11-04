//! A minimal Claude Messages API client over HTTPS. Rust has no official Anthropic SDK, so this
//! posts the JSON request itself and retries the failures that are worth retrying.

use serde_json::{Value, json};
use std::time::Duration;

const URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2026-10-21";
/// `fallbacks: "default"`: when a model's safeguards decline a request, the API re-runs it on a
/// suitable fallback model inside the same call.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Room for a reply plus its thinking; requests are not streamed, so this stays moderate.
const MAX_TOKENS: u32 = 16_000;
const MAX_RETRIES: u32 = 3;
/// Statuses that mean "try again shortly": rate limited, overloaded or a passing server error.
const RETRYABLE: &[u16] = &[408, 429, 500, 502, 503, 504, 529];

/// Something that answers a Messages API request. The real one is [`Client`]; tests script it.
pub trait Llm: Send {
    fn send(&self, body: &Value) -> Result<Value, String>;
}

pub struct Client {
    agent: ureq::Agent,
    key: String,
}

impl Client {
    pub fn new(key: String) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .user_agent(concat!("SoundCraft/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Client { agent, key }
    }

    /// One POST: the status and the body text, or a transport error.
    fn post(&self, body: &str, beta: bool) -> Result<(u16, String), String> {
        let mut req =
            self.agent.post(URL).header("x-api-key", &self.key).header("anthropic-version", API_VERSION).header("content-type", "application/json");
        if beta {
            req = req.header("anthropic-beta", FALLBACK_BETA);
        }
        let mut resp = req.send(body).map_err(|e| format!("could not reach the Claude API: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| format!("could not read the Claude API reply: {e}"))?;
        Ok((status, text))
    }
}

impl Llm for Client {
    fn send(&self, body: &Value) -> Result<Value, String> {
        let beta = body.get("fallbacks").is_some();
        let text = body.to_string();
        let mut delay = Duration::from_secs(2);
        let mut attempt = 0;
        loop {
            let outcome = self.post(&text, beta);
            let retry = match &outcome {
                Ok((status, _)) => RETRYABLE.contains(status),
                Err(_) => true,
            };
            if retry && attempt < MAX_RETRIES {
                attempt += 1;
                log::warn!("assistant: Claude API request failed, retry {attempt} in {delay:?}");
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
                continue;
            }
            let (status, body) = outcome?;
            if status == 200 {
                return serde_json::from_str(&body).map_err(|e| format!("the Claude API sent a reply that isn't JSON: {e}"));
            }
            return Err(api_error(status, &body));
        }
    }
}

/// A readable message for a failed request.
fn api_error(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect());
    match status {
        401 => "Your Anthropic API key was rejected. Check it in the Assistant window's settings.".to_string(),
        403 => format!("This API key isn't allowed to make that request: {detail}"),
        429 => format!("The Claude API is rate limiting this key; wait a moment and try again. ({detail})"),
        529 => "The Claude API is overloaded right now; try again in a minute.".to_string(),
        _ => format!("Claude API error {status}: {detail}"),
    }
}

/// The request body for one step of the conversation. `tools` and `system` never change during
/// a session, and the history only grows, so the whole prefix stays cacheable.
pub fn request(model: &str, system: &str, tools: &[Value], messages: &[Value]) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": [{"type": "text", "text": system}],
        "tools": tools,
        "messages": messages,
        // Medium effort keeps an interactive assistant responsive; thinking is adaptive by default.
        "output_config": {"effort": "medium"},
        // Cache the stable prefix (tools, system, earlier turns) between steps.
        "cache_control": {"type": "ephemeral"},
    });
    // Haiku has no server-side fallback; the others route declined requests to a fallback model.
    if !model.starts_with("claude-haiku")
        && let Some(o) = body.as_object_mut()
    {
        o.insert("fallbacks".into(), json!("default"));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_readable() {
        assert!(api_error(401, "{}").contains("API key was rejected"));
        let e = api_error(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad thing"}}"#);
        assert_eq!(e, "Claude API error 400: bad thing");
        assert!(api_error(502, "<html>gateway</html>").contains("<html>gateway"));
    }

    #[test]
    fn haiku_gets_no_fallbacks() {
        assert_eq!(request("claude-opus-5-5", "s", &[], &[])["fallbacks"], "default");
        assert!(request("claude-haiku-5-5", "s", &[], &[]).get("fallbacks").is_none());
    }
}
