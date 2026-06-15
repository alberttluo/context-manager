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
            return Ok(TranscriptState { context_tokens: 0, model: None, last_entry: EntryKind::Other });
        }
        Err(e) => return Err(e.into()),
    };

    let mut context_tokens = 0u64;
    let mut model: Option<String> = None;
    let mut last_entry = EntryKind::Other;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((m, usage)) = parse_usage_from_line(line) {
            context_tokens = effective_context_tokens(&usage);
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

    Ok(TranscriptState { context_tokens, model, last_entry })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn analyzes_latest_usage_and_last_entry() {
        let state = analyze(Path::new("tests/fixtures/sample.jsonl")).unwrap();
        // latest assistant usage: 6 + 2000 + 120000
        assert_eq!(state.context_tokens, 122_006);
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
    }
}
