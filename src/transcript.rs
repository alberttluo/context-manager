use crate::usage::{effective_context_tokens, parse_usage_from_line};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Assistant,
    User,
    Other,
}

#[derive(Debug, Clone)]
pub struct TranscriptState {
    pub context_tokens: u64,
    pub max_context_tokens: u64,
    pub model: Option<String>,
    pub last_entry: EntryKind,
}

#[derive(Deserialize)]
struct TypeOnly {
    #[serde(default)]
    r#type: String,
}

/// Read a transcript and report the latest assistant context size, its model,
/// and the kind of the last meaningful (assistant/user) entry. Bookkeeping
/// lines (snapshots, mode changes) are ignored for last-entry classification.
///
/// NOTE: re-reads the whole file each call. Fine for v1 (small files, few
/// sessions, multi-second poll). Tracking a byte offset is a future optimization.
pub fn analyze(path: &Path) -> anyhow::Result<TranscriptState> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TranscriptState { context_tokens: 0, max_context_tokens: 0, model: None, last_entry: EntryKind::Other });
        }
        Err(e) => return Err(e.into()),
    };

    let mut context_tokens = 0u64;
    let mut max_context_tokens = 0u64;
    let mut model: Option<String> = None;
    let mut last_entry = EntryKind::Other;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((m, usage)) = parse_usage_from_line(line) {
            context_tokens = effective_context_tokens(&usage);
            if context_tokens > max_context_tokens {
                max_context_tokens = context_tokens;
            }
            model = m;
        }
        if let Ok(t) = serde_json::from_str::<TypeOnly>(line) {
            match t.r#type.as_str() {
                "assistant" => last_entry = EntryKind::Assistant,
                "user" => last_entry = EntryKind::User,
                _ => {}
            }
        }
    }

    Ok(TranscriptState { context_tokens, max_context_tokens, model, last_entry })
}

/// Counts of genuine prompts submitted to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PromptStats {
    /// User entries that are real prompts (tool results excluded).
    pub prompts: usize,
    /// How many of those contain the caller's marker.
    pub with_marker: usize,
}

/// Whitespace-insensitive containment, so a needle still matches text that the
/// terminal or transcript has wrapped or re-indented.
pub fn contains_ignoring_whitespace(haystack: &str, needle: &str) -> bool {
    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    strip(haystack).contains(&strip(needle))
}

/// Count the genuine prompts in a transcript and how many carry `marker`.
///
/// Tool results are recorded as `type: "user"` entries too, so they must be
/// excluded: otherwise every tool call an assistant makes looks like the human
/// typing, and takeover detection fires constantly.
pub fn prompt_stats(path: &Path, marker: &str) -> anyhow::Result<PromptStats> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PromptStats::default()),
        Err(e) => return Err(e.into()),
    };

    let mut stats = PromptStats::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let Some(content) = v.pointer("/message/content") else { continue };
        let prompt_text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => {
                // Any tool_result block means this is machine output, not a prompt.
                if blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result")) {
                    continue;
                }
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            _ => continue,
        };
        stats.prompts += 1;
        if contains_ignoring_whitespace(&prompt_text, marker) {
            stats.with_marker += 1;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn prompt_stats_counts_prompts_and_excludes_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(
            &p,
            concat!(
                // A genuine human prompt.
                r#"{"type":"user","message":{"content":"do the thing"}}"#, "\n",
                // Our injected handoff prompt (string content, carries the marker).
                r#"{"type":"user","message":{"content":"Write a handoff to /h/sess-1.md now"}}"#, "\n",
                // A tool result — must NOT count as a prompt.
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"/h/sess-1.md"}]}}"#, "\n",
                // Assistant turn — irrelevant here.
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#, "\n",
                // A later human prompt with text blocks.
                r#"{"type":"user","message":{"content":[{"type":"text","text":"actually do this instead"}]}}"#, "\n"
            ),
        )
        .unwrap();

        let stats = prompt_stats(&p, "/h/sess-1.md").unwrap();
        assert_eq!(stats.prompts, 3, "tool_result entry must not count as a prompt");
        assert_eq!(stats.with_marker, 1, "only the injected prompt carries the marker");
    }

    #[test]
    fn prompt_stats_matches_marker_across_wrapping() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.jsonl");
        // The path is broken by a newline, as terminal wrapping would.
        std::fs::write(
            &p,
            "{\"type\":\"user\",\"message\":{\"content\":\"write to /h/\\nsess-1.md\"}}\n",
        )
        .unwrap();
        let stats = prompt_stats(&p, "/h/sess-1.md").unwrap();
        assert_eq!(stats.with_marker, 1);
    }

    #[test]
    fn prompt_stats_missing_file_is_empty() {
        assert_eq!(
            prompt_stats(Path::new("/no/such/transcript.jsonl"), "x").unwrap(),
            PromptStats::default()
        );
    }

    #[test]
    fn analyzes_latest_usage_and_last_entry() {
        let state = analyze(Path::new("tests/fixtures/sample.jsonl")).unwrap();
        // latest assistant usage: 6 + 2000 + 120000
        assert_eq!(state.context_tokens, 122_006);
        // peak: first turn was 10 + 1000 + 50000 = 51010, last is 122006 — peak is 122006
        assert_eq!(state.max_context_tokens, 122_006);
        assert_eq!(state.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(state.last_entry, EntryKind::Assistant);
    }

    #[test]
    fn empty_file_is_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.jsonl");
        std::fs::write(&p, "").unwrap();
        let state = analyze(&p).unwrap();
        assert_eq!(state.context_tokens, 0);
        assert_eq!(state.max_context_tokens, 0);
        assert_eq!(state.last_entry, EntryKind::Other);
        assert!(state.model.is_none());
    }

    #[test]
    fn last_entry_user_when_user_turn_is_last() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "{\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":1}}}\n{\"type\":\"user\",\"message\":{}}\n").unwrap();
        let state = analyze(&p).unwrap();
        assert_eq!(state.last_entry, EntryKind::User);
        assert_eq!(state.context_tokens, 1);
        assert_eq!(state.max_context_tokens, 1);
    }

    #[test]
    fn tracks_peak_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("peak.jsonl");
        // First turn: cache_read 300000 -> effective 300000; second turn: cache_read 50000 -> effective 50000.
        std::fs::write(
            &p,
            "{\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"usage\":{\"cache_read_input_tokens\":300000}}}\n\
             {\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"usage\":{\"cache_read_input_tokens\":50000}}}\n",
        )
        .unwrap();
        let state = analyze(&p).unwrap();
        assert_eq!(state.context_tokens, 50_000);
        assert_eq!(state.max_context_tokens, 300_000);
    }
}
