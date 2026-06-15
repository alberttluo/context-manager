use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

pub fn effective_context_tokens(u: &Usage) -> u64 {
    u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens
}

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Returns (model, usage) when the line is an assistant turn carrying usage.
pub fn parse_usage_from_line(line: &str) -> Option<(Option<String>, Usage)> {
    let parsed: Line = serde_json::from_str(line).ok()?;
    if parsed.r#type != "assistant" {
        return None;
    }
    let msg = parsed.message?;
    let usage = msg.usage?;
    Some((msg.model, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_tokens_sums_input_and_cache() {
        let u = Usage {
            input_tokens: 6,
            cache_creation_input_tokens: 4101,
            cache_read_input_tokens: 300_534,
            output_tokens: 126,
        };
        assert_eq!(effective_context_tokens(&u), 304_641);
    }

    #[test]
    fn parses_assistant_line_with_usage() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":6,"cache_creation_input_tokens":4101,"cache_read_input_tokens":300534,"output_tokens":126}}}"#;
        let parsed = parse_usage_from_line(line).unwrap();
        assert_eq!(parsed.0.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(effective_context_tokens(&parsed.1), 304_641);
    }

    #[test]
    fn ignores_non_assistant_lines() {
        assert!(parse_usage_from_line(r#"{"type":"user","message":{}}"#).is_none());
        assert!(parse_usage_from_line(r#"{"type":"file-history-snapshot"}"#).is_none());
        assert!(parse_usage_from_line("not json").is_none());
    }

    #[test]
    fn missing_usage_fields_default_to_zero() {
        let line = r#"{"type":"assistant","message":{"model":"m","usage":{"output_tokens":5}}}"#;
        let (_model, u) = parse_usage_from_line(line).unwrap();
        assert_eq!(effective_context_tokens(&u), 0);
    }
}
