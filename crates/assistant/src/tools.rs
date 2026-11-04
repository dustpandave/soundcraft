//! The assistant's tools: a subset of the MCP server's, in the Messages API's shape, plus the
//! guard that keeps it away from commands that would replace the user's session.

use serde_json::{Value, json};
use soundcraft_automation::mcp;

/// MCP tools the in-app assistant may call. Session switching and synthetic mouse/keyboard input
/// are left out: the assistant works through commands, and the user's open session stays theirs.
const ALLOWED: &[&str] =
    &["list_commands", "execute", "batch", "inspect_session", "import_audio", "bounce_mix", "save_session", "screenshot", "ui_inspect", "ui_set"];

/// Commands that would close or replace the open session or quit the app.
const BLOCKED_COMMANDS: &[&str] = &["session.new", "session.open", "session.close", "session.revert", "app.quit"];

/// Tool results are capped so one huge inspect can't crowd out the conversation.
const MAX_RESULT_CHARS: usize = 60_000;

/// Tool definitions for the Messages API.
pub fn definitions() -> Vec<Value> {
    mcp::tool_definitions()
        .into_iter()
        .filter(|t| t.get("name").and_then(Value::as_str).is_some_and(|n| ALLOWED.contains(&n)))
        .map(|t| {
            let description = t.get("description").and_then(Value::as_str).unwrap_or("").trim_start_matches("APP_ONLY. ").to_string();
            json!({"name": t["name"], "description": description, "input_schema": t["inputSchema"]})
        })
        .collect()
}

/// Why the assistant may not make this call, if it may not.
pub fn refusal(name: &str, input: &Value) -> Option<String> {
    if !ALLOWED.contains(&name) {
        return Some(format!("`{name}` is not available to the assistant."));
    }
    let blocked = |cmd: Option<&str>| cmd.map(str::trim).is_some_and(|c| BLOCKED_COMMANDS.contains(&c));
    let hit = match name {
        "execute" => blocked(input.get("command").and_then(Value::as_str)),
        "batch" => {
            input.get("calls").and_then(Value::as_array).is_some_and(|calls| calls.iter().any(|c| blocked(c.get("command").and_then(Value::as_str))))
        }
        _ => false,
    };
    hit.then(|| "The assistant can't open, close, replace or revert sessions or quit the app. Ask the user to do that themselves.".to_string())
}

/// A one-line description of a call for the chat log.
pub fn describe(name: &str, input: &Value) -> String {
    let short = |v: &Value| {
        let s = v.to_string();
        if s.chars().count() > 100 { format!("{}…", s.chars().take(100).collect::<String>()) } else { s }
    };
    match name {
        "execute" => {
            let cmd = input.get("command").and_then(Value::as_str).unwrap_or("?");
            match input.get("params") {
                Some(p) if p.as_object().is_some_and(|o| !o.is_empty()) => format!("{cmd} {}", short(p)),
                _ => cmd.to_string(),
            }
        }
        "batch" => {
            let calls = input.get("calls").and_then(Value::as_array);
            let names: Vec<&str> = calls.map(|c| c.iter().filter_map(|c| c.get("command").and_then(Value::as_str)).collect()).unwrap_or_default();
            let n = names.len();
            let first: Vec<&str> = names.into_iter().take(4).collect();
            format!("{n} commands: {}{}", first.join(", "), if n > 4 { ", …" } else { "" })
        }
        "list_commands" => match input.get("filter").and_then(Value::as_str) {
            Some(f) => format!("looking up commands matching “{f}”"),
            None => "listing commands".to_string(),
        },
        "inspect_session" => "reading the session".to_string(),
        "screenshot" => "taking a screenshot".to_string(),
        other => other.replace('_', " "),
    }
}

/// Turn an MCP tool result (`{content: [...], isError}`) into a Messages API `tool_result` block.
pub fn result_block(tool_use_id: &str, mcp_result: &Value) -> (Value, bool) {
    let is_error = mcp_result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let blocks: Vec<Value> = mcp_result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                    Some("text") => Some(json!({"type": "text", "text": cap(item.get("text").and_then(Value::as_str).unwrap_or(""))})),
                    Some("image") => Some(json!({"type": "image", "source": {
                        "type": "base64",
                        "media_type": item.get("mimeType").and_then(Value::as_str).unwrap_or("image/png"),
                        "data": item.get("data").and_then(Value::as_str).unwrap_or(""),
                    }})),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let content = if blocks.is_empty() { vec![json!({"type": "text", "text": "(no output)"})] } else { blocks };
    (json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}), !is_error)
}

/// An error `tool_result` that never reached the app.
pub fn error_block(tool_use_id: &str, message: &str) -> Value {
    json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": [{"type": "text", "text": message}], "is_error": true})
}

fn cap(s: &str) -> String {
    if s.len() <= MAX_RESULT_CHARS {
        return s.to_string();
    }
    let mut end = MAX_RESULT_CHARS;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… [truncated: {} more bytes. Ask for less, e.g. inspect_session with detail \"summary\" or a list_commands filter.]",
        &s[..end],
        s.len() - end
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowed_tools_are_offered_in_api_shape() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(names.contains(&"execute") && names.contains(&"inspect_session"));
        assert!(!names.contains(&"new_session") && !names.contains(&"click"));
        for d in &defs {
            assert!(d["input_schema"].is_object(), "{d}");
            assert!(!d["description"].as_str().unwrap_or("").starts_with("APP_ONLY"));
        }
    }

    #[test]
    fn session_replacing_commands_are_refused() {
        assert!(refusal("execute", &json!({"command": "session.open"})).is_some());
        assert!(refusal("execute", &json!({"command": " app.quit "})).is_some());
        assert!(refusal("batch", &json!({"calls": [{"command": "track.new"}, {"command": "session.new"}]})).is_some());
        assert!(refusal("new_session", &json!({})).is_some());
        assert!(refusal("execute", &json!({"command": "mix.volume"})).is_none());
    }

    #[test]
    fn results_convert_and_cap() {
        let (b, ok) = result_block("t1", &json!({"content": [{"type": "text", "text": "x".repeat(MAX_RESULT_CHARS + 10)}], "isError": false}));
        assert!(ok);
        assert_eq!(b["tool_use_id"], "t1");
        assert!(b["content"][0]["text"].as_str().unwrap_or("").contains("truncated: 10 more bytes"));
        let (b, ok) = result_block("t2", &json!({"content": [{"type": "image", "data": "AAA", "mimeType": "image/png"}], "isError": true}));
        assert!(!ok);
        assert_eq!(b["content"][0]["source"]["data"], "AAA");
        assert_eq!(b["is_error"], true);
    }

    #[test]
    fn descriptions_are_short() {
        assert_eq!(describe("execute", &json!({"command": "track.new", "params": {}})), "track.new");
        assert!(describe("execute", &json!({"command": "x", "params": {"a": "y".repeat(500)}})).ends_with('…'));
        assert_eq!(describe("batch", &json!({"calls": [{"command": "a"}, {"command": "b"}]})), "2 commands: a, b");
    }
}
