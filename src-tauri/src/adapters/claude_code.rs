//! Claude Code adapter: transcript fallback for completion summaries.
//!
//! The Stop hook normally carries `last_assistant_message` (verified against
//! claude 2.1.198), so this is only read when that field is absent. §9
//! invariant 4: transcripts are read from disk on demand and never stored.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// Transcripts grow to tens of MB; the last assistant message is at the end,
/// so a bounded tail keeps memory and latency flat (NFR-1/NFR-2).
const TAIL_BYTES: u64 = 256 * 1024;

pub fn last_assistant_message(transcript_path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(transcript_path).ok()?;
    let len = file.metadata().ok()?.len();
    let offset = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut raw = String::new();
    // Lossy-tolerant read: a seek can land mid-UTF-8; drop bytes up to the
    // first newline when we started mid-file (partial JSONL line anyway).
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    raw.push_str(&String::from_utf8_lossy(&bytes));
    let raw = if offset > 0 {
        raw.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        raw.as_str()
    };

    let mut last: Option<String> = None;
    for line in raw.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["type"] == "assistant" {
            let text = extract_text(&entry["message"]["content"]);
            if !text.is_empty() {
                last = Some(text);
            }
        }
    }
    last
}

fn extract_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_last_assistant_text_from_jsonl() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":"do the thing"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Working on it."}}]}}}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash"}},{{"type":"text","text":"Done, all tests pass."}}]}}}}"#).unwrap();
        writeln!(f, "not json at all").unwrap();

        let text = last_assistant_message(f.path()).unwrap();
        assert_eq!(text, "Done, all tests pass.");
    }

    #[test]
    fn missing_file_is_none() {
        assert!(last_assistant_message(Path::new("/nonexistent/t.jsonl")).is_none());
    }

    #[test]
    fn huge_transcript_reads_only_the_tail() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // ~600 KB of filler lines, then the answer within the 256 KB tail.
        let filler = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(200)
        );
        for _ in 0..3000 {
            writeln!(f, "{filler}").unwrap();
        }
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Tail found."}}]}}}}"#
        )
        .unwrap();
        assert_eq!(last_assistant_message(f.path()).unwrap(), "Tail found.");
    }
}
