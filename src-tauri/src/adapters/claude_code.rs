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

/// BLOCKING: stats, opens, reads and parses. `transcript_path` arrives
/// verbatim in a hook payload, so callers must keep this off the async
/// workers (see `ingest::transcript_summary`).
pub fn last_assistant_message(transcript_path: &Path) -> Option<String> {
    // Stat BEFORE opening: a regular file is the only thing safe to open
    // here. Opening a FIFO blocks in `open()` itself until a writer appears —
    // no timeout can reach it — and a character device like `/dev/zero`
    // reports length 0, which would send the tail logic to offset 0 and let
    // `read_to_end` allocate until the process died (AC-1.3: bad input is
    // dropped, never fatal). Same guard, and same reason, as the shim's
    // `last_turn_context` (ARCHITECTURE.md §5.2.3).
    let meta = std::fs::metadata(transcript_path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    let mut file = std::fs::File::open(transcript_path).ok()?;
    let truncated = len > TAIL_BYTES;
    if truncated {
        file.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
    }
    let mut raw = String::new();
    // Lossy-tolerant read: a seek can land mid-UTF-8; drop bytes up to the
    // first newline when we started mid-file (partial JSONL line anyway).
    // `take` re-imposes the bound on the READ rather than trusting the length
    // we just stated: a transcript being appended to between the stat and the
    // read is the normal case here, and the stat is only ever a hint.
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    raw.push_str(&String::from_utf8_lossy(&bytes));
    let raw = if truncated {
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

    /// A path that is not a regular file must be REFUSED, not opened. The
    /// hazardous one is a FIFO: `File::open` on it blocks inside `open()`
    /// until a writer appears, which would park whichever thread called us
    /// forever. Run on a worker thread with a deadline so a regression fails
    /// the suite instead of hanging it.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_ever_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("transcript.jsonl");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must be available on unix");
        assert!(status.success(), "mkfifo failed");

        let (tx, rx) = std::sync::mpsc::channel();
        let probe = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(last_assistant_message(&probe));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => assert!(result.is_none(), "a FIFO must never yield a summary"),
            Err(_) => panic!("last_assistant_message blocked on a FIFO — it opened it"),
        }
        // Also true for a symlink pointing at one: `metadata` follows links.
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(last_assistant_message(&link));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => assert!(result.is_none()),
            Err(_) => panic!("last_assistant_message blocked on a symlink to a FIFO"),
        }
    }

    /// The other non-regular shapes. `/dev/zero` is the one that used to be
    /// fatal: it stats as length 0, so the bounded tail degraded to "read the
    /// whole thing" and never stopped.
    ///
    /// Unlike the FIFO above this cannot be given a deadline — an unbounded
    /// read is not slow, it is an allocation loop. What keeps the assertion
    /// safe to run is the `take(TAIL_BYTES)` guard: the `is_file` check has
    /// to fail AND the read bound has to be gone before this can hurt the
    /// suite. Both would have to be removed together.
    #[test]
    fn other_non_regular_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(last_assistant_message(dir.path()).is_none(), "a directory");
        #[cfg(unix)]
        {
            assert!(
                last_assistant_message(Path::new("/dev/zero")).is_none(),
                "an infinite character device must never be read"
            );
            assert!(last_assistant_message(Path::new("/dev/null")).is_none());
        }
    }

}
